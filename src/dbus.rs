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

/// The interface behind `plush dismiss`, on the same connection but kept off the
/// notification object so the two never have to share a namespace.
pub const CONTROL_INTERFACE: &str = "org.plush.Control";
pub const CONTROL_PATH: &str = "/org/plush/Control";

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

/// What `plush dismiss` was asked to take off the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dismiss {
    /// The newest one, which is the panel on top of the stack.
    Newest,
    /// Everything currently shown.
    All,
}

/// Asks the running daemon to dismiss. This runs in the CLI process, so it talks to
/// the daemon rather than touching a queue it does not share.
pub fn request_dismiss(what: Dismiss) -> zbus::Result<()> {
    let method = match what {
        Dismiss::Newest => "Dismiss",
        Dismiss::All => "DismissAll",
    };
    async_io::block_on(async {
        let connection = zbus::Connection::session().await?;
        connection
            .call_method(Some(BUS_NAME), CONTROL_PATH, Some(CONTROL_INTERFACE), method, &())
            .await?;
        Ok(())
    })
}

/// Takes notifications out of the queue and tells the D-Bus thread to announce it.
/// The UI thread then animates them off screen, exactly as if they had been clicked.
struct Control {
    store: Shared,
    ping: Ping,
    closed: async_channel::Sender<CloseRequest>,
}

#[zbus::interface(name = "org.plush.Control")]
impl Control {
    fn dismiss(&self) {
        let newest = self.store.lock().unwrap().items.first().map(|n| n.id);
        self.retire(newest.into_iter().collect());
    }

    fn dismiss_all(&self) {
        let ids = self.store.lock().unwrap().items.iter().map(|n| n.id).collect();
        self.retire(ids);
    }
}

impl Control {
    fn retire(&self, ids: Vec<u32>) {
        if ids.is_empty() {
            return;
        }
        self.store.lock().unwrap().items.retain(|n| !ids.contains(&n.id));
        for id in ids {
            let _ = self.closed.try_send(CloseRequest { id, reason: REASON_DISMISSED });
        }
        self.ping.ping();
    }
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
    let control = tx.clone();
    std::thread::Builder::new()
        .name("plush-dbus".to_string())
        .spawn(move || {
            if let Err(err) = run(store, ping, rx, control) {
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
    control: async_channel::Sender<CloseRequest>,
) -> zbus::Result<()> {
    async_io::block_on(async move {
        let connection = zbus::connection::Builder::session()?
            .name(BUS_NAME)?
            .serve_at(PATH, Notifications { store: store.clone(), ping: ping.clone() })?
            .serve_at(CONTROL_PATH, Control { store, ping, closed: control })?
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

#[cfg(test)]
mod tests {
    use super::*;
    use calloop::ping::make_ping;

    fn queued(store: &Shared, id: u32, summary: &str) {
        store.lock().unwrap().items.insert(
            0,
            Notification {
                id,
                app_name: "test".into(),
                summary: summary.into(),
                body: String::new(),
                timeout: None,
            },
        );
    }

    fn ids(store: &Shared) -> Vec<u32> {
        store.lock().unwrap().items.iter().map(|n| n.id).collect()
    }

    /// The three are queued newest first, so id 3 is the one on top of the stack.
    fn control() -> (Control, Shared, async_channel::Receiver<CloseRequest>) {
        let store = shared();
        queued(&store, 1, "oldest");
        queued(&store, 2, "middle");
        queued(&store, 3, "newest");
        let (ping, _source) = make_ping().expect("an eventfd");
        let (closed, requests) = async_channel::bounded(8);
        (Control { store: store.clone(), ping, closed }, store, requests)
    }

    #[test]
    fn dismiss_takes_the_newest_notification() {
        let (control, store, requests) = control();

        control.dismiss();

        assert_eq!(ids(&store), vec![2, 1], "only the top panel goes");
        let request = requests.try_recv().expect("a close was announced");
        assert_eq!(request.id, 3);
        assert_eq!(request.reason, REASON_DISMISSED);
    }

    #[test]
    fn dismiss_all_clears_the_queue() {
        let (control, store, requests) = control();

        control.dismiss_all();

        assert!(ids(&store).is_empty());
        let announced: Vec<u32> = std::iter::from_fn(|| requests.try_recv().ok())
            .map(|request| request.id)
            .collect();
        assert_eq!(announced.len(), 3, "every notification is announced");
        assert!(announced.contains(&1) && announced.contains(&2) && announced.contains(&3));
    }

    #[test]
    fn dismissing_an_empty_queue_is_quiet() {
        let store = shared();
        let (ping, _source) = make_ping().expect("an eventfd");
        let (closed, requests) = async_channel::bounded(8);
        let control = Control { store: store.clone(), ping, closed };

        control.dismiss();
        control.dismiss_all();

        assert!(requests.try_recv().is_err(), "nothing to announce");
    }
}
