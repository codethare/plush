mod dbus;
mod render;
mod ui;

use std::time::Duration;

use calloop::{ping::make_ping, EventLoop};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::CompositorState,
    shell::wlr_layer::LayerShell,
    shm::{slot::SlotPool, Shm},
};
use wayland_client::{globals::registry_queue_init, Connection};

fn main() {
    let connection =
        Connection::connect_to_env().expect("plush: no Wayland compositor in this session");
    let (globals, event_queue) =
        registry_queue_init::<ui::Ui>(&connection).expect("plush: could not read the Wayland registry");

    let qh = event_queue.handle();
    let compositor =
        CompositorState::bind(&globals, &qh).expect("plush: wl_compositor is unavailable");
    let layer_shell = LayerShell::bind(&globals, &qh)
        .expect("plush: zwlr_layer_shell_v1 is unavailable (a wlroots compositor is required)");
    let shm = Shm::bind(&globals, &qh).expect("plush: wl_shm is unavailable");

    // Room for a spare buffer per card. memfd pages stay uncommitted until a card
    // actually paints them, so oversizing this costs address space, not memory.
    let pool_size = render::PANEL_W as usize * render::MAX_H as usize * 4 * render::MAX_CARDS * 2;
    let pool = SlotPool::new(pool_size, &shm).expect("plush: could not create the shm pool");

    let mut event_loop: EventLoop<'static, ui::Ui> =
        EventLoop::try_new().expect("plush: could not create the event loop");
    let handle = event_loop.handle();

    let (ping, ping_source) = make_ping().expect("plush: could not create the wakeup source");
    handle
        .insert_source(ping_source, |_, _, ui: &mut ui::Ui| ui.sync())
        .expect("plush: could not register the wakeup source");

    WaylandSource::new(connection, event_queue)
        .insert(handle.clone())
        .expect("plush: could not register the Wayland source");

    let store = dbus::shared();
    let closed = dbus::spawn(store.clone(), ping);

    let mut ui = ui::Ui::new(
        qh,
        handle.clone(),
        &globals,
        compositor,
        layer_shell,
        shm,
        pool,
        store,
        closed,
    );
    ui.sync();

    event_loop
        .run(None::<Duration>, &mut ui, |_| {})
        .expect("plush: event loop failed");
}
