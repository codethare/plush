//! Painting: Apple's dark "system" palette, rounded panels, text through cosmic-text.
//!
//! Everything here is pure pixel work. The glyph path deliberately does not use
//! `cosmic_text::Buffer::draw`: that callback hides the coverage/premultiplication
//! convention, and the canvas has to end up premultiplied *and* channel-swapped.

use cosmic_text::{Attrs, Buffer, FontSystem, Shaping, SwashCache, SwashContent, SwashImage};
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, PixmapMut, Stroke, Transform};

use crate::{
    config::{Config, Edge},
    dbus::Notification,
};

/// Panel width in logical pixels.
pub const PANEL_W: u32 = 380;
/// Tallest panel plush will draw; longer bodies are clipped.
pub const MAX_H: u32 = 320;
/// Cards kept on screen. Older ones fall off the stack.
pub const MAX_CARDS: usize = 4;

const PAD: f32 = 16.0;
const RADIUS: f32 = 14.0;
const MIN_H: u32 = 60;

const INK_PRIMARY: cosmic_text::Color = cosmic_text::Color::rgba(255, 255, 255, 245);
const INK_SECONDARY: cosmic_text::Color = cosmic_text::Color::rgba(235, 235, 245, 155);
const INK_TERTIARY: cosmic_text::Color = cosmic_text::Color::rgba(235, 235, 245, 115);

fn text_width() -> f32 {
    PANEL_W as f32 - 2.0 * PAD
}

/// Shapes a notification into a wrapped, styled buffer.
pub fn build(font_system: &mut FontSystem, config: &Config, notification: &Notification) -> Buffer {
    let font = &config.font;
    let mut buffer = Buffer::new(font_system, font.body_metrics());
    buffer.set_size(Some(text_width()), Some(MAX_H as f32 - 2.0 * PAD));

    let mut parts: Vec<(String, Attrs)> = Vec::new();
    if !notification.app_name.is_empty() {
        parts.push((
            notification.app_name.clone(),
            Attrs::new()
                .family(font.family())
                .metrics(font.app_metrics())
                .color(INK_TERTIARY)
                .weight(font.app_weight.0),
        ));
    }
    parts.push((
        notification.summary.clone(),
        Attrs::new()
            .family(font.family())
            .metrics(font.title_metrics())
            .color(INK_PRIMARY)
            .weight(font.title_weight.0),
    ));
    if !notification.body.is_empty() {
        parts.push((
            notification.body.clone(),
            Attrs::new()
                .family(font.family())
                .metrics(font.body_metrics())
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
        &Attrs::new().family(font.family()).metrics(font.body_metrics()),
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
///
/// `progress` is the card's animation state: 0 is a full side off screen and invisible,
/// 1 is settled. Which side is the configured position, so a panel on the left slides
/// right, one on the right slides left, and the centred ones drop in or rise. Cards
/// drawn with animations turned off pass 1.0.
#[allow(clippy::too_many_arguments)]
pub fn paint(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    config: &Config,
    font_system: &mut FontSystem,
    swash: &mut SwashCache,
    buffer: &mut Buffer,
    progress: f32,
) {
    let pixels = &mut canvas[..width as usize * height as usize * 4];
    let progress = progress.clamp(0.0, 1.0);

    // A panel travels its own side, so at progress 0 it sits entirely outside the buffer.
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

    panel(pixels, width, height, config, dx, dy);
    text(pixels, width, height, font_system, swash, buffer, dx, dy);
    // wl_shm's Argb8888 is BGRA in memory on little-endian; tiny-skia draws RGBA.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
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

fn panel(pixels: &mut [u8], width: u32, height: u32, config: &Config, dx: f32, dy: f32) {
    let Some(mut pixmap) = PixmapMut::from_bytes(pixels, width, height) else {
        return;
    };
    pixmap.fill(Color::TRANSPARENT);

    let (w, h) = (width as f32, height as f32);
    let background = config.background.color;
    let mut fill = Paint::default();
    fill.set_color_rgba8(background.0, background.1, background.2, background.3);
    fill.anti_alias = true;
    pixmap.fill_path(
        &rounded_rect(dx, dy, w, h, RADIUS),
        &fill,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let stroke_width = config.border.width;
    // Zero means no border, and tiny-skia would draw a one-pixel hairline for it
    // anyway. A stroke at least as wide as the panel would invert the inset path.
    if stroke_width <= 0.0 || stroke_width >= w.min(h) {
        return;
    }

    // Inset by half the stroke so the whole border lands inside the panel.
    let border = config.border.color;
    let mut edge = Paint::default();
    edge.set_color_rgba8(border.0, border.1, border.2, border.3);
    edge.anti_alias = true;
    let inset = stroke_width / 2.0;
    pixmap.stroke_path(
        &rounded_rect(
            dx + inset,
            dy + inset,
            w - stroke_width,
            h - stroke_width,
            RADIUS - inset,
        ),
        &edge,
        &Stroke { width: stroke_width, ..Stroke::default() },
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
    dx: f32,
    dy: f32,
) {
    buffer.shape_until_scroll(font_system, false);
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let Some(color) = glyph.color_opt else {
                continue;
            };
            let origin = glyph.physical((PAD + dx, PAD + dy + run.line_y), 1.0);
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
    let r = radius.max(0.0).min(w / 2.0).min(h / 2.0);
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

    fn render_with(config: Config, sample: Notification, progress: f32) -> (Vec<u8>, u32, u32) {
        let mut font_system = FontSystem::new();
        let mut buffer = build(&mut font_system, &config, &sample);
        let height = card_height(&mut font_system, &mut buffer);
        let mut canvas = vec![0u8; PANEL_W as usize * height as usize * 4];
        let mut swash = SwashCache::new();
        paint(
            &mut canvas,
            PANEL_W,
            height,
            &config,
            &mut font_system,
            &mut swash,
            &mut buffer,
            progress,
        );
        (canvas, PANEL_W, height)
    }

    /// Leftmost column with anything in it, in logical pixels.
    /// Bounding box of everything painted, as (left, right, top, bottom).
    fn inked_box(canvas: &[u8], width: u32, height: u32) -> Option<(usize, usize, usize, usize)> {
        let (w, h) = (width as usize, height as usize);
        let mut found: Option<(usize, usize, usize, usize)> = None;
        for y in 0..h {
            for x in 0..w {
                if canvas[(y * w + x) * 4 + 3] > 0 {
                    let (left, right, top, bottom) = found.unwrap_or((usize::MAX, 0, usize::MAX, 0));
                    found = Some((left.min(x), right.max(x), top.min(y), bottom.max(y)));
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
        }
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

    /// Guards the fontconfig trap where weight 600 for the generic sans family
    /// resolves to a serif CJK face whose missing space falls back to a colour
    /// emoji, leaving a seventeen-pixel hole in the title.
    #[test]
    fn title_is_shaped_from_one_font() {
        let mut font_system = FontSystem::new();
        let mut buffer = build(&mut font_system, &Config::default(), &sample());
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

        let (last_col, last_row) = (settled.1 as usize - 1, settled.2 as usize - 1);
        assert_eq!(
            inked_box(&settled.0, settled.1, settled.2),
            Some((0, last_col, 0, last_row)),
            "a settled panel fills its buffer"
        );

        let (left, right, ..) = inked_box(&halfway.0, halfway.1, halfway.2).expect("still visible");
        assert!(
            (150..200).contains(&left),
            "half a slide in, the panel should sit about half a width right; left ink at {left}"
        );
        assert_eq!(right, last_col, "and still be clipped against the right edge");

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
            let (last_col, last_row) = (width as usize - 1, height as usize - 1);

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
        };
        let mut buffer = build(&mut font_system, &Config::default(), &long);
        assert_eq!(card_height(&mut font_system, &mut buffer), MAX_H);
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

        let near_top = (w + w / 2) * 4;
        assert_eq!(&canvas[near_top..near_top + 3], &[0, 0, 255], "width 0 draws no border");

        let bordered: Config = toml::from_str(
            "[background]\ncolor = \"#ff0000ff\"\n\n[border]\nwidth = 3.0\ncolor = \"#ffffffff\"\n",
        )
        .unwrap();
        let (canvas, width, _) = render_with(bordered, sample(), 1.0);
        let w = width as usize;
        let near_top = (w + w / 2) * 4;
        assert!(
            canvas[near_top..near_top + 3].iter().all(|channel| *channel > 200),
            "a 3px white border paints over the panel edge"
        );
    }
}
