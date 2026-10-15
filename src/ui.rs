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
    dbus::{CloseRequest, Icon, Notification, Shared, REASON_DISMISSED, REASON_EXPIRED},
    render,
};

/// Distance from the screen edge, in logical pixels.
const MARGIN: i32 = 16;
/// Space between stacked cards.
const GAP: i32 = 10;
/// One animation frame. 60 Hz, which is what the motion was tuned against.
const FRAME: Duration = Duration::from_millis(16);
/// One tick of the countdown. 20 Hz, which is enough for the line to read as draining
/// rather than stepping, and cheap enough to run for the length of a notification.
const TICK: Duration = Duration::from_millis(50);

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
    /// What this card was built from, so a `replaces_id` update is detected.
    source: Source,
    surface: LayerSurface,
    content: Buffer,
    /// Kept on the card rather than read back out of the queue: a card that is already
    /// leaving is gone from the queue but still being painted.
    icon: Option<Icon>,
    height: u32,
    /// The size the compositor last configured. Animation redraws need it, which is
    /// why a card cannot be painted before its first `configure`.
    size: Option<(u32, u32)>,
    timeouts: Timeouts,
    anim: Anim,
}

/// What a card was built from. A change in any of it means the card is rebuilt rather
/// than patched, which is the only way to get a fresh `configure` to draw with — and an
/// icon that changed under a constant `replaces_id` is such a change.
#[derive(PartialEq)]
struct Source {
    app_name: String,
    summary: String,
    body: String,
    icon: Option<Icon>,
}

impl Source {
    fn of(notification: &Notification) -> Self {
        Self {
            app_name: notification.app_name.clone(),
            summary: notification.summary.clone(),
            body: notification.body.clone(),
            icon: notification.icon.clone(),
        }
    }

    /// Compared against the notification rather than against another `Source`, so a
    /// queued icon is never cloned just to be compared.
    fn matches(&self, notification: &Notification) -> bool {
        self.app_name == notification.app_name
            && self.summary == notification.summary
            && self.body == notification.body
            && self.icon == notification.icon
    }
}

/// When a card goes away, and what a hold does to that.
///
/// `timeout` is the client's total, which is the denominator of the countdown; `deadline`
/// is when the card expires, and is absent both for a card that never expires and for one
/// being held; `held` is the remaining time a hold froze. Keeping the held time here is
/// what makes a hold resume with what was left rather than with the client's timeout
/// again, and it is why `rearm` needs no special case: a held card has no deadline.
#[derive(Clone, Copy, Debug)]
struct Timeouts {
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    held: Option<Duration>,
}

impl Timeouts {
    fn new(timeout: Option<Duration>, now: Instant) -> Self {
        Self { timeout, deadline: timeout.map(|timeout| now + timeout), held: None }
    }

    /// Freezes the countdown where it is. A card that never expires, or one that is
    /// already held, does not move.
    fn hold(&mut self, now: Instant) {
        if self.held.is_none() {
            self.held = self.remaining(now);
            self.deadline = None;
        }
    }

    /// Resumes with the time that was left, not with the client's timeout again.
    fn release(&mut self, now: Instant) {
        if let Some(left) = self.held.take() {
            self.deadline = Some(now + left);
        }
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.held
            .or_else(|| self.deadline.map(|deadline| deadline.saturating_duration_since(now)))
    }

    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| deadline <= now)
    }

    /// How much of the client's timeout is left, which is the length of the countdown
    /// line. `None` for a card that never expires, which has no line to draw.
    fn fraction(&self, now: Instant) -> Option<f32> {
        let timeout = self.timeout?;
        let left = self.remaining(now)?;
        Some((left.as_secs_f32() / timeout.as_secs_f32()).clamp(0.0, 1.0))
    }
}

impl Card {
    /// An exit that has run its course; the surface is dropped once this is true.
    fn leaving_done(&self, now: Instant) -> bool {
        self.anim.leaving() && self.anim.finished(now)
    }
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
    /// The countdown timer, alive only while a line is draining.
    tick: Option<RegistrationToken>,
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
            tick: None,
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
                .position(|card| card.id == notification.id && card.source.matches(notification));
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
        self.arm_tick();
    }

    fn make_card(&mut self, notification: &Notification) -> Card {
        let has_icon = notification.icon.is_some();
        let mut content = render::build(&mut self.font_system, &self.config, notification);
        let height = render::card_height(&mut self.font_system, &mut content, has_icon);

        let surface = self.compositor.create_surface(&self.qh);
        let card = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("plush"),
            None,
        );
        card.set_anchor(anchor(self.config.panel.position));
        // 0: be moved clear of anything that has claimed space, such as a bar, and claim
        // none ourselves, so windows may use the full screen under us.
        card.set_exclusive_zone(0);
        card.set_keyboard_interactivity(KeyboardInteractivity::None);
        card.set_size(render::PANEL_W, height);
        // An initial commit with no buffer; the configure it triggers is what draws.
        card.commit();

        Card {
            id: notification.id,
            source: Source::of(notification),
            surface: card,
            content,
            icon: notification.icon.clone(),
            height,
            timeouts: Timeouts::new(notification.timeout, Instant::now()),
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
            .filter(|card| card.timeouts.expired(now))
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
        let deadline = self.cards.iter().filter_map(|card| card.timeouts.deadline).min();
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
        let now = Instant::now();
        let remaining = card.timeouts.fraction(now);
        render::paint(
            canvas,
            width,
            height,
            &self.config,
            card.icon.as_ref(),
            &mut self.font_system,
            &mut self.swash,
            &mut card.content,
            card.anim.progress(now),
            remaining,
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

    /// Holds the countdown of the card under the pointer open. A card on its way out has
    /// nothing left to hold.
    fn hold(&mut self, id: u32) {
        let now = Instant::now();
        if let Some(card) = self
            .cards
            .iter_mut()
            .find(|card| card.id == id && !card.anim.leaving())
        {
            card.timeouts.hold(now);
        }
        self.rearm();
        self.arm_tick();
    }

    /// Puts a held card back on its timeout, with the time it had left. A card that is
    /// on its way out is skipped for the same reason `hold` skips it: it has already
    /// been announced as closed, and giving it a deadline again would announce it twice.
    fn release(&mut self, id: u32) {
        let now = Instant::now();
        if let Some(card) = self
            .cards
            .iter_mut()
            .find(|card| card.id == id && !card.anim.leaving())
        {
            card.timeouts.release(now);
        }
        self.rearm();
        self.arm_tick();
    }

    /// Keeps one tick alive while any card has a countdown to draw. A held card has
    /// none: its line is frozen, so there is nothing to repaint.
    fn arm_tick(&mut self) {
        if self.tick.is_some() || !self.draining() {
            return;
        }
        let timer = calloop::timer::Timer::from_duration(TICK);
        self.tick = self.handle.insert_source(timer, |_, _, ui: &mut Ui| ui.ticks()).ok();
    }

    /// Whether any card on screen has a countdown running.
    fn draining(&self) -> bool {
        self.cards
            .iter()
            .any(|card| !card.anim.leaving() && card.timeouts.deadline.is_some())
    }

    /// One countdown tick: repaint the cards whose line is draining. A card that is still
    /// moving is left to the frame timer, which is already painting it.
    fn ticks(&mut self) -> TimeoutAction {
        for index in 0..self.cards.len() {
            let settled = {
                let card = &self.cards[index];
                !card.anim.running() && card.timeouts.deadline.is_some()
            };
            if settled {
                self.draw(index);
            }
        }

        if self.draining() {
            TimeoutAction::ToDuration(TICK)
        } else {
            // Like the frame timer, the source removes itself here, so the token goes
            // with it rather than being left to name a slot some later source reuses.
            self.tick = None;
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
            let Some(id) = self
                .cards
                .iter()
                .find(|card| card.surface.wl_surface() == &event.surface)
                .map(|card| card.id)
            else {
                continue;
            };
            match event.kind {
                // A card the pointer is inside is held open; leaving it puts it back on
                // its timeout with the time that was left.
                PointerEventKind::Enter { .. } => self.hold(id),
                PointerEventKind::Leave { .. } => self.release(id),
                // Dismiss the frontmost hit and stop: `remove` rebuilds the card list.
                PointerEventKind::Press { .. } => {
                    self.remove(id, REASON_DISMISSED);
                    return;
                }
                _ => {}
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

    /// A hold freezes the countdown; releasing it runs what was left, rather than the
    /// client's timeout over again.
    #[test]
    fn a_hold_resumes_with_the_time_that_was_left() {
        let start = Instant::now();
        let mut timeouts = Timeouts::new(Some(Duration::from_secs(5)), start);
        let held = start + Duration::from_secs(2);

        timeouts.hold(held);
        assert!(!timeouts.expired(held + Duration::from_secs(30)));
        assert_eq!(timeouts.remaining(held + Duration::from_secs(30)), Some(Duration::from_secs(3)));
        assert_eq!(timeouts.fraction(held + Duration::from_secs(30)), Some(0.6));

        timeouts.release(held + Duration::from_secs(30));
        assert_eq!(timeouts.deadline, Some(held + Duration::from_secs(33)));
        assert_eq!(timeouts.remaining(held + Duration::from_secs(31)), Some(Duration::from_secs(2)));
        assert!(!timeouts.expired(held + Duration::from_secs(32)));
        assert!(timeouts.expired(held + Duration::from_secs(33)));
    }

    /// A hold is time the client never asked for: it pauses the countdown, it does not
    /// add to it.
    #[test]
    fn a_hold_never_extends_the_total_timeout() {
        let start = Instant::now();
        let mut timeouts = Timeouts::new(Some(Duration::from_secs(2)), start);

        timeouts.hold(start + Duration::from_secs(1));
        timeouts.release(start + Duration::from_secs(61));

        assert_eq!(timeouts.deadline, Some(start + Duration::from_secs(62)));
        assert!(!timeouts.expired(start + Duration::from_secs(61)));
        assert!(timeouts.expired(start + Duration::from_secs(62)));
    }

    /// A card the client asked to never expire has no countdown to freeze, so a hold is
    /// a no-op rather than something that could give it a deadline.
    #[test]
    fn a_hold_on_a_panel_that_never_expires_is_a_no_op() {
        let start = Instant::now();
        let mut timeouts = Timeouts::new(None, start);

        timeouts.hold(start + Duration::from_secs(1));
        timeouts.hold(start + Duration::from_secs(2));
        timeouts.release(start + Duration::from_secs(3));

        assert_eq!(timeouts.deadline, None);
        assert_eq!(timeouts.remaining(start + Duration::from_secs(9)), None);
        assert_eq!(timeouts.fraction(start + Duration::from_secs(9)), None, "no line to draw");
        assert!(!timeouts.expired(start + Duration::from_secs(9)));
    }

    /// The line is the fraction of the client's timeout that is left, which is what makes
    /// it agree with the expiry it is counting down to.
    #[test]
    fn the_countdown_is_the_fraction_of_the_timeout_that_is_left() {
        let start = Instant::now();
        let timeouts = Timeouts::new(Some(Duration::from_millis(4000)), start);

        assert_eq!(timeouts.fraction(start), Some(1.0));
        assert_eq!(timeouts.fraction(start + Duration::from_secs(1)), Some(0.75));
        assert_eq!(timeouts.fraction(start + Duration::from_secs(4)), Some(0.0));
        assert_eq!(timeouts.fraction(start + Duration::from_secs(9)), Some(0.0));
    }

    /// A `replaces_id` that changes only the icon still has to rebuild the card, or the
    /// new icon never reaches the screen.
    #[test]
    fn a_new_icon_is_a_change_to_rebuild_for() {
        let icon = |pixel: u8| Some(Icon { width: 1, height: 1, rgba: vec![pixel, 0, 0, 255] });
        let notification = |icon: Option<Icon>| Notification {
            id: 7,
            app_name: "app".into(),
            summary: "summary".into(),
            body: "body".into(),
            timeout: None,
            icon,
        };

        let original = Source::of(&notification(icon(1)));
        assert!(original.matches(&notification(icon(1))), "the same icon is not a change");
        assert!(!original.matches(&notification(icon(2))), "a different icon is");
        assert!(!original.matches(&notification(None)), "and losing it is too");

        let retexted = Notification { summary: "another summary".into(), ..notification(icon(1)) };
        assert!(!original.matches(&retexted), "as is the text it was always watching");
    }
}
