//! The `org.freedesktop.Notifications` service and the thread that owns it.
//!
//! The interface never touches Wayland. It mutates the shared queue and pings the
//! UI thread; the UI thread mutates the same queue and pushes a [`CloseRequest`]
//! back here. This thread holds the only `SignalEmitter`, which keeps every
//! emission on the connection's own runtime.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use calloop::ping::Ping;
use zbus::object_server::SignalEmitter;
use zvariant::OwnedValue;

pub const BUS_NAME: &str = "org.freedesktop.Notifications";
pub const PATH: &str = "/org/freedesktop/Notifications";

/// `expire_timeout = -1` on the wire means "pick one for me".
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(5000);

/// Notification spec closing reasons.
pub const REASON_EXPIRED: u32 = 1;
pub const REASON_DISMISSED: u32 = 2;
const REASON_CLOSE_CALL: u32 = 3;

type Hints = HashMap<String, OwnedValue>;

#[derive(Clone, Debug)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    pub body: String,
    /// `None` means the client asked for a notification that never expires.
    pub timeout: Option<Duration>,
}

/// "This id left the screen, for this reason" — sent UI thread to D-Bus thread.
#[derive(Clone, Copy, Debug)]
pub struct CloseRequest {
    pub id: u32,
    pub reason: u32,
}

#[derive(Default)]
pub struct Store {
    pub items: Vec<Notification>,
    next_id: u32,
}

/// The single source of truth for what should be on screen.
pub type Shared = Arc<Mutex<Store>>;

pub fn shared() -> Shared {
    Arc::new(Mutex::new(Store::default()))
}

struct Notifications {
    store: Shared,
    ping: Ping,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    fn get_capabilities(&self) -> Vec<String> {
        // Only "body": advertising "body-markup" or "actions" would promise parsing
        // and rendering that plush does not do.
        vec!["body".to_string()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "plush".to_string(),
            "plush".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            "1.2".to_string(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        _hints: Hints,
        expire_timeout: i32,
    ) -> u32 {
        let timeout = match expire_timeout {
            0 => None,
            ms if ms > 0 => Some(Duration::from_millis(ms as u64)),
            _ => Some(DEFAULT_TIMEOUT),
        };

        let id = {
            let mut store = self.store.lock().unwrap();
            let id = if replaces_id != 0 && store.items.iter().any(|n| n.id == replaces_id) {
                replaces_id
            } else {
                store.next_id = store.next_id.wrapping_add(1);
                store.next_id
            };
            store.items.retain(|n| n.id != id);
            store.items.insert(
                0,
                Notification { id, app_name, summary, body, timeout },
            );
            id
        };

        self.ping.ping();
        id
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let was_shown = {
            let mut store = self.store.lock().unwrap();
            let before = store.items.len();
            store.items.retain(|n| n.id != id);
            store.items.len() != before
        };
        self.ping.ping();
        if was_shown {
            let _ = emitter.notification_closed(id, REASON_CLOSE_CALL).await;
        }
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
}

/// Starts the D-Bus thread. The returned sender asks it to emit
/// `NotificationClosed`; the UI thread is the only caller.
pub fn spawn(store: Shared, ping: Ping) -> async_channel::Sender<CloseRequest> {
    let (tx, rx) = async_channel::bounded(32);
    std::thread::Builder::new()
        .name("plush-dbus".to_string())
        .spawn(move || {
            if let Err(err) = run(store, ping, rx) {
                eprintln!("plush: D-Bus service stopped: {err}");
                std::process::exit(1);
            }
        })
        .expect("spawn D-Bus thread");
    tx
}

fn run(
    store: Shared,
    ping: Ping,
    closed: async_channel::Receiver<CloseRequest>,
) -> zbus::Result<()> {
    async_io::block_on(async move {
        let connection = zbus::connection::Builder::session()?
            .name(BUS_NAME)?
            .serve_at(PATH, Notifications { store, ping })?
            .build()
            .await?;

        let path = PATH.try_into().expect("valid object path");
        let emitter = SignalEmitter::from_parts(connection.clone(), path);

        while let Ok(request) = closed.recv().await {
            emitter.notification_closed(request.id, request.reason).await?;
        }
        Ok(())
    })
}
