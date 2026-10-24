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
use zvariant::{OwnedValue, Value};

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

/// An icon decoded from a notification's `image-data` hint, ready to draw.
///
/// Premultiplied RGBA, which is why it is decoded here rather than in the renderer: the
/// paint path runs about twenty times a second for a panel with a countdown, and this is
/// the layout `tiny_skia` blits without any conversion at all.
#[derive(Clone, Debug, PartialEq)]
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The largest icon plush will hold on to. Anything bigger is scaled down to 32 px anyway,
/// so the only thing the limit saves is the copy: a client must not be able to make the
/// daemon carry a screenshot per notification.
const MAX_ICON: i32 = 512;

/// Decodes the `image-data` hint, if the client sent a usable one.
///
/// The hint is an `(iiibiiay)` structure of width, height, row stride, has-alpha, bits per
/// sample, channel count and data. Both accepted channel counts are read as the cairo
/// formats the hint names — `ARGB32` for four channels, `RGB24` for three — so the bytes are
/// blue, green, red in memory, and with four channels the colours are already multiplied by
/// the alpha. That is the reading every other daemon uses; `has_alpha` is not consulted,
/// because the channel count is what says how the bytes are laid out.
///
/// Anything else is not an icon: a hint plush cannot read is dropped rather than guessed at,
/// and the notification is shown as a text-only panel. Nothing here may panic — the hint
/// comes off a bus that any client can write to.
fn icon_from(hints: &Hints) -> Option<Icon> {
    // `a{sv}` may hand the value back still wrapped in its variant.
    let hint = hints.get("image-data")?;
    let mut value: &Value<'_> = hint;
    while let Value::Value(inner) = value {
        value = inner;
    }
    let Value::Structure(structure) = value else {
        return None;
    };
    let [width, height, stride, _has_alpha, bits, channels, data] = structure.fields() else {
        return None;
    };
    let int = |value: &Value<'_>| value.downcast_ref::<i32>().ok();
    let (width, height, stride, bits, channels) =
        (int(width)?, int(height)?, int(stride)?, int(bits)?, int(channels)?);
    if bits != 8 || !(3..=4).contains(&channels) {
        return None;
    }
    if width <= 0 || height <= 0 || width > MAX_ICON || height > MAX_ICON {
        return None;
    }
    let Value::Array(data) = data else {
        return None;
    };
    let data: Vec<u8> =
        data.inner().iter().map(|byte| byte.downcast_ref::<u8>().ok()).collect::<Option<_>>()?;

    let (width, height, stride, channels) =
        (width as usize, height as usize, stride as usize, channels as usize);
    // A row may be padded, so the stride says where each row starts and how much data there
    // has to be. Checked, because both come from the client — and a negative stride would
    // read as an enormous one.
    if stride < width * channels || stride.checked_mul(height)? > data.len() {
        return None;
    }

    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in data.chunks(stride).take(height) {
        for pixel in row[..width * channels].chunks_exact(channels) {
            let alpha = if channels == 4 { pixel[3] } else { u8::MAX };
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], alpha]);
        }
    }
    Some(Icon { width: width as u32, height: height as u32, rgba })
}

#[derive(Clone, Debug)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    pub body: String,
    /// `None` means the client asked for a notification that never expires.
    pub timeout: Option<Duration>,
    /// The `image-data` hint, or `None` when the client sent none or one that is unreadable.
    pub icon: Option<Icon>,
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
    /// Ids the user asked to close from outside the panel — `plush dismiss` — waiting for the
    /// UI thread to take them. The reason a panel leaves is chosen there, and this is the only
    /// way the command line can say "the user asked for this" rather than "it disappeared".
    pub dismissed: Vec<u32>,
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
        {
            let mut store = self.store.lock().unwrap();
            store.items.retain(|n| !ids.contains(&n.id));
            store.dismissed.extend(ids.iter().copied());
        }
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
        hints: Hints,
        expire_timeout: i32,
    ) -> u32 {
        let timeout = match expire_timeout {
            0 => None,
            ms if ms > 0 => Some(Duration::from_millis(ms as u64)),
            _ => Some(DEFAULT_TIMEOUT),
        };
        let icon = icon_from(&hints);

        let id = {
            let mut store = self.store.lock().unwrap();
            let id = if replaces_id != 0 && store.items.iter().any(|n| n.id == replaces_id) {
                replaces_id
            } else {
                store.next_id = store.next_id.wrapping_add(1);
                store.next_id
            };
            store.items.retain(|n| n.id != id);
            store.items.insert(0, Notification { id, app_name, summary, body, timeout, icon });
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
                icon: None,
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

    /// The UI thread learns why a panel is going away from the store, so a dismissal from the
    /// command line leaves the way a click does rather than the way a timeout does.
    #[test]
    fn a_dismissal_records_that_the_user_asked() {
        let (control, store, _requests) = control();

        control.dismiss();
        assert_eq!(store.lock().unwrap().dismissed, vec![3], "the one that went");

        control.dismiss_all();
        let mut dismissed = store.lock().unwrap().dismissed.clone();
        dismissed.sort_unstable();
        assert_eq!(dismissed, vec![1, 2, 3], "and then the rest");
    }

    #[test]
    fn dismiss_all_clears_the_queue() {
        let (control, store, requests) = control();

        control.dismiss_all();

        assert!(ids(&store).is_empty());
        let announced: Vec<u32> =
            std::iter::from_fn(|| requests.try_recv().ok()).map(|request| request.id).collect();
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

    /// A hint the way a client puts one on the wire: the seven `image-data` fields, as a
    /// variant inside the hint dictionary.
    fn image_data(
        width: i32,
        height: i32,
        stride: i32,
        has_alpha: bool,
        bits: i32,
        channels: i32,
        data: Vec<u8>,
    ) -> Hints {
        let structure =
            zvariant::Structure::from((width, height, stride, has_alpha, bits, channels, data));
        hints("image-data", Value::Structure(structure))
    }

    fn hints(name: &str, value: Value<'static>) -> Hints {
        let mut hints = Hints::new();
        hints.insert(name.to_string(), OwnedValue::try_from(value).expect("an owned value"));
        hints
    }

    /// Four channels are cairo's ARGB32: blue, green, red, alpha in memory, with the
    /// colours already multiplied by the alpha.
    #[test]
    fn a_four_channel_hint_becomes_premultiplied_rgba() {
        let hint = image_data(2, 1, 8, true, 8, 4, vec![10, 20, 30, 40, 50, 60, 70, 80]);

        let icon = icon_from(&hint).expect("a usable hint");

        assert_eq!((icon.width, icon.height), (2, 1));
        assert_eq!(icon.rgba, vec![30, 20, 10, 40, 70, 60, 50, 80], "BGRa in, RGBA out");
    }

    /// Three channels are cairo's RGB24: no alpha byte at all, so every pixel is opaque.
    #[test]
    fn a_three_channel_hint_is_opaque() {
        let hint = image_data(2, 1, 6, false, 8, 3, vec![10, 20, 30, 50, 60, 70]);

        let icon = icon_from(&hint).expect("a usable hint");

        assert_eq!(icon.rgba, vec![30, 20, 10, 255, 70, 60, 50, 255]);
    }

    /// A row is allowed to be padded to a wider stride, so rows must not be read as though
    /// they were packed.
    #[test]
    fn a_padded_row_stride_is_respected() {
        let hint = image_data(
            1,
            2,
            8,
            true,
            8,
            4,
            vec![10, 20, 30, 40, 9, 9, 9, 9, 50, 60, 70, 80, 9, 9, 9, 9],
        );

        let icon = icon_from(&hint).expect("a usable hint");

        assert_eq!(icon.rgba, vec![30, 20, 10, 40, 70, 60, 50, 80], "padding is skipped");
    }

    /// The value may come back still wrapped in its variant, which is what `a{sv}` does.
    #[test]
    fn a_variant_wrapped_hint_is_unwrapped() {
        let structure = zvariant::Structure::from((1, 1, 4, true, 8, 4, vec![1u8, 2, 3, 4]));
        let wrapped = Value::Value(Box::new(Value::Structure(structure)));

        let icon = icon_from(&hints("image-data", wrapped)).expect("a usable hint");

        assert_eq!(icon.rgba, vec![3, 2, 1, 4]);
    }

    #[test]
    fn hints_that_are_not_an_icon_are_dropped() {
        let good = || vec![1u8, 2, 3, 4];
        let cases: Vec<(&str, Hints)> = vec![
            ("no hint at all", Hints::new()),
            ("a hint under another name", hints("image-path", Value::Str("icon.png".into()))),
            ("a plain string", hints("image-data", Value::Str("4x4".into()))),
            ("a structure of three fields", {
                let structure = zvariant::Structure::from((1, 1, 4));
                hints("image-data", Value::Structure(structure))
            }),
            ("a data array that is not bytes", {
                let structure =
                    zvariant::Structure::from((1, 1, 4, true, 8, 4, vec![1i32, 2, 3, 4]));
                hints("image-data", Value::Structure(structure))
            }),
            ("16 bits per sample", image_data(1, 1, 8, true, 16, 4, good())),
            ("one channel", image_data(1, 1, 1, false, 8, 1, vec![1])),
            ("five channels", image_data(1, 1, 5, true, 8, 5, vec![1, 2, 3, 4, 5])),
            ("no width", image_data(0, 1, 4, true, 8, 4, good())),
            ("a negative height", image_data(1, -1, 4, true, 8, 4, good())),
            ("a stride narrower than a row", image_data(2, 1, 4, true, 8, 4, good())),
            ("a negative stride", image_data(2, 1, -8, true, 8, 4, good())),
            ("less data than it claims", image_data(4, 4, 16, true, 8, 4, good())),
            ("an image too big to hold", image_data(4096, 4096, 16384, true, 8, 4, good())),
        ];

        for (what, hint) in cases {
            assert!(icon_from(&hint).is_none(), "{what} should not become an icon");
        }
    }

    /// A `Notify` with an unreadable hint still has to answer, and still has to show the
    /// text: the panel only loses its icon.
    #[test]
    fn a_notification_carries_the_icon_it_was_sent() {
        let store = shared();
        let (ping, _source) = make_ping().expect("an eventfd");
        let notifications = Notifications { store: store.clone(), ping };
        let call = |hints: Hints| {
            async_io::block_on(notifications.notify(
                "test".into(),
                0,
                String::new(),
                "summary".into(),
                "body".into(),
                Vec::new(),
                hints,
                -1,
            ))
        };

        let id = call(image_data(1, 1, 4, true, 8, 4, vec![1, 2, 3, 4]));
        let id_without_icon = call(hints("image-data", Value::Str("nope".into())));

        let items = store.lock().unwrap();
        let with_icon = items.items.iter().find(|n| n.id == id).expect("the first notification");
        let without =
            items.items.iter().find(|n| n.id == id_without_icon).expect("the second notification");
        assert_eq!(with_icon.icon.as_ref().map(|icon| icon.rgba.clone()), Some(vec![3, 2, 1, 4]));
        assert_eq!(without.icon, None, "an unreadable hint leaves a text-only panel");
        assert_eq!(without.summary, "summary", "and the notification is still there");
    }

    #[test]
    fn a_notification_with_no_timeout_never_expires() {
        let store = shared();
        let (ping, _source) = make_ping().expect("an eventfd");
        let notifications = Notifications { store: store.clone(), ping };

        async_io::block_on(notifications.notify(
            "test".into(),
            0,
            String::new(),
            "summary".into(),
            String::new(),
            Vec::new(),
            Hints::new(),
            0,
        ));

        assert_eq!(store.lock().unwrap().items[0].timeout, None);
    }
}
