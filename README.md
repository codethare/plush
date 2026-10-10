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
| Animation | slides in from the right and fades up over 220 ms; the same motion reversed on the way out |
| `replaces_id` | replaces the content in place and restarts the timeout |
| Dismiss | pointer press, or `CloseNotification` |
| Closing reasons | `1` expired, `2` dismissed by user, `3` `CloseNotification` |
| `GetCapabilities` | `["body"]` |
| `GetServerInformation` | `plush`, `plush`, version, `1.2` |

## Configuration

plush reads `$XDG_CONFIG_HOME/plush/config.toml`, which is `~/.config/plush/config.toml`
unless `XDG_CONFIG_HOME` has been moved. **There is no need to create it:** with no user file
plush runs the bundled `data/config.toml`, which is the look it has always had. Copy that file
across only when you want to change something.

The file is optional, and so is every key in it: anything you leave out keeps the default shown
below.

```sh
mkdir -p ~/.config/plush
cp data/config.toml ~/.config/plush/config.toml
```

A file that is present but wrong stops the daemon with a parser error naming the file, the
line and the column. Unknown keys and unknown tables are rejected too, so a typo cannot leave
you staring at a panel that quietly ignored your setting.

Colours are `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`. The shorthand repeats each digit, so
`#abc` is `#aabbcc`. The alpha channel is meaningful: below `ff` the panel is translucent over
whatever is behind it.

### `[border]`

| Key | Default | |
|---|---|---|
| `width` | `1.0` | Stroke width in logical pixels. `0` draws no border. |
| `color` | `"#ffffff1a"` | A 10% white highlight. |

### `[background]`

| Key | Default | |
|---|---|---|
| `color` | `"#1c1c1ed6"` | A translucent dark grey. |

### `[font]`

| Key | Default | |
|---|---|---|
| `family` | `"sans-serif"` | Any installed family, or a generic: `sans-serif`, `serif`, `monospace`, `cursive`, `fantasy`. |
| `app-size` | `11.0` | The app name line. |
| `app-line-height` | `15.0` | |
| `app-weight` | `"medium"` | |
| `title-size` | `14.0` | The summary line. |
| `title-line-height` | `19.0` | |
| `title-weight` | `"bold"` | |
| `body-size` | `13.0` | The body text. |
| `body-line-height` | `18.0` | |
| `body-weight` | `"normal"` | |

Weights are named: `thin`, `extra-light`, `light`, `normal`, `medium`, `semibold`, `bold`,
`extra-bold`, `black`.

`title-weight = "bold"` is the default rather than `"semibold"` on purpose. fontconfig resolves
weight 600 for the generic sans family to a serif CJK face whose space glyph is missing, which
then falls back to a colour-emoji space seventeen pixels wide and leaves a hole in the title.
`semibold` is accepted, and is fine when the family you name actually has that weight.

### `[animation]`

| Key | Default | |
|---|---|---|
| `enabled` | `true` | Slide and fade each panel in from the right, and back out when it goes away. |

A panel arrives over 220 ms: it slides in from just past the right edge while fading
up, and eases out so it leaves the edge quickly and settles gently. Dismissing or
expiring a panel runs the same motion backwards. The duration is fixed; there is no
speed setting.

The surface itself never moves. `zwlr_layer_shell_v1` only honours margins on edges you
have anchored to, and sliding in from off-screen would need a negative margin, which the
protocol never promises — so the slide is drawn inside the buffer and works the same on
every compositor. Frames are driven by a timer at 60 Hz that only runs while something is
moving; with `enabled = false` there is no timer at all and panels simply appear.

## Deliberately not implemented

- No configuration beyond the border, the background, the font and the animation toggle.
  Panel width, position, padding, corner radius, the stack limit and the text colours are
  compile-time constants.
- No icon rendering: `app_icon` is accepted and ignored.
- No markup: `body-markup` is not advertised, so clients send plain text.
- No action buttons: `actions` is accepted, `ActionInvoked` is never emitted.
- No grouping or notification history.
- No animation tuning beyond on and off: no duration, easing or distance settings.
- No scale or spring: panels slide and fade, they do not grow into place.
- No light theme. The text inks are constants and are not configurable, so a light
  `background.color` gives white-on-light text and poor contrast. No `color-scheme` detection.
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

Four consequences worth knowing:

- **A panel is drawn only from `configure`.** The compositor's suggested size is the single
  source of truth for surface state, so a buffer can never be attached with stale geometry.
- **Replacing a notification rebuilds its surface.** A commit without a new buffer does not
  repaint, so a fresh surface is the reliable way to get a fresh `configure`.
- **One `draw` serves both the compositor and the animation.** A card is painted from
  `configure`, which is the only place its size comes from, and from the frame timer. A frame
  that cannot be painted is retried rather than treated as complete, because a card parked
  halfway through its own fade would stay that way.
- **A departing card leaves the stack before it stops moving.** It keeps its place while it
  fades out, so the panels below it close up immediately, and its surface is dropped only
  once the exit has finished.
- **Configuration is read once, at startup.** It is parsed into a `Config` that the renderer
  then reads as plain values. There is no reload, so editing the file needs a restart.

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
- Configuration is covered by unit tests for colour shorthand, partial files, and the
  rejection of unknown keys, plus tests asserting that a missing file falls back to the
  bundled `data/config.toml`, that a user file overrides it, and that the bundled values are
  still the pre-configuration look.
- The animation is covered by unit tests for the easing curve and the way progress maps to
  pixels: nothing is visible at 0, the panel is a half-width to the right at 0.5, and a
  settled panel is painted exactly as it was before animations existed.
- Measured against a headless sway with the protocol traced: 17 ms between frames (60 Hz),
  roughly 15 frames to arrive and 12 to leave, and with `enabled = false` three paints in
  2 ms and a quarter of the CPU. Four panels animating at once allocate no extra buffers,
  leak no surfaces, and log no `wl_display` errors.

## License

MIT.
