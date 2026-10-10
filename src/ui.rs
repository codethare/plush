//! The Wayland side: one layer-shell card per notification.
//!
//! Cards are never resized by content they already know — the compositor's
//! `configure` is the only place a card is drawn, so surface state and buffer
//! state can never disagree.

use std::time::{Duration, Instant};

use calloop::{timer::TimeoutAction, LoopHandle, RegistrationToken};
use cosmic_text::{Buffer, FontSystem, SwashCache};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::GlobalList,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use crate::{
    anim::Anim,
    config::{Column, Config, Position, Row},
    dbus::{CloseRequest, Notification, Shared, REASON_DISMISSED, REASON_EXPIRED},
    render,
};

/// Distance from the screen edge, in logical pixels.
const MARGIN: i32 = 16;
/// Space between stacked cards.
const GAP: i32 = 10;
/// One animation frame. 60 Hz, which is what the motion was tuned against.
const FRAME: Duration = Duration::from_millis(16);

/// Where the stack is anchored. The centred columns leave both sides free, which is
/// what makes the compositor centre the surface horizontally.
fn anchor(position: Position) -> Anchor {
    let vertical = match position.row() {
        Row::Top => Anchor::TOP,
        Row::Bottom => Anchor::BOTTOM,
    };
    vertical
        | match position.column() {
            Column::Left => Anchor::LEFT,
            Column::Centre => Anchor::empty(),
            Column::Right => Anchor::RIGHT,
        }
}

/// Margins for a card `offset` pixels into the stack, in the order `set_margin` wants
/// them: top, right, bottom, left.
///
/// Only anchored edges take a margin, so a centred column has none on either side. The
/// stack is measured from the anchored edge, which is why a bottom position stacks
/// upwards from the bottom of the screen.
fn margins(position: Position, offset: i32) -> (i32, i32, i32, i32) {
    let (left, right) = match position.column() {
        Column::Left => (MARGIN, 0),
        Column::Centre => (0, 0),
        Column::Right => (0, MARGIN),
    };
    let (top, bottom) = match position.row() {
        Row::Top => (MARGIN + offset, 0),
        Row::Bottom => (0, MARGIN + offset),
    };
    (top, right, bottom, left)
}

struct Card {
    id: u32,
    /// The text this card was built from, so a `replaces_id` update is detected.
    source: (String, String, String),
    surface: LayerSurface,
    content: Buffer,
    height: u32,
    /// The size the compositor last configured. Animation redraws need it, which is
    /// why a card cannot be painted before its first `configure`.
    size: Option<(u32, u32)>,
    deadline: Option<Instant>,
    anim: Anim,
}

impl Card {
    /// An exit that has run its course; the surface is dropped once this is true.
    fn leaving_done(&self, now: Instant) -> bool {
        self.anim.leaving() && self.anim.finished(now)
    }
}

fn source_of(notification: &Notification) -> (String, String, String) {
    (
        notification.app_name.clone(),
        notification.summary.clone(),
        notification.body.clone(),
    )
}

pub struct Ui {
    qh: QueueHandle<Ui>,
    handle: LoopHandle<'static, Ui>,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    config: Config,
    font_system: FontSystem,
    swash: SwashCache,
    store: Shared,
    closed: async_channel::Sender<CloseRequest>,
    cards: Vec<Card>,
    pointer: Option<wl_pointer::WlPointer>,
    expiry: Option<RegistrationToken>,
    /// The frame timer, alive only while something is moving.
    frame: Option<RegistrationToken>,
}

impl Ui {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        qh: QueueHandle<Ui>,
        handle: LoopHandle<'static, Ui>,
        globals: &GlobalList,
        compositor: CompositorState,
        layer_shell: LayerShell,
        shm: Shm,
        pool: SlotPool,
        config: Config,
        store: Shared,
        closed: async_channel::Sender<CloseRequest>,
    ) -> Self {
        Self {
            registry_state: RegistryState::new(globals),
            seat_state: SeatState::new(globals, &qh),
            output_state: OutputState::new(globals, &qh),
            qh,
            handle,
            compositor,
            layer_shell,
            shm,
            pool,
            config,
            font_system: FontSystem::new(),
            swash: SwashCache::new(),
            store,
            closed,
            cards: Vec::new(),
            pointer: None,
            expiry: None,
            frame: None,
        }
    }

    /// Brings the on-screen cards in line with the shared queue. The only place
    /// cards are created, destroyed or re-stacked.
    pub fn sync(&mut self) {
        let items: Vec<Notification> = {
            let mut store = self.store.lock().unwrap();
            if store.items.len() > render::MAX_CARDS {
                for dropped in store.items.drain(render::MAX_CARDS..) {
                    let _ = self
                        .closed
                        .try_send(CloseRequest { id: dropped.id, reason: REASON_EXPIRED });
                }
            }
            store.items.clone()
        };

        // Keep the cards that still exist, in queue order, building the new ones.
        // A card whose text changed is rebuilt rather than patched: a fresh surface
        // is what makes the compositor hand us a `configure` to draw with.
        let mut ordered = Vec::with_capacity(items.len());
        for notification in &items {
            let unchanged = self
                .cards
                .iter()
                .position(|card| card.id == notification.id && card.source == source_of(notification));
            match unchanged {
                Some(index) => ordered.push(self.cards.remove(index)),
                None => {
                    self.cards.retain(|card| card.id != notification.id);
                    ordered.push(self.make_card(notification));
                }
            }
        }
        // Cards the queue no longer holds are on their way out: with animations on
        // they keep their surface and slide away, otherwise the surface is dropped
        // here and now. Either way they leave the stack, so the rest close up.
        let now = Instant::now();
        for mut card in std::mem::take(&mut self.cards) {
            match card.anim {
                Anim::Leaving(_) if !card.leaving_done(now) => ordered.push(card),
                Anim::Leaving(_) => {}
                _ if self.config.animation.enabled => {
                    card.anim.leave();
                    ordered.push(card);
                }
                _ => {}
            }
        }
        self.cards = ordered;

        let position = self.config.panel.position;
        let mut offset = 0;
        for card in &mut self.cards {
            // A departing card keeps the place it left from while it fades away.
            if card.anim.leaving() {
                continue;
            }
            let (top, right, bottom, left) = margins(position, offset);
            card.surface.set_margin(top, right, bottom, left);
            card.surface.commit();
            offset += card.height as i32 + GAP;
        }

        self.rearm();
        self.arm_frames();
    }

    fn make_card(&mut self, notification: &Notification) -> Card {
        let mut content = render::build(&mut self.font_system, &self.config, notification);
        let height = render::card_height(&mut self.font_system, &mut content);

        let surface = self.compositor.create_surface(&self.qh);
        let card = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("plush"),
            None,
        );
        card.set_anchor(anchor(self.config.panel.position));
        // -1: never reserve space, so windows may use the full screen under us.
        card.set_exclusive_zone(-1);
        card.set_keyboard_interactivity(KeyboardInteractivity::None);
        card.set_size(render::PANEL_W, height);
        // An initial commit with no buffer; the configure it triggers is what draws.
        card.commit();

        Card {
            id: notification.id,
            source: source_of(notification),
            surface: card,
            content,
            height,
            deadline: notification.timeout.map(|timeout| Instant::now() + timeout),
            anim: if self.config.animation.enabled {
                Anim::entering()
            } else {
                Anim::Settled
            },
            size: None,
        }
    }

    fn remove(&mut self, id: u32, reason: u32) {
        self.store.lock().unwrap().items.retain(|n| n.id != id);
        let _ = self.closed.try_send(CloseRequest { id, reason });
        self.sync();
    }

    fn expire(&mut self) {
        let now = Instant::now();
        let expired: Vec<u32> = self
            .cards
            .iter()
            .filter(|card| card.deadline.is_some_and(|deadline| deadline <= now))
            .map(|card| card.id)
            .collect();
        if expired.is_empty() {
            // The card this timer was set for has already gone — a departing one is
            // dropped once its exit finishes. The next deadline still needs a timer,
            // and this one has just removed itself, so arm a fresh one.
            self.rearm();
            return;
        }
        self.store
            .lock()
            .unwrap()
            .items
            .retain(|n| !expired.contains(&n.id));
        for id in expired {
            let _ = self.closed.try_send(CloseRequest { id, reason: REASON_EXPIRED });
        }
        self.sync();
    }

    /// One timer for the earliest deadline, replaced whenever the stack changes.
    fn rearm(&mut self) {
        if let Some(token) = self.expiry.take() {
            self.handle.remove(token);
        }
        let deadline = self.cards.iter().filter_map(|card| card.deadline).min();
        let Some(deadline) = deadline else {
            return;
        };
        let timer = calloop::timer::Timer::from_duration(
            deadline.saturating_duration_since(Instant::now()),
        );
        self.expiry = self
            .handle
            .insert_source(timer, |_, _, ui: &mut Ui| {
                // The source removes itself on `Drop`, and `LoopHandle::remove` does not
                // check that a token still names the same source. Keeping the token would
                // let the next `rearm` delete whatever source has since reused the slot.
                ui.expiry = None;
                ui.expire();
                TimeoutAction::Drop
            })
            .ok();
    }

    /// Paints a card where its animation currently has it, and reports whether the
    /// frame reached the compositor.
    ///
    /// Called from `configure`, which is the only source of the size, and from the
    /// frame timer while the card is moving. A `false` means the frame was dropped —
    /// every buffer in the pool can be in flight at once — which the caller has to
    /// retry rather than treat as a finished animation.
    fn draw(&mut self, index: usize) -> bool {
        let Some((width, height)) = self.cards[index].size else {
            return false;
        };
        let stride = width as i32 * 4;
        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) else {
            return false;
        };

        let card = &mut self.cards[index];
        render::paint(
            canvas,
            width,
            height,
            &self.config,
            &mut self.font_system,
            &mut self.swash,
            &mut card.content,
            card.anim.progress(Instant::now()),
        );

        card.surface.wl_surface().damage_buffer(0, 0, width as i32, height as i32);
        if buffer.attach_to(card.surface.wl_surface()).is_err() {
            return false;
        }
        card.surface.commit();
        true
    }

    /// Keeps one frame timer alive while any card is moving.
    fn arm_frames(&mut self) {
        if self.frame.is_some() || !self.cards.iter().any(|card| card.anim.running()) {
            return;
        }
        let timer = calloop::timer::Timer::from_duration(FRAME);
        self.frame = self.handle.insert_source(timer, |_, _, ui: &mut Ui| ui.frames()).ok();
    }

    /// One animation frame: repaint what is moving, settle what has arrived, and
    /// drop the surfaces of what has finished leaving.
    fn frames(&mut self) -> TimeoutAction {
        let now = Instant::now();
        let mut busy = false;

        for index in 0..self.cards.len() {
            match self.cards[index].anim {
                Anim::Settled => {}
                Anim::Entering(_) => {
                    // Park only once the resting frame is really on screen. A dropped
                    // frame would otherwise leave the card stuck part-way faded.
                    let painted = self.draw(index);
                    if painted && self.cards[index].anim.finished(now) {
                        self.cards[index].anim = Anim::Settled;
                    } else {
                        busy = true;
                    }
                }
                Anim::Leaving(_) => {
                    // Nothing to paint once it has gone; `retain` below drops it.
                    if !self.cards[index].anim.finished(now) {
                        self.draw(index);
                        busy = true;
                    }
                }
            }
        }

        // A departing card that has faded out is dropped here, surface and all. It was
        // already out of the stack, so nothing below it needs re-laying out.
        self.cards.retain(|card| !card.leaving_done(now));

        if busy {
            TimeoutAction::ToDuration(FRAME)
        } else {
            // The source removes itself here, so the token goes with it rather than
            // being left to name a slot some later source will reuse.
            self.frame = None;
            TimeoutAction::Drop
        }
    }
}

impl CompositorHandler for Ui {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Ui {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for Ui {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(id) = self
            .cards
            .iter()
            .find(|card| &card.surface == layer)
            .map(|card| card.id)
        {
            self.remove(id, REASON_EXPIRED);
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (width, height) = configure.new_size;
        if width == 0 || height == 0 {
            return;
        }
        let Some(index) = self.cards.iter().position(|card| &card.surface == layer) else {
            return;
        };

        self.cards[index].size = Some((width, height));
        self.draw(index);
    }
}

impl SeatHandler for Ui {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        // No keyboard: cards never take focus, so they never steal typing.
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl PointerHandler for Ui {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if !matches!(event.kind, PointerEventKind::Press { .. }) {
                continue;
            }
            let target = self
                .cards
                .iter()
                .find(|card| card.surface.wl_surface() == &event.surface)
                .map(|card| card.id);
            // Dismiss the frontmost hit and stop: `remove` rebuilds the card list.
            if let Some(id) = target {
                self.remove(id, REASON_DISMISSED);
                return;
            }
        }
    }
}

impl ShmHandler for Ui {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(Ui);

impl ProvidesRegistryState for Ui {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(Ui);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_anchor_matches_the_position() {
        assert_eq!(anchor(Position::TopLeft), Anchor::TOP | Anchor::LEFT);
        assert_eq!(anchor(Position::BottomLeft), Anchor::BOTTOM | Anchor::LEFT);
        assert_eq!(anchor(Position::TopCenter), Anchor::TOP);
        assert_eq!(anchor(Position::BottomCenter), Anchor::BOTTOM);
        assert_eq!(anchor(Position::TopRight), Anchor::TOP | Anchor::RIGHT);
        assert_eq!(anchor(Position::BottomRight), Anchor::BOTTOM | Anchor::RIGHT);
    }

    /// `set_margin` takes top, right, bottom, left. Only anchored edges get a margin, and
    /// the stack is measured from the anchored edge, so a bottom position grows upwards.
    #[test]
    fn the_stack_starts_at_the_named_corner_and_grows_away_from_its_edge() {
        assert_eq!(margins(Position::TopLeft, 0), (MARGIN, 0, 0, MARGIN));
        assert_eq!(margins(Position::TopLeft, 50), (MARGIN + 50, 0, 0, MARGIN));

        assert_eq!(margins(Position::BottomLeft, 0), (0, 0, MARGIN, MARGIN));
        assert_eq!(margins(Position::BottomLeft, 50), (0, 0, MARGIN + 50, MARGIN));

        assert_eq!(margins(Position::TopRight, 0), (MARGIN, MARGIN, 0, 0));
        assert_eq!(margins(Position::BottomRight, 50), (0, MARGIN, MARGIN + 50, 0));

        // A centred column is anchored to no side, so it takes no side margin.
        assert_eq!(margins(Position::TopCenter, 0), (MARGIN, 0, 0, 0));
        assert_eq!(margins(Position::BottomCenter, 7), (0, 0, MARGIN + 7, 0));
    }
}
