//! Painting: Apple's dark "system" palette, rounded panels, text through cosmic-text.
//!
//! Everything here is pure pixel work. The glyph path deliberately does not use
//! `cosmic_text::Buffer::draw`: that callback hides the coverage/premultiplication
//! convention, and the canvas has to end up premultiplied *and* channel-swapped.

use cosmic_text::{Attrs, Buffer, FontSystem, Shaping, SwashCache, SwashContent, SwashImage};
use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, LinearGradient, Paint, Path, PathBuilder,
    Pattern, PixmapMut, PixmapRef, Point, SpreadMode, Stroke, Transform,
};

use crate::{
    config::{Config, Edge, Rgba},
    dbus::{Icon, Notification},
};

/// Panel width in logical pixels.
pub const PANEL_W: u32 = 380;
/// Tallest panel plush will draw; longer bodies are clipped.
pub const MAX_H: u32 = 320;
/// Cards kept on screen. Older ones fall off the stack.
pub const MAX_CARDS: usize = 4;

/// Space reserved around the panel for its drop shadow, in logical pixels. The buffer
/// and the layer surface grow by it; the panel keeps its size and screen position.
pub const SHADOW: u32 = 26;

/// The surface a panel of `w`×`h` needs: the panel plus its shadow margin on every side.
/// The only place that ratio is defined, so the buffer, the layer surface and the input
/// region cannot drift apart.
pub const fn surface(w: u32, h: u32) -> (u32, u32) {
    (w + 2 * SHADOW, h + 2 * SHADOW)
}

/// Shadow layers as (blur radius, alpha at the panel edge, vertical offset), logical px
/// and 0-255. A tight contact shadow under a wide ambient one; the offset is positive
/// because the light comes from above.
const SHADOW_TIGHT: (u32, u32, u32) = (9, 120, 2);
const SHADOW_WIDE: (u32, u32, u32) = (20, 95, 6);
// The margin must fit the widest layer, or its falloff is cut off at the buffer edge.
const _: () = assert!(SHADOW >= SHADOW_WIDE.0 + SHADOW_WIDE.2);

/// How far the shadow reaches above a panel, in logical pixels: the widest layer's blur
/// less the downward offset that keeps it out of the panel itself. Nothing else needs to
/// know, but the gap between stacked cards has to clear it.
pub const SHADOW_REACH_UP: u32 = SHADOW_WIDE.0 - SHADOW_WIDE.2;

/// The rim's dark underside: an inner shadow at the bottom edge, fading up into the sides.
/// Its lit half is the configured border colour, fading down from the top edge.
const BORDER_DARK_ALPHA: u32 = 90;

/// Where each half of the rim peaks, as (position down the panel, share of the peak).
///
/// The lit half holds its strength down the top corner arcs and then dies quickly, so it
/// reads as a specular on glass rather than a wash over the upper half; the dark half stays
/// out of the way until the lower third and is whole at the bottom edge. Between them the
/// edge carries a little of both, which is what keeps it visible against the wallpaper
/// without drawing a line around the panel.
const BORDER_LIT: [(f32, f32); 5] =
    [(0.0, 1.0), (0.16, 0.9), (0.42, 0.55), (0.78, 0.0), (1.0, 0.0)];
const BORDER_DARK: [(f32, f32); 5] =
    [(0.0, 0.08), (0.45, 0.15), (0.72, 0.4), (0.9, 0.85), (1.0, 1.0)];

/// The dark band drawn one stroke inside the rim, same shape as the halves above. It is what
/// makes the lit rim read as the edge of something with thickness: the top of a glass edge
/// has only the wall below it, so the band starts there at nothing and is whole along the
/// bottom edge, where the body shadows its own lower wall.
const BORDER_INNER: [(f32, f32); 5] =
    [(0.0, 0.0), (0.3, 0.05), (0.6, 0.3), (0.85, 0.8), (1.0, 1.0)];
const BORDER_INNER_ALPHA: u32 = 130;

const PAD: f32 = 16.0;
const RADIUS: f32 = 12.0;
const MIN_H: u32 = 60;
/// The icon slot, and the space between it and the text.
const ICON: f32 = 32.0;
const ICON_GAP: f32 = 12.0;
/// The countdown line: a hairline of white that reads without competing with the text.
const COUNTDOWN_H: f32 = 2.0;
const COUNTDOWN_INK: Rgba = Rgba(255, 255, 255, 110);

const INK_PRIMARY: cosmic_text::Color = cosmic_text::Color::rgba(255, 255, 255, 245);
const INK_SECONDARY: cosmic_text::Color = cosmic_text::Color::rgba(235, 235, 245, 155);
const INK_TERTIARY: cosmic_text::Color = cosmic_text::Color::rgba(235, 235, 245, 115);

fn text_width(has_icon: bool) -> f32 {
    PANEL_W as f32 - PAD - text_x(has_icon)
}

/// Left edge of the text: the padding, or the far side of the icon slot.
fn text_x(has_icon: bool) -> f32 {
    PAD + if has_icon { ICON + ICON_GAP } else { 0.0 }
}

/// Shapes a notification into a wrapped, styled buffer, at the output's density.
///
/// `scale` is the buffer-to-logical density: the buffer and every metric here are in
/// buffer pixels, so the layout wraps at the same words on a dense output.
pub fn build(
    font_system: &mut FontSystem,
    config: &Config,
    scale: u32,
    notification: &Notification,
) -> Buffer {
    let density = scale.max(1) as f32;
    let font = &config.font;
    let mut buffer = Buffer::new(font_system, font.body_metrics().scale(density));
    buffer.set_size(
        Some(text_width(notification.icon.is_some()) * density),
        Some((MAX_H as f32 - 2.0 * PAD) * density),
    );

    let mut parts: Vec<(String, Attrs)> = Vec::new();
    if !notification.app_name.is_empty() {
        parts.push((
            notification.app_name.clone(),
            Attrs::new()
                .family(font.family())
                .metrics(font.app_metrics().scale(density))
                .color(INK_TERTIARY)
                .weight(font.app_weight.0),
        ));
    }
    parts.push((
        notification.summary.clone(),
        Attrs::new()
            .family(font.family())
            .metrics(font.title_metrics().scale(density))
            .color(INK_PRIMARY)
            .weight(font.title_weight.0),
    ));
    if !notification.body.is_empty() {
        parts.push((
            notification.body.clone(),
            Attrs::new()
                .family(font.family())
                .metrics(font.body_metrics().scale(density))
                .color(INK_SECONDARY)
                .weight(font.body_weight.0),
        ));
    }

    let last = parts.len().saturating_sub(1);
    for (i, (text, _)) in parts.iter_mut().enumerate() {
        if i < last {
            text.push('\n');
        }
    }

    let spans: Vec<(&str, Attrs)> = parts
        .iter()
        .map(|(text, attrs)| (text.as_str(), attrs.clone()))
        .collect();
    buffer.set_rich_text(
        spans,
        &Attrs::new().family(font.family()).metrics(font.body_metrics().scale(density)),
        Shaping::Advanced,
        None,
    );
    buffer
}

/// Panel height in buffer pixels needed by an already-`build`t buffer.
///
/// `has_icon` is passed in because the slot and its padding are taller than a one-line
/// panel would otherwise be, so a short notification with an icon is taller than the same
/// text without one.
pub fn card_height(
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    has_icon: bool,
    scale: u32,
) -> u32 {
    let density = scale.max(1);
    let pad = PAD * density as f32;
    buffer.shape_until_scroll(font_system, false);
    let text: f32 = buffer.layout_runs().map(|run| run.line_height).sum();
    let floor = if has_icon { ((ICON + 2.0 * PAD) * density as f32) as u32 } else { 0 };
    ((text + 2.0 * pad).ceil() as u32).clamp((MIN_H * density).max(floor), MAX_H * density)
}

/// Renders one panel into `canvas`.
///
/// `progress` is the card's animation state: 0 is a full side off screen and invisible,
/// 1 is settled. Which side is the configured position, so a panel on the left slides
/// right, one on the right slides left, and the centred ones drop in or rise. Cards
/// drawn with animations turned off pass 1.0.
///
/// An `icon` is drawn in its slot to the left of the text, and arrives with the panel:
/// it is offset, and faded, with everything else.
///
/// `remaining` is how much of the notification's timeout is left, which is the length of
/// the countdown line; `None` for a notification that never expires, which has no line.
///
/// `scale` is the buffer-to-logical density: the buffer is `width`×`height` buffer pixels
/// and every constant below is logical, so they are multiplied by it here.
#[allow(clippy::too_many_arguments)]
pub fn paint(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    config: &Config,
    icon: Option<&Icon>,
    font_system: &mut FontSystem,
    swash: &mut SwashCache,
    buffer: &mut Buffer,
    progress: f32,
    remaining: Option<f32>,
    scale: u32,
) {
    let pixels = &mut canvas[..width as usize * height as usize * 4];
    let progress = progress.clamp(0.0, 1.0);
    let density = scale.max(1);

    panel(pixels, width, height, config, density);
    if let Some(image) = icon {
        draw_icon(pixels, width, height, image, density);
    }
    if let Some(remaining) = remaining {
        countdown(pixels, width, height, config, remaining, density);
    }
    text(pixels, width, height, font_system, swash, buffer, icon.is_some(), density);
    // wl_shm's Argb8888 is BGRA in memory on little-endian; tiny-skia draws RGBA.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    // The card travels its own side, so at progress 0 it sits entirely outside the buffer.
    // It is drawn at rest inside its shadow margin and then slid out: chrome, text and all
    // move together, and what leaves the buffer is clipped at the edge.
    let edge = config.panel.position.edge();
    let travel = (1.0 - progress)
        * match edge {
            Edge::Left | Edge::Right => width as f32,
            Edge::Top | Edge::Bottom => height as f32,
        };
    let (dx, dy) = match edge {
        Edge::Left => (-travel, 0.0),
        Edge::Right => (travel, 0.0),
        Edge::Top => (0.0, -travel),
        Edge::Bottom => (0.0, travel),
    };
    shift(pixels, width, height, dx.round() as i32, dy.round() as i32);
    // The buffer is premultiplied, so fading it means scaling every channel alike.
    if progress < 1.0 {
        let alpha = (progress * 255.0) as u32;
        for pixel in pixels.chunks_exact_mut(4) {
            for channel in pixel.iter_mut() {
                *channel = (*channel as u32 * alpha / 255) as u8;
            }
        }
    }
}

/// The card's chrome: the two shadow layers outside the panel, then its translucent
/// body and the hairline that joins the body to the shadow.
fn panel(pixels: &mut [u8], width: u32, height: u32, config: &Config, density: u32) {
    pixels.fill(0);

    let margin = SHADOW * density;
    let radius = RADIUS * density as f32;
    let x = margin as f32;
    let (w, h) = (width as f32 - 2.0 * x, height as f32 - 2.0 * x);
    shadow(
        pixels,
        width,
        height,
        &Panel { x: margin, y: margin, w: w as u32, h: h as u32, r: radius as u32 },
        density,
    );

    let Some(mut pixmap) = PixmapMut::from_bytes(pixels, width, height) else {
        return;
    };
    let background = config.background.color;
    let mut fill = Paint::default();
    fill.set_color_rgba8(background.0, background.1, background.2, background.3);
    fill.anti_alias = true;
    pixmap.fill_path(
        &rounded_rect(x, x, w, h, radius),
        &fill,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // The hairline is measured in buffer pixels, not logical ones: it stays one device
    // pixel thin on a dense output, which is what an edge highlight wants there.
    let stroke_width = config.border.width;
    // Zero means no hairline, and tiny-skia would draw a one-pixel stroke for it
    // anyway. A stroke at least as wide as the panel would invert the inset path.
    if stroke_width <= 0.0 || stroke_width >= w.min(h) {
        return;
    }

    let hairline = Stroke { width: stroke_width, ..Stroke::default() };

    // Three strokes down the panel's height, in the order they are laid: the dark inner
    // shadow at the bottom edge, the light rim, then the shadowed band just inside it.
    // Drawn back to front, so the rim covers the seam where the translucent body meets
    // the drop shadow instead of the other way round.
    let bg = config.border.color;
    for (step, curve, peak, ink) in [
        (0.0, &BORDER_DARK[..], BORDER_DARK_ALPHA, Rgba(0, 0, 0, 0)),
        (0.0, &BORDER_LIT[..], bg.3 as u32, bg),
        (stroke_width, &BORDER_INNER[..], BORDER_INNER_ALPHA, Rgba(0, 0, 0, 0)),
    ] {
        if peak == 0 {
            continue;
        }
        // `step` is how far in from the rim this stroke's centre runs, in stroke widths.
        let d = stroke_width + step;
        let outline = rounded_rect(
            x + d / 2.0,
            x + d / 2.0,
            w - d,
            h - d,
            (radius - d / 2.0).max(0.0),
        );
        let stops = curve
            .iter()
            .map(|(at, share)| {
                GradientStop::new(
                    *at,
                    Color::from_rgba8(ink.0, ink.1, ink.2, (peak as f32 * share).round() as u8),
                )
            })
            .collect();
        let Some(shader) = LinearGradient::new(
            Point::from_xy(0.0, x),
            Point::from_xy(0.0, x + h),
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        ) else {
            return;
        };
        let paint = Paint { shader, anti_alias: true, ..Paint::default() };
        pixmap.stroke_path(&outline, &paint, &hairline, Transform::identity(), None);
    }
}

/// The panel inside the shadow margin: origin and size in buffer px, plus the rounded
/// outline the shadow grows out of.
struct Panel {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    r: u32,
}

/// Both drop-shadow layers in one pass. They share the panel outline, so each pixel gets
/// its alpha from both falloffs and is written once. The buffer arrives cleared, so only
/// the band the layers can reach is touched.
fn shadow(buf: &mut [u8], w: u32, h: u32, panel: &Panel, density: u32) {
    // (blur, vertical offset, alpha by squared distance)
    let layers: [(u32, i64, Vec<u32>); 2] = [SHADOW_WIDE, SHADOW_TIGHT].map(|(blur, a0, offset)| {
        let b = (blur * density).max(1);
        let offset = (offset * density) as i64;
        let lut = (0..=b * b)
            .map(|d2| {
                // A blurred edge starts at half the peak and eases out: a ramp from the
                // full value leaves a visible cusp where it begins.
                let t = (d2 as f64).sqrt() / b as f64;
                (a0 as f64 * 0.5 * (1.0 + (std::f64::consts::PI * t).cos())).round() as u32
            })
            .collect();
        (b, offset, lut)
    });
    let reach = layers.iter().map(|l| l.0).max().unwrap_or(1);
    let drop = layers.iter().map(|l| l.1.max(0) as u32).max().unwrap_or(0);

    let half_w = panel.w as f32 / 2.0;
    let half_h = panel.h as f32 / 2.0;
    let radius = (panel.r as f32).min(half_w).min(half_h);
    let (cx, cy) = (panel.x as f32 + half_w, panel.y as f32 + half_h);

    let x0 = panel.x.saturating_sub(reach);
    let x1 = (panel.x + panel.w + reach).min(w);
    let y0 = panel.y.saturating_sub(reach);
    let y1 = (panel.y + panel.h + reach + drop).min(h);

    // Alpha by horizontal position, rebuilt only when the row's geometry changes: every
    // row level with the straight part of both layers shares one profile. `inked` is the
    // span of it that holds anything, so the rows themselves copy only the two bands.
    let mut band = vec![0u8; (x1 - x0) as usize];
    let mut inked = 0..0;
    let mut cached = [u32::MAX; 2];

    for y in y0..y1 {
        let py = y as f32 + 0.5;
        // Per layer: this row's distance from the shifted outline's centre line. When it
        // is at most the straight half, the distance to the outline is horizontal alone.
        let row: [(f32, u32, &Vec<u32>); 2] = [0, 1].map(|k| {
            let (blur, offset, lut) = &layers[k];
            let qy = (py - cy - *offset as f32).abs() - (half_h - radius);
            (qy, *blur, lut)
        });

        let key = [row[0].0.to_bits(), row[1].0.to_bits()];
        if key != cached {
            cached = key;
            // The panel's own outline: the body is translucent, so nothing may be painted
            // underneath it. Only the layers' shifted outlines reach past it.
            let panel_qy = (py - cy).abs() - (half_h - radius);
            let mut first = band.len();
            let mut last = 0;
            for (i, out) in band.iter_mut().enumerate() {
                let qx = (x0 as f32 + i as f32 + 0.5 - cx).abs() - (half_w - radius);
                // The body's edge is antialiased, so its coverage fades in over the last
                // pixel and the shadow has to fade out over the same one: gating the two on
                // "is this pixel inside the outline" leaves the partly covered pixels along
                // a corner arc with nothing behind them, and a light background shows through
                // as a fleck. A whole pixel inside is still left to the body alone, because
                // the body is translucent.
                let cover = (outline_gap(qx, panel_qy, radius) + 1.0).clamp(0.0, 1.0);
                let a = if cover <= 0.0 {
                    0
                } else {
                    let mut a = 0u32;
                    for (qy, blur, lut) in row {
                        // Each layer is that same outline shifted down, so the light reads
                        // as coming from above; inside a shifted outline it is at peak.
                        let d = outline_gap(qx, qy, radius).max(0.0);
                        let la = lut[((d * d) as u32).min(blur * blur) as usize];
                        a = la + div255(a * (255 - la));
                    }
                    (a as f32 * cover).round() as u8
                };
                if a > 0 {
                    first = first.min(i);
                    last = i;
                }
                *out = a;
            }
            inked = if last >= first { first..last + 1 } else { 0..0 };
        }

        let row_start = y as usize * w as usize + x0 as usize + inked.start;
        let dst = &mut buf[row_start * 4..(row_start + inked.len()) * 4];
        for (pixel, a) in dst.chunks_exact_mut(4).zip(&band[inked.clone()]) {
            pixel.copy_from_slice(&[0, 0, 0, *a]);
        }
    }
}

/// Signed distance from a rounded rectangle's outline, given the horizontal and vertical
/// gaps to where its corner arcs begin: negative inside.
fn outline_gap(qx: f32, qy: f32, radius: f32) -> f32 {
    if qy <= 0.0 {
        qx - radius
    } else if qx <= 0.0 {
        qy - radius
    } else {
        (qx * qx + qy * qy).sqrt() - radius
    }
}

fn div255(x: u32) -> u32 {
    (x + 1 + (x >> 8)) >> 8
}

/// Moves the whole frame by `(dx, dy)` buffer pixels, leaving transparent behind. The card
/// slides out of its own surface, so whatever leaves the buffer is clipped away.
fn shift(pixels: &mut [u8], width: u32, height: u32, dx: i32, dy: i32) {
    let (w, h) = (width as i32, height as i32);
    if dy >= h || dy <= -h {
        pixels.fill(0);
        return;
    }
    // Rows first: whole rows move, then the rows they leave are blanked.
    let pitch = w as usize * 4;
    let (src, dst) = match dy {
        1.. => (0..pixels.len() - dy as usize * pitch, dy as usize * pitch),
        ..=-1 => ((-dy) as usize * pitch..pixels.len(), 0),
        0 => (0..pixels.len(), 0),
    };
    pixels.copy_within(src, dst);
    let blanked = match dy {
        1.. => 0..dy as usize * pitch,
        ..=-1 => pixels.len() - (-dy) as usize * pitch..pixels.len(),
        0 => 0..0,
    };
    pixels[blanked].fill(0);

    for y in 0..h {
        let row = &mut pixels[y as usize * pitch..(y as usize + 1) * pitch];
        match dx {
            dx if dx >= w || dx <= -w => row.fill(0),
            dx if dx > 0 => {
                let gap = dx as usize * 4;
                row.copy_within(0..pitch - gap, gap);
                row[..gap].fill(0);
            }
            dx if dx < 0 => {
                let gap = (-dx) as usize * 4;
                row.copy_within(gap..pitch, 0);
                row[pitch - gap..].fill(0);
            }
            _ => {}
        }
    }
}

/// Draws the icon in its slot: scaled to fit, never enlarged, and centred vertically in
/// the panel. The scaling is a shader, so there is no resampler to keep correct here.
fn draw_icon(pixels: &mut [u8], width: u32, height: u32, image: &Icon, density: u32) {
    let Some(mut pixmap) = PixmapMut::from_bytes(pixels, width, height) else {
        return;
    };
    let Some(source) = PixmapRef::from_bytes(&image.rgba, image.width, image.height) else {
        return;
    };

    let slot = ICON * density as f32;
    let scale = (slot / image.width as f32).min(slot / image.height as f32).min(1.0);
    let (w, h) = (image.width as f32 * scale, image.height as f32 * scale);
    // The panel is centred in its buffer, so the slot is centred the same way.
    let (x, y) = (((SHADOW as f32) + PAD) * density as f32, (height as f32 - h) / 2.0);

    let paint = Paint {
        shader: Pattern::new(
            source,
            SpreadMode::Pad,
            FilterQuality::Bilinear,
            1.0,
            Transform::from_scale(scale, scale).post_translate(x, y),
        ),
        ..Default::default()
    };
    pixmap.fill_path(
        &rect(x, y, w, h),
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

/// An axis-aligned rectangle, which is all an icon slot needs.
fn rect(x: f32, y: f32, w: f32, h: f32) -> Path {
    let mut path = PathBuilder::new();
    path.move_to(x, y);
    path.line_to(x + w, y);
    path.line_to(x + w, y + h);
    path.line_to(x, y + h);
    path.close();
    path.finish().expect("a rectangle is a valid path")
}

/// The countdown: a bar inside the bottom edge, its length the fraction of the timeout
/// that is left, shortening from a fixed left end as the notification runs out.
///
/// It runs between the rounded corners rather than across the whole panel, because a
/// straight bar in the bottom two rows would otherwise paint over the corner that gets
/// rounded away.
fn countdown(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    config: &Config,
    remaining: f32,
    density: u32,
) {
    if remaining <= 0.0 {
        return;
    }
    let Some(mut pixmap) = PixmapMut::from_bytes(pixels, width, height) else {
        return;
    };

    let density = density as f32;
    let margin = SHADOW as f32 * density;
    let radius = RADIUS * density;
    let thickness = COUNTDOWN_H * density;
    let border = config.border.width;
    // The line hugs its edge, inset by the hairline so it never paints over it, and by the
    // corner radius along it so it never crosses the rounded corners.
    let position = config.countdown.position;
    let (x, y, w, h) = if matches!(position, Edge::Top | Edge::Bottom) {
        let span = (width as f32 - 2.0 * margin - 2.0 * radius) * remaining.min(1.0);
        let y = if position == Edge::Top {
            margin + border
        } else {
            height as f32 - margin - border - thickness
        };
        (margin + radius, y, span, thickness)
    } else {
        let span = (height as f32 - 2.0 * margin - 2.0 * radius) * remaining.min(1.0);
        let x = if position == Edge::Left {
            margin + border
        } else {
            width as f32 - margin - border - thickness
        };
        (x, margin + radius, thickness, span)
    };
    let mut ink = Paint::default();
    ink.set_color_rgba8(
        COUNTDOWN_INK.0,
        COUNTDOWN_INK.1,
        COUNTDOWN_INK.2,
        COUNTDOWN_INK.3,
    );
    ink.anti_alias = true;
    pixmap.fill_path(
        &rect(x, y, w, h),
        &ink,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

#[allow(clippy::too_many_arguments)]
fn text(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    font_system: &mut FontSystem,
    swash: &mut SwashCache,
    buffer: &mut Buffer,
    has_icon: bool,
    density: u32,
) {
    // The metrics were shaped at this density, so the layout is already in buffer pixels
    // and the glyphs are rasterised at it; only the fixed insets are still logical.
    let margin = SHADOW as f32 * density as f32;
    let inset = text_x(has_icon) * density as f32;
    buffer.shape_until_scroll(font_system, false);
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let Some(color) = glyph.color_opt else {
                continue;
            };
            let origin = glyph.physical(
                (margin + inset, margin + PAD * density as f32 + run.line_y),
                1.0,
            );
            let Some(image) = swash.get_image(font_system, origin.cache_key) else {
                continue;
            };
            blit(
                pixels,
                width,
                height,
                origin.x + image.placement.left,
                origin.y - image.placement.top,
                image,
                color,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn blit(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    image: &SwashImage,
    color: cosmic_text::Color,
) {
    let (red, green, blue, alpha) = color.as_rgba_tuple();
    let (image_w, image_h) = (image.placement.width as i32, image.placement.height as i32);

    for row in 0..image_h {
        let pixel_y = y + row;
        if pixel_y < 0 || pixel_y >= height as i32 {
            continue;
        }
        for column in 0..image_w {
            let pixel_x = x + column;
            if pixel_x < 0 || pixel_x >= width as i32 {
                continue;
            }

            let index = (row * image_w + column) as usize;
            let (r, g, b, a) = match image.content {
                SwashContent::Mask => {
                    let coverage = image.data[index] as u32 * alpha as u32 / 255;
                    (red, green, blue, coverage as u8)
                }
                SwashContent::Color => {
                    // swash hands back premultiplied RGBA here.
                    let offset = index * 4;
                    let a = image.data[offset + 3] as u32 * alpha as u32 / 255;
                    (
                        image.data[offset],
                        image.data[offset + 1],
                        image.data[offset + 2],
                        a as u8,
                    )
                }
                SwashContent::SubpixelMask => continue,
            };
            if a == 0 {
                continue;
            }

            let offset = (pixel_y as usize * width as usize + pixel_x as usize) * 4;
            let dst = &mut pixels[offset..offset + 4];
            let inverse = 255 - a as u32;
            dst[0] = (r as u32 * a as u32 / 255 + dst[0] as u32 * inverse / 255) as u8;
            dst[1] = (g as u32 * a as u32 / 255 + dst[1] as u32 * inverse / 255) as u8;
            dst[2] = (b as u32 * a as u32 / 255 + dst[2] as u32 * inverse / 255) as u8;
            dst[3] = (a as u32 + dst[3] as u32 * inverse / 255) as u8;
        }
    }
}

/// Control-point offset that turns a cubic into a quarter circle: `4/3·tan(π/8)`.
const KAPPA: f32 = 0.552_284_8;

/// A rounded rectangle whose corners are circular arcs, matching the outline the shadow is
/// measured from. A quadratic through the corner point would pull the corner in by half a
/// pixel at this radius, leaving a seam between the body and its shadow.
fn rounded_rect(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Path {
    let r = radius.max(0.0).min(w / 2.0).min(h / 2.0);
    let c = r * KAPPA;
    let mut path = PathBuilder::new();
    path.move_to(x + r, y);
    path.line_to(x + w - r, y);
    path.cubic_to(x + w - r + c, y, x + w, y + r - c, x + w, y + r);
    path.line_to(x + w, y + h - r);
    path.cubic_to(x + w, y + h - r + c, x + w - r + c, y + h, x + w - r, y + h);
    path.line_to(x + r, y + h);
    path.cubic_to(x + r - c, y + h, x, y + h - r + c, x, y + h - r);
    path.line_to(x, y + r);
    path.cubic_to(x, y + r - c, x + r - c, y, x + r, y);
    path.close();
    path.finish().expect("rounded rect is a valid path")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::Notification;

    fn render_with(config: Config, sample: Notification, progress: f32) -> (Vec<u8>, u32, u32) {
        render_at(config, sample, progress, 1)
    }

    /// The same frame at an output density, for the HiDPI path.
    fn render_at(
        config: Config,
        sample: Notification,
        progress: f32,
        scale: u32,
    ) -> (Vec<u8>, u32, u32) {
        let mut font_system = FontSystem::new();
        let has_icon = sample.icon.is_some();
        let mut buffer = build(&mut font_system, &config, scale, &sample);
        let height = card_height(&mut font_system, &mut buffer, has_icon, scale);
        let (width, height) = surface(PANEL_W, height.div_ceil(scale));
        let (width, height) = (width * scale, height * scale);
        let mut canvas = vec![0u8; width as usize * height as usize * 4];
        let mut swash = SwashCache::new();
        paint(
            &mut canvas,
            width,
            height,
            &config,
            sample.icon.as_ref(),
            &mut font_system,
            &mut swash,
            &mut buffer,
            progress,
            None,
            scale,
        );
        (canvas, width, height)
    }

    /// A panel with a countdown on it and no text, so the line is the only ink on it.
    fn render_countdown(config: Config, remaining: Option<f32>) -> (Vec<u8>, u32, u32) {
        let mut font_system = FontSystem::new();
        let mut buffer = Buffer::new(&mut font_system, config.font.body_metrics());
        let (width, height) = surface(PANEL_W, MIN_H);
        let mut canvas = vec![0u8; width as usize * height as usize * 4];
        let mut swash = SwashCache::new();
        paint(
            &mut canvas,
            width,
            height,
            &config,
            None,
            &mut font_system,
            &mut swash,
            &mut buffer,
            1.0,
            remaining,
            1,
        );
        (canvas, width, height)
    }

    /// A panel with nothing in it but what is drawn on top of it, so an ink box is the
    /// icon and the glyphs and nothing else.
    fn transparent() -> Config {
        transparent_on("bottom")
    }

    /// The same, with the countdown line running along `edge`.
    fn transparent_on(edge: &str) -> Config {
        toml::from_str(&format!(
            "[background]\ncolor = \"#00000000\"\n\n[border]\nwidth = 0.0\n\n\
             [countdown]\nposition = \"{edge}\"\n"
        ))
        .expect("a transparent panel")
    }

    /// An icon of one flat colour, so a pixel can say where it came from.
    fn solid_icon(width: u32, height: u32, pixel: [u8; 4]) -> Icon {
        Icon { width, height, rgba: pixel.repeat((width * height) as usize) }
    }

    /// Bounding box of everything painted on the panel, in panel pixels, as
    /// (left, right, top, bottom). The shadow margin around the panel is skipped, and so
    /// is the shadow in the panel's corner notches (it is premultiplied black), so what it
    /// measures is the content rather than the chrome.
    fn inked_box(canvas: &[u8], width: u32, height: u32) -> Option<(usize, usize, usize, usize)> {
        let (w, h) = (width as usize, height as usize);
        let margin = SHADOW as usize;
        let mut found: Option<(usize, usize, usize, usize)> = None;
        for y in margin..h - margin {
            for x in margin..w - margin {
                let pixel = &canvas[(y * w + x) * 4..][..4];
                if pixel[3] > 0 && pixel[..3].iter().any(|channel| *channel > 0) {
                    let (left, right, top, bottom) =
                        found.unwrap_or((usize::MAX, 0, usize::MAX, 0));
                    found = Some((
                        left.min(x - margin),
                        right.max(x - margin),
                        top.min(y - margin),
                        bottom.max(y - margin),
                    ));
                }
            }
        }
        found
    }

    /// Total alpha, as a stand-in for how visible the card is.
    fn ink(canvas: &[u8]) -> u64 {
        canvas.chunks_exact(4).map(|pixel| u64::from(pixel[3])).sum()
    }

    fn sample() -> Notification {
        Notification {
            id: 1,
            app_name: "Test App".into(),
            summary: "Build finished".into(),
            body: "cargo build --release succeeded in 12.4s after a long enough run".into(),
            timeout: None,
            icon: None,
        }
    }

    /// A copy of the sample with an icon on it, which is what every icon test starts from.
    fn with_icon(icon: Icon) -> Notification {
        Notification { icon: Some(icon), ..sample() }
    }

    #[test]
    fn panel_has_the_expected_shape_and_colours() {
        let (canvas, width, height) = render_with(Config::default(), sample(), 1.0);
        let (w, h) = (width as usize, height as usize);

        let interior = canvas[(h / 2 * w + w / 2) * 4 + 3];
        assert!(interior > 200, "panel interior must be opaque, alpha={interior}");
        for (x, y) in [(0usize, 0usize), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
            let alpha = canvas[(y * w + x) * 4 + 3];
            assert!(alpha < 40, "corner ({x},{y}) must be rounded away, alpha={alpha}");
        }
        assert!(
            canvas
                .chunks_exact(4)
                .any(|p| p[0] > 200 && p[1] > 200 && p[2] > 200),
            "panel must contain light text pixels"
        );
    }

    /// The panel floats inside its shadow margin: the body is inside, the shadow spills
    /// out of it and fades, and the corners of the buffer stay empty.
    #[test]
    fn the_panel_sits_inside_a_soft_shadow() {
        let (canvas, width, height) = render_with(Config::default(), sample(), 1.0);
        let (w, h) = (width as usize, height as usize);
        let mid = h / 2;
        let alpha = |x: usize, y: usize| canvas[(y * w + x) * 4 + 3];

        for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
            assert_eq!(alpha(x, y), 0, "corner ({x},{y}) must stay empty");
        }

        let body = alpha(SHADOW as usize + 2, mid);
        assert!(body > 200, "the panel body is on the panel, alpha={body}");
        let near = alpha(SHADOW as usize - 1, mid);
        let far = alpha(SHADOW as usize - SHADOW_WIDE.0 as usize + 1, mid);
        assert!((1..255).contains(&near), "just outside is shadow, alpha={near}");
        assert!(near > far, "and it fades away from the panel: {near} then {far}");
    }

    /// The shadow wraps a corner instead of being cut off at it: measured straight out of
    /// the arc, the corner carries a good share of what the edge beside it carries.
    #[test]
    fn the_shadow_wraps_the_corners() {
        let (canvas, width, height) = render_with(Config::default(), sample(), 1.0);
        let (w, h) = (width as usize, height as usize);
        let margin = SHADOW as usize;
        let alpha = |x: usize, y: usize| canvas[(y * w + x) * 4 + 3];

        let edge = alpha(margin - 4, h / 2);
        // Four pixels out from the arc, on the diagonal through the corner's centre.
        let step = (RADIUS as f64 + 4.0) / 2f64.sqrt();
        let at = (margin as f64 + RADIUS as f64 - step) as usize;
        let corner = alpha(at, at);

        assert!(corner > 0, "the corner has no shadow at all");
        assert!(
            corner * 3 >= edge,
            "the corner carries {corner} against the edge's {edge}"
        );
    }

    /// The body is translucent, so nothing may be painted underneath it: the layer that
    /// hangs below the panel would otherwise darken the strip inside its top edge.
    #[test]
    fn no_shadow_is_painted_under_the_body() {
        let (canvas, width, height) = render_with(Config::default(), sample(), 1.0);
        let (w, _) = (width as usize, height as usize);
        let margin = SHADOW as usize;
        let alpha = |x: usize, y: usize| canvas[(y * w + x) * 4 + 3];
        let column = margin + PANEL_W as usize / 2;

        assert_eq!(
            alpha(column, margin + 1),
            alpha(column, margin + 8),
            "the strip inside the top edge is body, not shadow"
        );
    }

    /// The hairline runs from a lit top edge to a dark inner shadow at the bottom, fading
    /// through both halves of the gradient on the way down, so the translucent body and the
    /// shadow read as one edge rather than as two surfaces meeting.
    #[test]
    fn the_hairline_fades_from_light_to_dark() {
        let (canvas, width, height) = render_with(Config::default(), sample(), 1.0);
        let (w, h) = (width as usize, height as usize);
        let margin = SHADOW as usize;
        let radius = RADIUS as usize;
        let grey = |x: usize, y: usize| canvas[(y * w + x) * 4];

        // Down the straight part of the left edge, clear of the corner arcs.
        let (top_y, bottom_y) = (margin + radius, h - margin - radius);
        let body = grey(w / 2, (top_y + bottom_y) / 2);
        let along: Vec<u8> = (0..=7)
            .map(|step| grey(margin, top_y + (bottom_y - top_y) * step / 7))
            .collect();
        let (top, bottom) = (along[0], along[7]);

        assert!(top > body, "lit from above: {top} against the body's {body}");
        assert!(bottom < body, "and dark underneath: {bottom} against {body}");
        assert!(
            along.windows(2).all(|pair| pair[0] >= pair[1]),
            "the edge fades one way, top to bottom: {along:?}"
        );
        assert!(
            along[3] > body,
            "the sides keep a little light, or the edge vanishes into the wallpaper: \
             {} against {body}",
            along[3]
        );
    }

    /// A dark band one stroke inside the rim makes the lit edge read as glass with
    /// thickness rather than as a line drawn on the panel. It is under the bottom edge, where
    /// the body shadows its own lower wall, and absent at the top, where there is nothing
    /// above the rim to shadow it.
    #[test]
    fn a_dark_band_sits_inside_the_rim_under_the_bottom_edge() {
        let (canvas, width, height) = render_with(Config::default(), sample(), 1.0);
        let (w, h) = (width as usize, height as usize);
        let margin = SHADOW as usize;
        let radius = RADIUS as usize;
        let grey = |x: usize, y: usize| canvas[(y * w + x) * 4];

        // One pixel in from the rim, down the straight part of the left edge, clear of the
        // corner arcs.
        let (top_y, bottom_y) = (margin + radius, h - margin - radius);
        let body = grey(w / 2, (top_y + bottom_y) / 2);
        let near_top = grey(margin + 1, top_y + 2);
        let darkest = (bottom_y - 8..bottom_y).map(|y| grey(margin + 1, y)).min().unwrap();

        assert_eq!(near_top, body, "nothing shadows the top of the wall: {near_top} vs {body}");
        assert!(
            darkest + 4 < body,
            "the lower wall is in shadow: {darkest} against the body's {body}"
        );
    }

    /// The shadow must not drop away where the body starts: the body's edge is antialiased
    /// over one pixel, so if the shadow stops a pixel earlier, that pixel is only partly
    /// covered and a light background shows through it as a fleck along a corner arc.
    /// Crossing the edge, the coverage may rise but may not fall.
    #[test]
    fn the_shadow_does_not_drop_away_at_the_panel_edge() {
        use std::f64::consts::PI;

        let (canvas, width, height) = render_with(Config::default(), sample(), 1.0);
        let (w, h) = (width as usize, height as usize);
        let margin = SHADOW as f64;
        let r = RADIUS as f64;
        let alpha = |x: usize, y: usize| canvas[(y * w + x) * 4 + 3] as i32;

        // (corner centre, the arc's angular range) — screen coordinates, y down.
        let corners = [
            (margin + r, margin + r, PI, 1.5 * PI),
            (w as f64 - margin - r, margin + r, 1.5 * PI, 2.0 * PI),
            (margin + r, h as f64 - margin - r, 0.5 * PI, PI),
            (w as f64 - margin - r, h as f64 - margin - r, 0.0, 0.5 * PI),
        ];
        for (cx, cy, from, to) in corners {
            for step in 0..7 {
                let angle = from + (to - from) * step as f64 / 6.0;
                let (dx, dy) = (angle.cos(), angle.sin());
                let sample = |gap: f64| {
                    let (x, y) = ((cx + dx * (r + gap)) as usize, (cy + dy * (r + gap)) as usize);
                    alpha(x, y)
                };
                let outside = sample(0.6);
                let edge = sample(-0.4);
                assert!(
                    edge + 6 >= outside,
                    "the edge at {angle:.2} rad is {edge} against the shadow's {outside}: \
                     the background shows through"
                );
            }
        }
    }

    /// The shadow fades out inside its own margin, so no layer is cut off at the buffer
    /// edge — which is what the margin exists for.
    #[test]
    fn the_shadow_does_not_reach_the_buffer_edge() {
        let (canvas, width, height) = render_with(Config::default(), sample(), 1.0);
        let (w, h) = (width as usize, height as usize);
        let alpha = |x: usize, y: usize| canvas[(y * w + x) * 4 + 3];

        for x in [0, w - 1] {
            for y in 0..h {
                assert_eq!(alpha(x, y), 0, "the buffer's side column is empty at y={y}");
            }
        }
        for y in [0, h - 1] {
            for x in 0..w {
                assert_eq!(alpha(x, y), 0, "the buffer's top or bottom row is empty at x={x}");
            }
        }
    }

    /// A dense output gets a buffer at its own density: the same panel, twice the pixels,
    /// and every logical constant (shadow margin, radius, padding) doubled with it.
    #[test]
    fn a_dense_output_gets_a_panel_at_its_own_resolution() {
        let one = render_with(Config::default(), sample(), 1.0);
        let two = render_at(Config::default(), sample(), 1.0, 2);

        assert_eq!((two.1, two.2), (2 * one.1, 2 * one.2), "the buffer follows the density");

        let (canvas, width, height) = (two.0, two.1, two.2);
        let (w, h) = (width as usize, height as usize);
        let mid = h / 2;
        let alpha = |x: usize, y: usize| canvas[(y * w + x) * 4 + 3];

        let margin = (SHADOW * 2) as usize;
        assert!(alpha(margin + 2, mid) > 200, "the body is on the panel");
        assert!((1..255).contains(&alpha(margin - 1, mid)), "and the shadow outside it");
        for (x, y) in [(0usize, 0usize), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
            assert_eq!(alpha(x, y), 0, "corner ({x},{y}) stays empty");
        }
        assert!(
            canvas
                .chunks_exact(4)
                .any(|pixel| pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200),
            "the text is rasterised at the buffer density"
        );
    }

    /// The hairline is measured in buffer pixels, so a dense output gets the same one
    /// device pixel rather than a thicker line.
    #[test]
    fn the_hairline_stays_one_pixel_thin_at_any_density() {
        let one = render_with(Config::default(), sample(), 1.0);
        let two = render_at(Config::default(), sample(), 1.0, 2);

        for (canvas, width, _, scale) in [(one.0, one.1, one.2, 1u32), (two.0, two.1, two.2, 2)] {
            let margin = (SHADOW * scale) as usize;
            let w = width as usize;
            let row = |y: usize| canvas[(y * w + w / 2) * 4];
            let body = row(margin + 3 * scale as usize);
            assert_ne!(row(margin), body, "scale {scale}: the top edge is a hairline");
            assert_eq!(
                row(margin + 1),
                body,
                "scale {scale}: and it is one physical pixel thin"
            );
        }
    }

    /// The panel's logical height is unchanged by density, so its pixels double and the
    /// height clamp doubles with them.
    #[test]
    fn a_dense_panel_is_twice_as_tall_in_its_own_pixels() {
        let mut font_system = FontSystem::new();
        let mut one = build(&mut font_system, &Config::default(), 1, &sample());
        let mut two = build(&mut font_system, &Config::default(), 2, &sample());
        let single = card_height(&mut font_system, &mut one, false, 1);
        let dense = card_height(&mut font_system, &mut two, false, 2);
        assert!(dense.abs_diff(2 * single) <= 2, "{single} at 1x, {dense} at 2x");

        let long = Notification {
            id: 2,
            app_name: String::new(),
            summary: "s".into(),
            body: "wrapping words ".repeat(400),
            timeout: None,
            icon: None,
        };
        let mut one = build(&mut font_system, &Config::default(), 1, &long);
        let mut two = build(&mut font_system, &Config::default(), 2, &long);
        assert_eq!(card_height(&mut font_system, &mut one, false, 1), MAX_H);
        assert_eq!(card_height(&mut font_system, &mut two, false, 2), MAX_H * 2);
    }

    /// Guards the fontconfig trap where weight 600 for the generic sans family
    /// resolves to a serif CJK face whose missing space falls back to a colour
    /// emoji, leaving a seventeen-pixel hole in the title.
    #[test]
    fn title_is_shaped_from_one_font() {
        let mut font_system = FontSystem::new();
        let mut buffer = build(&mut font_system, &Config::default(), 1, &sample());
        buffer.shape_until_scroll(&mut font_system, false);

        let title = buffer
            .layout_runs()
            .find(|run| run.text.starts_with("Build finished"))
            .expect("title run");
        let fonts: std::collections::HashSet<_> = title.glyphs.iter().map(|g| g.font_id).collect();
        assert_eq!(fonts.len(), 1, "title must not fall back mid-run: {fonts:?}");

        let space = title
            .glyphs
            .iter()
            .find(|g| &title.text[g.start..g.end] == " ")
            .expect("title has a space");
        assert!(space.w < 6.0, "title space is {:.2}px wide", space.w);
    }

    #[test]
    fn a_card_at_progress_zero_is_invisible() {
        let (canvas, ..) = render_with(Config::default(), sample(), 0.0);
        assert!(
            canvas.chunks_exact(4).all(|pixel| pixel[3] == 0),
            "nothing may be visible before the card has arrived"
        );
    }

    #[test]
    fn arriving_slides_in_from_the_right_and_fades_up() {
        let settled = render_with(Config::default(), sample(), 1.0);
        let halfway = render_with(Config::default(), sample(), 0.5);

        let (left, right, ..) = inked_box(&settled.0, settled.1, settled.2).expect("the panel");
        assert_eq!((left, right), (0, PANEL_W as usize - 1), "a settled panel fills its panel");

        let (half_left, half_right, ..) =
            inked_box(&halfway.0, halfway.1, halfway.2).expect("still visible");
        assert!(
            half_left > 100,
            "half a slide in, the panel should sit about half a width right; left ink at {half_left}"
        );
        assert_eq!(
            half_right,
            PANEL_W as usize - 1,
            "and it is clipped against the edge it slides to"
        );

        assert!(
            ink(&halfway.0) < ink(&settled.0),
            "a card on its way in is fainter than a settled one"
        );
    }

    /// Each position arrives from the edge it is anchored to, so halfway through the
    /// panel is still against that edge and clear of the opposite one.
    #[test]
    fn a_panel_arrives_from_the_edge_its_position_names() {
        let cases = [
            ("top-left", Edge::Left),
            ("bottom-left", Edge::Left),
            ("top-center", Edge::Top),
            ("bottom-center", Edge::Bottom),
            ("top-right", Edge::Right),
            ("bottom-right", Edge::Right),
        ];
        for (name, edge) in cases {
            let config: Config =
                toml::from_str(&format!("[panel]\nposition = {name:?}\n")).expect(name);
            let (canvas, width, height) = render_with(config, sample(), 0.5);
            let (left, right, top, bottom) =
                inked_box(&canvas, width, height).unwrap_or_else(|| panic!("{name}: nothing painted"));
            let (last_col, last_row) = (
                width as usize - 2 * SHADOW as usize - 1,
                height as usize - 2 * SHADOW as usize - 1,
            );

            match edge {
                Edge::Left => {
                    assert_eq!(left, 0, "{name} must still reach the left edge");
                    assert!(right < last_col, "{name} must be clipped on the right");
                }
                Edge::Right => {
                    assert_eq!(right, last_col, "{name} must still reach the right edge");
                    assert!(left > 0, "{name} must be clipped on the left");
                }
                Edge::Top => {
                    assert_eq!(top, 0, "{name} must still reach the top edge");
                    assert!(bottom < last_row, "{name} must be clipped at the bottom");
                }
                Edge::Bottom => {
                    assert_eq!(bottom, last_row, "{name} must still reach the bottom edge");
                    assert!(top > 0, "{name} must be clipped at the top");
                }
            }
        }
    }

    #[test]
    fn no_position_is_visible_before_it_arrives() {
        for name in [
            "top-left",
            "bottom-left",
            "top-center",
            "bottom-center",
            "top-right",
            "bottom-right",
        ] {
            let config: Config =
                toml::from_str(&format!("[panel]\nposition = {name:?}\n")).expect(name);
            let (canvas, ..) = render_with(config, sample(), 0.0);
            assert!(
                canvas.chunks_exact(4).all(|pixel| pixel[3] == 0),
                "{name} is showing something before it has arrived"
            );
        }
    }

    #[test]
    fn progress_only_ever_adds_to_a_settled_card() {
        let settled = render_with(Config::default(), sample(), 1.0);
        for progress in [0.0, 0.25, 0.5, 0.75] {
            let frame = render_with(Config::default(), sample(), progress);
            assert_eq!(frame.1, settled.1, "the canvas size never changes with progress");
            assert!(ink(&frame.0) <= ink(&settled.0));
        }
    }

    #[test]
    fn long_bodies_are_clipped_to_max_height() {
        let mut font_system = FontSystem::new();
        let long = Notification {
            id: 2,
            app_name: String::new(),
            summary: "s".into(),
            body: "wrapping words ".repeat(400),
            timeout: None,
            icon: None,
        };
        let mut buffer = build(&mut font_system, &Config::default(), 1, &long);
        assert_eq!(card_height(&mut font_system, &mut buffer, false, 1), MAX_H);
    }

    #[test]
    fn configured_colours_and_border_reach_the_pixels() {
        let plain: Config = toml::from_str(
            "[background]\ncolor = \"#ff0000ff\"\n\n[border]\nwidth = 0.0\n",
        )
        .unwrap();
        let (canvas, width, height) = render_with(plain, sample(), 1.0);
        let (w, h) = (width as usize, height as usize);

        let centre = (h / 2 * w + w / 2) * 4;
        // canvas is B,G,R,A after the swap
        assert_eq!(&canvas[centre..centre + 3], &[0, 0, 255], "opaque red background");

        // Two pixels inside the panel's top edge, so the hairline is out of the way.
        let near_top = ((SHADOW as usize + 2) * w + w / 2) * 4;
        assert_eq!(&canvas[near_top..near_top + 3], &[0, 0, 255], "width 0 draws no hairline");

        let bordered: Config = toml::from_str(
            "[background]\ncolor = \"#ff0000ff\"\n\n[border]\nwidth = 3.0\ncolor = \"#ffffffff\"\n",
        )
        .unwrap();
        let (canvas, width, _) = render_with(bordered, sample(), 1.0);
        let w = width as usize;
        let near_top = ((SHADOW as usize + 1) * w + w / 2) * 4;
        assert!(
            canvas[near_top..near_top + 3].iter().all(|channel| *channel > 200),
            "a 3px white hairline paints over the panel edge"
        );
    }

    /// With no icon the layout is the one plush has always had: text at the padding, in
    /// the full panel width.
    #[test]
    fn without_an_icon_the_text_gets_the_whole_panel() {
        assert_eq!(text_x(false), PAD);
        assert_eq!(text_width(false), PANEL_W as f32 - 2.0 * PAD);
        assert_eq!(text_x(true), PAD + ICON + ICON_GAP);
        assert_eq!(text_width(true), PANEL_W as f32 - 2.0 * PAD - ICON - ICON_GAP);
    }

    /// The icon goes in the slot, and the text is moved over with it. The icon here is
    /// fully transparent, so what moves in the ink box is the text alone.
    #[test]
    fn an_icon_moves_the_text_over_by_the_slot() {
        let invisible = solid_icon(8, 8, [0, 0, 0, 0]);
        let plain = render_with(transparent(), sample(), 1.0);
        let shifted = render_with(transparent(), with_icon(invisible), 1.0);

        let (left, ..) = inked_box(&plain.0, plain.1, plain.2).expect("text");
        let (moved, ..) = inked_box(&shifted.0, shifted.1, shifted.2).expect("text");

        assert_eq!(moved - left, (ICON + ICON_GAP) as usize, "the text clears the slot");
    }

    /// A narrower column of text wraps sooner, which is the other half of the layout.
    ///
    /// The words are short on purpose: a wrap point only moves when the next word no
    /// longer fits, and long words make the two widths land on the same one.
    #[test]
    fn text_wraps_narrower_beside_an_icon() {
        let long = Notification {
            id: 2,
            app_name: String::new(),
            summary: "s".into(),
            body: "word ".repeat(40),
            timeout: None,
            icon: None,
        };
        let plain = render_with(Config::default(), long.clone(), 1.0);
        let beside = render_with(
            Config::default(),
            Notification { icon: Some(solid_icon(8, 8, [255, 0, 0, 255])), ..long },
            1.0,
        );

        assert!(
            beside.2 > plain.2,
            "the same body needs more lines beside an icon: {} against {}",
            beside.2,
            plain.2
        );
    }

    /// The slot is drawn where the text was moved from, and in the icon's own colour.
    #[test]
    fn the_icon_is_drawn_in_the_slot_beside_the_text() {
        let (canvas, width, height) = render_with(transparent(), with_icon(solid_icon(64, 64, [255, 0, 0, 255])), 1.0);
        let (w, h) = (width as usize, height as usize);
        let (_, right, top, bottom) = inked_box(&canvas, width, height).expect("ink");

        // The canvas is B,G,R,A after the swap, so a red icon is blue at index 0. The
        // ink box is on the panel, which starts one shadow margin into the buffer.
        let margin = SHADOW as usize;
        let centre = ((top + bottom) / 2 + margin) * w * 4 + (PAD as usize + 16 + margin) * 4;
        assert!(canvas[centre + 2] > 200, "the slot holds the icon");
        assert!(canvas[centre] < 60, "in the icon's colour, not the panel's");
        assert!(right > PAD as usize + 32, "and the text follows it");
        assert!(h > (ICON + 2.0 * PAD) as usize, "the panel is tall enough for both");
    }

    /// The icon is centred in the panel, however tall the panel is for the text.
    #[test]
    fn the_icon_is_centred_vertically_in_the_panel() {
        let (canvas, width, height) = render_with(transparent(), with_icon(solid_icon(32, 32, [255, 0, 0, 255])), 1.0);
        let (w, h) = (width as usize, height as usize);
        let margin = SHADOW as usize;
        let top = (height - 2 * SHADOW - 32) / 2;

        let slot = |y: usize| ((y + margin) * w + PAD as usize + 16 + margin) * 4;
        assert!(canvas[slot(top as usize) + 2] > 200, "the slot starts where it should");
        assert!(canvas[slot(top as usize + 31) + 2] > 200, "and ends where it should");
        assert_eq!(canvas[slot(top as usize - 1) + 3], 0, "nothing above the slot");
        assert_eq!(canvas[slot((top + 32) as usize) + 3], 0, "nothing below it");
        assert!(h >= (ICON + 2.0 * PAD) as usize);
    }

    /// A wide icon is scaled to the slot's width, and not stretched to its height.
    #[test]
    fn a_wide_icon_keeps_its_aspect_ratio() {
        let mut bare = with_icon(solid_icon(128, 32, [255, 0, 0, 255]));
        bare.app_name = String::new();
        bare.summary = String::new();
        bare.body = String::new();
        let (canvas, width, height) = render_with(transparent(), bare, 1.0);

        let (left, right, top, bottom) = inked_box(&canvas, width, height).expect("the icon");
        let (drawn_w, drawn_h) = (right - left + 1, bottom - top + 1);
        assert!((31..=33).contains(&drawn_w), "a 128x32 icon is drawn 32 wide, got {drawn_w}");
        assert!((7..=9).contains(&drawn_h), "and 8 tall, not stretched: got {drawn_h}");
        assert_eq!(left, PAD as usize, "starting at the padding");
    }

    /// A one-line notification is taller with an icon, because the slot and its padding
    /// are taller than the minimum panel.
    #[test]
    fn a_short_notification_with_an_icon_is_tall_enough_for_the_slot() {
        let mut one_line = sample();
        one_line.app_name = String::new();
        one_line.summary = "One line".into();
        one_line.body = String::new();

        let plain = render_with(Config::default(), one_line.clone(), 1.0);
        let icon = render_with(
            Config::default(),
            Notification { icon: Some(solid_icon(8, 8, [255, 0, 0, 255])), ..one_line },
            1.0,
        );

        assert_eq!(
            plain.2,
            surface(PANEL_W, MIN_H).1,
            "a one-line panel is still the minimum height"
        );
        assert_eq!(
            icon.2,
            surface(PANEL_W, (ICON + 2.0 * PAD) as u32).1,
            "and taller with an icon"
        );
    }

    /// The line spans the panel between the corners when nothing has elapsed, shortens
    /// from its fixed left end, and is not there at all when the timeout is up.
    #[test]
    fn the_countdown_line_drains_from_the_left() {
        let full = render_countdown(transparent(), Some(1.0));
        let half = render_countdown(transparent(), Some(0.5));
        let gone = render_countdown(transparent(), Some(0.0));

        let (left, right, top, bottom) = inked_box(&full.0, full.1, full.2).expect("the line");
        let panel_w = full.1 as usize - 2 * SHADOW as usize;
        assert_eq!(left, RADIUS as usize, "the line starts inside one corner");
        assert_eq!(right, panel_w - RADIUS as usize - 1, "and ends inside the other");
        assert_eq!(bottom - top + 1, COUNTDOWN_H as usize, "and is a hairline");

        let (half_left, half_right, ..) = inked_box(&half.0, half.1, half.2).expect("the line");
        assert_eq!(half_left, left, "the left end never moves");
        assert!(half_right < right, "half the timeout is a shorter line");
        assert_eq!(inked_box(&gone.0, gone.1, gone.2), None, "and no line once it is up");
    }

    /// The line hugs whichever edge it is configured for: top and bottom run horizontally,
    /// left and right vertically, each between the corners and draining from its far end.
    #[test]
    fn the_countdown_line_follows_its_configured_edge() {
        let line = COUNTDOWN_H as usize;
        let corner = RADIUS as usize;

        for edge in ["top", "bottom", "left", "right"] {
            let full = render_countdown(transparent_on(edge), Some(1.0));
            let (left, right, top, bottom) =
                inked_box(&full.0, full.1, full.2).unwrap_or_else(|| panic!("{edge}: no line"));
            let panel = (PANEL_W as usize, full.2 as usize - 2 * SHADOW as usize);
            let (across, along) = ((corner, panel.0 - corner - 1), (corner, panel.1 - corner - 1));

            match edge {
                "top" => {
                    assert_eq!((left, right), across, "{edge}: between the corners");
                    assert_eq!((top, bottom), (0, line - 1), "{edge}: a bar at the top");
                }
                "bottom" => {
                    assert_eq!((left, right), across, "{edge}: between the corners");
                    assert_eq!(
                        (top, bottom),
                        (panel.1 - line, panel.1 - 1),
                        "{edge}: a bar at the bottom"
                    );
                }
                "left" => {
                    assert_eq!((left, right), (0, line - 1), "{edge}: a bar at the left");
                    assert_eq!((top, bottom), along, "{edge}: between the corners");
                }
                _ => {
                    assert_eq!(
                        (left, right),
                        (panel.0 - line, panel.0 - 1),
                        "{edge}: a bar at the right"
                    );
                    assert_eq!((top, bottom), along, "{edge}: between the corners");
                }
            }

            // Half the timeout shortens it from the far end, never from the near one.
            let half = render_countdown(transparent_on(edge), Some(0.5));
            let (half_left, half_right, half_top, half_bottom) =
                inked_box(&half.0, half.1, half.2).unwrap_or_else(|| panic!("{edge}: no line"));
            match edge {
                "top" | "bottom" => {
                    assert_eq!(half_left, left, "{edge}: the left end never moves");
                    assert!(half_right < right, "{edge}: half the timeout is a shorter line");
                }
                _ => {
                    assert_eq!(half_top, top, "{edge}: the top end never moves");
                    assert!(half_bottom < bottom, "{edge}: half the timeout is a shorter line");
                }
            }
        }
    }

    /// A panel that never expires has no line to draw.
    #[test]
    fn a_panel_with_no_timeout_has_no_countdown_line() {
        let (canvas, width, height) = render_countdown(transparent(), None);
        assert_eq!(inked_box(&canvas, width, height), None, "nothing is drawn");
    }

    /// The line is inside the panel: above the bottom border, and not over the corners it
    /// would otherwise paint across.
    #[test]
    fn the_countdown_line_keeps_inside_the_panel() {
        let plain = render_countdown(Config::default(), None);
        let lined = render_countdown(Config::default(), Some(1.0));
        let (w, h) = (lined.1 as usize, lined.2 as usize);

        let mut rows: Vec<usize> = plain
            .0
            .chunks_exact(4)
            .zip(lined.0.chunks_exact(4))
            .enumerate()
            .filter(|(_, (before, after))| before != after)
            .map(|(pixel, _)| pixel / w)
            .collect();
        rows.dedup();
        let top = h
            - SHADOW as usize
            - Config::default().border.width as usize
            - COUNTDOWN_H as usize;
        assert_eq!(rows, vec![top, top + 1], "the line sits above the hairline");

        for (x, y) in [(0usize, h - 1), (w - 1, h - 1), (0, 0), (w - 1, 0)] {
            assert!(
                lined.0[(y * w + x) * 4 + 3] < 40,
                "corner ({x},{y}) must stay rounded away"
            );
        }
    }
}
