# plush

A fast, minimal notification daemon for wlroots-based Wayland compositors.

`plush` implements `org.freedesktop.Notifications` and renders each notification as a
translucent, rounded panel pinned to the top-right of the screen. It is small on purpose:
about a thousand lines, five dependencies that matter, no configuration file, and no
runtime state beyond the notifications currently on screen.

```
┌────────────────────────────────┐
│ Test App                       │
│ Build finished                 │
│ cargo build --release succeeded│
│ in 12.4s.                      │
└────────────────────────────────┘
```

## Requirements

- A compositor implementing `zwlr_layer_shell_v1` — sway, Hyprland, river, labwc and
  other wlroots-derived compositors. Plain GNOME/Mutter and KDE/KWin do not implement it.
- `wl_shm` for software rendering. No GPU is required.
- A D-Bus session bus.

## Build

```sh
cargo build --release
```

## Install

```sh
install -Dm755 target/release/plush ~/.local/bin/plush

# Lets clients start plush on demand instead of requiring it to be running already.
mkdir -p ~/.local/share/dbus-1/services
sed "s|/usr/bin/plush|$HOME/.local/bin/plush|" data/org.freedesktop.Notifications.service \
  > ~/.local/share/dbus-1/services/org.freedesktop.Notifications.service
```

Start it from your session startup, or just let the first notification start it.

## Usage

```sh
notify-send "Build finished" "cargo build --release succeeded in 12.4s"
notify-send -t 0 "Stays until clicked"      # 0 means never expire
notify-send -u critical "Urgent"            # urgency is accepted and ignored
```

Click a panel to dismiss it.

## Behaviour

| | |
|---|---|
| Anchor | top-right, 16 px from the edge |
| Panel | 380 px wide, 14 px corner radius, 16 px padding |
| Stack | up to 4 panels, 10 px apart, newest on top |
| Max height | 320 px; longer bodies are clipped |
| Expiry | per-notification; `-1` becomes 5000 ms, `0` never expires |
| `replaces_id` | replaces the content in place and restarts the timeout |
| Dismiss | pointer press, or `CloseNotification` |
| Closing reasons | `1` expired, `2` dismissed by user, `3` `CloseNotification` |
| `GetCapabilities` | `["body"]` |
| `GetServerInformation` | `plush`, `plush`, version, `1.2` |

## Deliberately not implemented

- No configuration file. The palette, width, position and limits are compile-time constants.
- No icon rendering: `app_icon` is accepted and ignored.
- No markup: `body-markup` is not advertised, so clients send plain text.
- No action buttons: `actions` is accepted, `ActionInvoked` is never emitted.
- No grouping or notification history.
- No light theme: one dark palette, no `color-scheme` detection.
- No keyboard handling: panels never take focus.
- Corners are circular arcs, not Apple's continuous-curvature squircles.
- Panels are positioned in logical pixels; output scale is not consulted, so a HiDPI
  output gets a panel that is physically smaller.

## Design

Two threads, one shared queue.

The main thread runs a `calloop` event loop holding `WaylandSource` and a ping source. It
owns the `FontSystem`, the glyph cache, the `shm` pool, and every layer surface. It is the
only code that touches Wayland.

The D-Bus thread runs its own `async-io` runtime with the `org.freedesktop.Notifications`
interface. It owns the connection, and therefore the only `SignalEmitter` — so every
`NotificationClosed` is emitted from the connection's own runtime rather than from a
foreign thread.

They communicate through a `Mutex<Vec<Notification>>` plus a `calloop` ping. The D-Bus side
mutates the queue and pings; the main thread reconciles its panels against the queue. When
the main thread retires a notification (expiry or click) it pushes a reason back over an
`async_channel` for the D-Bus thread to emit. Neither thread blocks on the other.

Two consequences worth knowing:

- **A panel is drawn only from `configure`.** The compositor's suggested size is the single
  source of truth for surface state, so a buffer can never be attached with stale geometry.
- **Replacing a notification rebuilds its surface.** A commit without a new buffer does not
  repaint, so a fresh surface is the reliable way to get a fresh `configure`.

Text is shaped with `cosmic-text` and rasterised through `SwashCache` by hand. The glyph
path deliberately avoids `cosmic_text::Buffer::draw`, which hides the coverage and
premultiplication convention; `plush` blends coverage itself and swaps red and blue at the
end, because `wl_shm`'s `Argb8888` is BGRA in memory while `tiny-skia` paints RGBA.

## Verified

- Builds and passes `cargo clippy --all-targets` with no warnings.
- End-to-end against a headless sway: `notify-send`, `replaces_id` replacement,
  `CloseNotification`, and timeout expiry all behave, with zero `wl_display` errors in the
  Wayland protocol trace.
- Panel pixels are covered by unit tests asserting the rounded corners, the opaque
  interior, and that a title never falls back to a second font mid-run.

## License

MIT.
