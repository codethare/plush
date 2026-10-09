//! Painting: Apple's dark "system" palette, rounded panels, text through cosmic-text.
//!
//! Everything here is pure pixel work. The glyph path deliberately does not use
//! `cosmic_text::Buffer::draw`: that callback hides the coverage/premultiplication
//! convention, and the canvas has to end up premultiplied *and* channel-swapped.

use cosmic_text::{
    Attrs, Buffer, FontSystem, Metrics, Shaping, SwashCache, SwashContent, SwashImage, Weight,
};
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, PixmapMut, Stroke, Transform};

use crate::dbus::Notification;

/// Panel width in logical pixels.
pub const PANEL_W: u32 = 380;
/// Tallest panel plush will draw; longer bodies are clipped.
pub const MAX_H: u32 = 320;
/// Cards kept on screen. Older ones fall off the stack.
pub const MAX_CARDS: usize = 4;

const PAD: f32 = 16.0;
const RADIUS: f32 = 14.0;
const MIN_H: u32 = 60;

const PANEL_BG: (u8, u8, u8, u8) = (28, 28, 30, 214);
const PANEL_EDGE: (u8, u8, u8, u8) = (255, 255, 255, 26);
const INK_PRIMARY: cosmic_text::Color = cosmic_text::Color::rgba(255, 255, 255, 245);
const INK_SECONDARY: cosmic_text::Color = cosmic_text::Color::rgba(235, 235, 245, 155);
const INK_TERTIARY: cosmic_text::Color = cosmic_text::Color::rgba(235, 235, 245, 115);

fn text_width() -> f32 {
    PANEL_W as f32 - 2.0 * PAD
}

fn body_metrics() -> Metrics {
    Metrics::new(13.0, 18.0)
}

/// Shapes a notification into a wrapped, styled buffer.
pub fn build(font_system: &mut FontSystem, notification: &Notification) -> Buffer {
    let mut buffer = Buffer::new(font_system, body_metrics());
    buffer.set_size(Some(text_width()), Some(MAX_H as f32 - 2.0 * PAD));

    let mut parts: Vec<(String, Attrs)> = Vec::new();
    if !notification.app_name.is_empty() {
        parts.push((
            notification.app_name.clone(),
            Attrs::new()
                .metrics(Metrics::new(11.0, 15.0))
                .color(INK_TERTIARY)
                .weight(Weight::MEDIUM),
        ));
    }
    parts.push((
        notification.summary.clone(),
        Attrs::new()
            .metrics(Metrics::new(14.0, 19.0))
            .color(INK_PRIMARY)
            // Bold, not semibold: fontconfig resolves weight 600 for the generic
            // sans family to a serif CJK face whose space glyph is missing, which
            // then falls back to a colour-emoji space seventeen pixels wide.
            .weight(Weight::BOLD),
    ));
    if !notification.body.is_empty() {
        parts.push((
            notification.body.clone(),
            Attrs::new().metrics(body_metrics()).color(INK_SECONDARY),
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
        &Attrs::new().metrics(body_metrics()),
        Shaping::Advanced,
        None,
    );
    buffer
}

/// Panel height needed by an already-`build`t buffer.
pub fn card_height(font_system: &mut FontSystem, buffer: &mut Buffer) -> u32 {
    buffer.shape_until_scroll(font_system, false);
    let text: f32 = buffer.layout_runs().map(|run| run.line_height).sum();
    ((text + 2.0 * PAD).ceil() as u32).clamp(MIN_H, MAX_H)
}

/// Renders one panel into `canvas`.
pub fn paint(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    font_system: &mut FontSystem,
    swash: &mut SwashCache,
    buffer: &mut Buffer,
) {
    let pixels = &mut canvas[..width as usize * height as usize * 4];
    panel(pixels, width, height);
    text(pixels, width, height, font_system, swash, buffer);
    // wl_shm's Argb8888 is BGRA in memory on little-endian; tiny-skia draws RGBA.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

fn panel(pixels: &mut [u8], width: u32, height: u32) {
    let Some(mut pixmap) = PixmapMut::from_bytes(pixels, width, height) else {
        return;
    };
    pixmap.fill(Color::TRANSPARENT);

    let (w, h) = (width as f32, height as f32);
    let mut fill = Paint::default();
    fill.set_color_rgba8(PANEL_BG.0, PANEL_BG.1, PANEL_BG.2, PANEL_BG.3);
    fill.anti_alias = true;
    pixmap.fill_path(
        &rounded_rect(0.0, 0.0, w, h, RADIUS),
        &fill,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // Inset by half a pixel so the hairline lands inside the panel.
    let mut edge = Paint::default();
    edge.set_color_rgba8(PANEL_EDGE.0, PANEL_EDGE.1, PANEL_EDGE.2, PANEL_EDGE.3);
    edge.anti_alias = true;
    let stroke = Stroke { width: 1.0, ..Stroke::default() };
    pixmap.stroke_path(
        &rounded_rect(0.5, 0.5, w - 1.0, h - 1.0, RADIUS - 0.5),
        &edge,
        &stroke,
        Transform::identity(),
        None,
    );
}

fn text(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    font_system: &mut FontSystem,
    swash: &mut SwashCache,
    buffer: &mut Buffer,
) {
    buffer.shape_until_scroll(font_system, false);
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let Some(color) = glyph.color_opt else {
                continue;
            };
            let origin = glyph.physical((PAD, PAD + run.line_y), 1.0);
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

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Path {
    let r = radius.min(w / 2.0).min(h / 2.0);
    let mut path = PathBuilder::new();
    path.move_to(x + r, y);
    path.line_to(x + w - r, y);
    path.quad_to(x + w, y, x + w, y + r);
    path.line_to(x + w, y + h - r);
    path.quad_to(x + w, y + h, x + w - r, y + h);
    path.line_to(x + r, y + h);
    path.quad_to(x, y + h, x, y + h - r);
    path.line_to(x, y + r);
    path.quad_to(x, y, x + r, y);
    path.close();
    path.finish().expect("rounded rect is a valid path")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::Notification;

    fn render(sample: Notification) -> (Vec<u8>, u32, u32, FontSystem) {
        let mut font_system = FontSystem::new();
        let mut buffer = build(&mut font_system, &sample);
        let height = card_height(&mut font_system, &mut buffer);
        let mut canvas = vec![0u8; PANEL_W as usize * height as usize * 4];
        let mut swash = SwashCache::new();
        paint(&mut canvas, PANEL_W, height, &mut font_system, &mut swash, &mut buffer);
        (canvas, PANEL_W, height, font_system)
    }

    fn sample() -> Notification {
        Notification {
            id: 1,
            app_name: "Test App".into(),
            summary: "Build finished".into(),
            body: "cargo build --release succeeded in 12.4s after a long enough run".into(),
            timeout: None,
        }
    }

    #[test]
    fn panel_has_the_expected_shape_and_colours() {
        let (canvas, width, height, _) = render(sample());
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

    /// Guards the fontconfig trap where weight 600 for the generic sans family
    /// resolves to a serif CJK face whose missing space falls back to a colour
    /// emoji, leaving a seventeen-pixel hole in the title.
    #[test]
    fn title_is_shaped_from_one_font() {
        let mut font_system = FontSystem::new();
        let mut buffer = build(&mut font_system, &sample());
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
    fn long_bodies_are_clipped_to_max_height() {
        let mut font_system = FontSystem::new();
        let long = Notification {
            id: 2,
            app_name: String::new(),
            summary: "s".into(),
            body: "wrapping words ".repeat(400),
            timeout: None,
        };
        let mut buffer = build(&mut font_system, &long);
        assert_eq!(card_height(&mut font_system, &mut buffer), MAX_H);
    }
}
