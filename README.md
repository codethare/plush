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

plush dismiss        # close the notification on top of the stack
plush dismiss all    # close every notification on screen
plush --help         # print the usage
plush --version      # print the version
```

Click a panel to dismiss it, or use `plush dismiss`. Both announce
`NotificationClosed` with reason `2`, so a client hears about it either way.

`plush` with no arguments runs the daemon. A typo is an error rather than a second
daemon: anything other than the commands above prints the usage and exits `2`. The usage
goes to stdout from `--help`, and to stderr after an error.

`plush dismiss --help` prints the help for that subcommand rather than the top-level usage.

A `--` ends flag parsing, so everything after it is a word rather than a flag. That leaves
`plush -- dismiss` and `plush dismiss -- all` both working, and it means `plush -- --help`
is an unknown command rather than a request for help — the usual meaning of the separator.
Since neither command has an argument that could be mistaken for a flag, `--` is only ever
needed to be explicit about that.

`--help` and `--version` never touch Wayland or D-Bus, so they work over ssh and in a
script without a compositor.

The two `dismiss` commands talk to the running daemon over D-Bus, so they do not need to
be started from the same place. If nothing is running the call fails with a
`ServiceUnknown` error — unless the D-Bus activation file from the install step is in
place, in which case the bus starts the daemon first and there is then nothing to dismiss.

## Behaviour

| | |
|---|---|
| Position | `top-right` by default; 32 px inside the edge it is anchored to, and clear of anything a bar or a dock has claimed there; the drop shadow lives inside that gap |
| Panel | 380 px wide, 12 px corner radius, 16 px padding, a hairline edge and a drop shadow |
| Dense output | drawn at the output's integer scale, so the panel keeps its logical size and its pixels are the output's own; fractional scaling uses the integer scale the compositor reports |
| Icon | the `image-data` hint, fitted into a 32 px slot to the left of the text and never enlarged; `image-path` and `app_icon` are ignored |
| Stack | up to 4 panels, 10 px apart, newest on top |
| Max height | 320 px; longer bodies are clipped |
| Expiry | per-notification; `-1` becomes 5000 ms, `0` never expires |
| Hold | a pointer inside a panel pauses its timeout; leaving resumes with the time that was left |
| Countdown | a 2 px line along one edge showing how much of the timeout is left — inside the bottom edge unless `[countdown] position` moves it; a notification that never expires has none |
| Animation | slides in from the nearest edge and fades up over 220 ms; the same motion reversed on the way out |
| `replaces_id` | replaces the content in place and restarts the timeout |
| Dismiss | pointer press, `CloseNotification`, or `plush dismiss` |
| Closing reasons | `1` expired, `2` dismissed by user, `3` `CloseNotification` |
| Control interface | `org.plush.Control` at `/org/plush/Control`, methods `Dismiss` and `DismissAll` |
| `GetCapabilities` | `["body"]` |
| `GetServerInformation` | `plush`, `plush`, version, `1.2` |

## Configuration

plush reads `$XDG_CONFIG_HOME/plush/config.toml`, which is `~/.config/plush/config.toml`
unless `XDG_CONFIG_HOME` has been moved. **There is no need to create it:** with no user file
plush runs the bundled `data/config.toml`, which is the default look. Copy that file across
only when you want to change something.

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

### `[panel]`

| Key | Default | |
|---|---|---|
| `position` | `"top-right"` | `top-left`, `bottom-left`, `top-center`, `bottom-center`, `top-right` or `bottom-right`. |

The stack is measured from the edges the position names, 16 px in, and grows away from them:
a top position stacks downwards and a bottom one upwards. The centred columns are anchored
to no side at all, which is what makes the compositor centre them horizontally.

Those edges are the edges of the space the compositor leaves free, not of the screen. A panel
claims no space for itself and asks to be moved clear of whatever has already claimed some, so on
an output with a 32 px bar the first panel of a top position starts 48 px down, and a maximized
window still covers the whole output underneath it. The compositor is the one that knows, and it
keeps the panel against the boundary when the bar is hidden, shown or resized.

A panel arrives from the edge its position is nearest, so the direction follows the corner:

| Position | Arrives from | Motion |
|---|---|---|
| `top-left`, `bottom-left` | the left edge | slides right |
| `top-right`, `bottom-right` | the right edge | slides left |
| `top-center` | above | drops down |
| `bottom-center` | below | rises up |

### `[border]`

| Key | Default | |
|---|---|---|
| `width` | `1.0` | Stroke width in buffer pixels, so the default stays one device pixel thin at any density. `0` draws no hairline. |
| `color` | `"#ffffff22"` | The hairline's lit ink: solid along the top edge, fading out over the panel height over a fixed dark underside. |

### `[background]`

| Key | Default | |
|---|---|---|
| `color` | `"#1e1e1ed4"` | A translucent dark grey. |

### `[countdown]`

| Key | Default | |
|---|---|---|
| `position` | `"bottom"` | The panel edge the countdown line runs along. `top` and `bottom` are horizontal bars, `left` and `right` vertical ones; it drains from the far end of that edge, so a horizontal bar starts at the left and a vertical one at the top. |

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
| `enabled` | `true` | Slide and fade each panel in from the nearest edge, and back out when it goes away. |

A panel arrives over 220 ms: it slides in from just past the nearest edge while fading
up, and eases out so it leaves the edge quickly and settles gently. Which edge that is
follows `[panel] position`. Dismissing or expiring a panel runs the same motion
backwards. The duration is fixed; there is no speed setting.

The surface itself never moves. `zwlr_layer_shell_v1` only honours margins on edges you
have anchored to, and sliding in from off-screen would need a negative margin, which the
protocol never promises — so the slide is drawn inside the buffer and works the same on
every compositor. Frames are driven by a timer at 60 Hz that only runs while something is
moving; with `enabled = false` panels simply appear. A panel with a timeout is repainted
about twenty times a second while its countdown line drains, with or without animation.

## Deliberately not implemented

- No avoidance of a surface that reserves nothing: placement follows the space the compositor has
  left free, and `zwlr_layer_shell_v1` reports no geometry of any other client, so a bar that never
  asks for an exclusive zone can still be covered. A bar anchored to a corner only is in the same
  group: the protocol treats a positive zone on a corner anchor as zero, so it reserves nothing
  either.
- No configuration beyond the position, the border, the background, the font, the countdown
  edge and the animation toggle. Panel width, padding, corner radius, the stack limit, the
  text colours and the icon size are compile-time constants.
- No icon from `image-path`, from `app_icon` or from an icon theme: only the pixels a client
  puts in `image-data` are drawn, so an app that sends a path or a theme name still gets a
  text-only panel. An `image-data` hint is taken as 8 bits per sample, 3 or 4 channels, and
  at most 512 px on a side.
- No markup: `body-markup` is not advertised, so clients send plain text.
- No action buttons: `actions` is accepted, `ActionInvoked` is never emitted.
- No grouping or notification history.
- No animation tuning beyond on and off: no duration, easing or distance settings.
- No scale or spring: panels slide and fade, they do not grow into place.
- No light theme. The text inks are constants and are not configurable, so a light
  `background.color` gives white-on-light text and poor contrast. No `color-scheme` detection.
- No keyboard handling: panels never take focus.
- No hover highlight and no close button: the pointer holds a panel open and a press
  dismisses it, and that is all it does.
- Corners are circular arcs, not Apple's continuous-curvature squircles.
- Fractional scaling: panels are drawn at the integer scale the compositor reports, so a
  1.25× or 1.5× output is rendered at 2× and scaled down there rather than natively.

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

Five consequences worth knowing:

- **A panel is drawn only from `configure`.** The compositor's suggested size is the single
  source of truth for surface state, so a buffer can never be attached with stale geometry.
- **A density change stops painting until the next `configure`.** Moving a card to an
  output with a different scale resizes its surface and advertises the new density; a buffer
  whose size disagrees with the configure size times that density is a protocol error, and the
  compositor destroys the surface, so nothing is painted until the new size arrives.
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
- **The CLI is a separate process, not a client of the queue.** `plush dismiss` opens its own
  connection and calls `org.plush.Control`, so it works from anywhere in the session and needs
  no socket of its own. The daemon then retires those notifications through exactly the same
  path a pointer press takes, which is why they animate away rather than vanishing.

Text is shaped with `cosmic-text` and rasterised through `SwashCache` by hand. The glyph
path deliberately avoids `cosmic_text::Buffer::draw`, which hides the coverage and
premultiplication convention; `plush` blends coverage itself and swaps red and blue at the
end, because `wl_shm`'s `Argb8888` is BGRA in memory while `tiny-skia` paints RGBA.

## Verified

- Builds and passes `cargo clippy --all-targets` with no warnings.
- Panel placement is measured against a headless sway with a `swaybar` on top: the panel's top
  edge lands at the bar's height plus 16 px, follows the bar when it is removed, restored or made
  taller, and is still created exactly once for the whole run — one `get_layer_surface` in the
  protocol trace — so nothing is re-created to move it. The arrangement that makes this work was
  read from the source of all three compositors this runs on (sway's `arrange_layers`, river's
  `LayerShellOutput`, niri's smithay `LayerMap`). sway and river 0.5 with `tailrace` were measured;
  niri could not be started on the machine this was measured on (no headless backend, no DRM
  device), so its half rests on that reading.
- The same session checks what a panel does not do: with four panels on screen a maximized window
  still covers the full output underneath them, and `bottom-right` and `top-center` are unmoved —
  the first 16 px off the bottom of the free area with its stack 10 px apart, the second centred
  horizontally on the output.
- End-to-end against a headless sway: `notify-send`, `replaces_id` replacement,
  `CloseNotification`, and timeout expiry all behave, with zero `wl_display` errors in the
  Wayland protocol trace.
- Panel pixels are covered by unit tests asserting the rounded corners, the opaque
  interior, and that a title never falls back to a second font mid-run.
- The dense path is covered headless at 1× and 2×: the buffer doubles, the shadow margin
  and corner radius double with it, the text is rasterised at the buffer density, the
  hairline stays one physical pixel, and the height clamp doubles. The compositor side of
  it — `set_buffer_scale`, the configure that follows, and where the pointer lands — is
  not yet measured against a real HiDPI output.
- Icons and the countdown line are covered by unit tests for the slot geometry, the aspect
  ratio, the vertical centring, the wrap width beside a slot, the length of the line against
  the timeout, the line on each of its four edges draining from the far end, and that
  neither is drawn without a hint or without a timeout — plus the
  `image-data` parse, including every shape of hint that has to be refused rather than
  guessed at.
- The hold is covered by unit tests for the remaining time, the resumption with what was
  left rather than the client's timeout again, a hold that never extends a panel's life, and
  a hold on a panel that never expires.
- End to end against a headless sway, with the panels read back off the screen: a 64×64
  `image-data` hint paints a 32×32 icon inside the slot, a hint that is a string or a
  structure of three fields is refused without disturbing the panel, a `replaces_id` that
  changes only the icon swaps that icon in place on a panel that keeps its size, a `-t 0`
  notification draws no countdown line and a timed one drains from its right end, and a
  session of all of it logs no `wl_display` error.
- Configuration is covered by unit tests for colour shorthand, partial files, and the
  rejection of unknown keys, plus tests asserting that a missing file falls back to the
  bundled `data/config.toml`, that a user file overrides it, and that the bundled values are
  still the pre-configuration look.
- The animation is covered by unit tests for the easing curve and the way progress maps to
  pixels: nothing is visible at 0, the panel is a half-width to the right at 0.5, and a
  settled panel is painted exactly as it was before animations existed.
- Measured against a headless sway with the protocol traced: 17 ms between frames (60 Hz),
  roughly 15 frames to arrive and 12 to leave, and with `enabled = false` three paints in
  2 ms and a quarter of the CPU for a panel that never expires, against about twenty paints
  a second while a countdown line is draining. Four panels animating at once allocate no
  extra buffers, leak no surfaces, and log no `wl_display` errors.
- The CLI is exercised end to end against a live daemon: `plush dismiss` closes exactly the
  top panel and `plush dismiss all` the rest, each announced as reason `2`; the surfaces are
  painted and destroyed without a `wl_display` error, dismissing a panel mid-animation works,
  an empty queue is quiet, a typo exits `2` without starting a second daemon, and with no
  daemon the call reports `ServiceUnknown` rather than panicking.
- `--help` and `--version` are covered by unit tests asserting they are commands in their own
  right, that the usage text lists every command, and that they still work when arguments
  follow them. `--` is covered by tests for both sides of the terminator, including that a
  `--help` after it is a word rather than the flag.

## License

MIT.
