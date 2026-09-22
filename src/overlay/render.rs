//! Draws the profile-switch banner into a premultiplied RGBA bitmap on the
//! CPU, so both platform windows only have to present pixels. Uses the same
//! font and palette as the tray panel.

use skrifa::{
    FontRef, GlyphId, MetadataProvider as _,
    instance::{LocationRef, Size},
    outline::{DrawSettings, OutlinePen},
};
use vello_cpu::{
    Pixmap, RenderContext, Resources,
    color::{AlphaColor, Srgb},
    kurbo::{BezPath, Circle, RoundedRect, Shape as _},
};

const HEIGHT: f64 = 60.0;
const RADIUS: f64 = 14.0;
const PADDING: f64 = 18.0;
const DOT_RADIUS: f64 = 4.0;
const TEXT_LEFT: f64 = PADDING + DOT_RADIUS * 2.0 + 12.0;
const MIN_WIDTH: f64 = 240.0;
const MAX_WIDTH: f64 = 460.0;
const TITLE_SIZE: f32 = 15.0;
const DETAIL_SIZE: f32 = 12.5;
const TITLE_BASELINE: f64 = 27.0;
const DETAIL_BASELINE: f64 = 45.0;

const BACKGROUND: AlphaColor<Srgb> = AlphaColor::from_rgba8(16, 17, 19, 245);
const BORDER: AlphaColor<Srgb> = AlphaColor::from_rgba8(255, 255, 255, 20);
const TEXT: AlphaColor<Srgb> = AlphaColor::from_rgba8(236, 238, 240, 255);
const MUTED: AlphaColor<Srgb> = AlphaColor::from_rgba8(150, 156, 163, 255);
const ACCENT: AlphaColor<Srgb> = AlphaColor::from_rgba8(93, 222, 137, 255);

/// A rendered banner: premultiplied RGBA8 rows, top to bottom.
pub struct Banner {
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u8>,
}

/// Renders `title` over `detail` at `scale` physical pixels per point.
pub fn banner(title: &str, detail: &str, scale: f64) -> Banner {
    let font = FontRef::new(epaint_default_fonts::UBUNTU_LIGHT)
        .expect("the bundled Ubuntu Light font is valid");
    let text_room = MAX_WIDTH - TEXT_LEFT - PADDING;
    let title = fit(&font, title, TITLE_SIZE, text_room);
    let detail = fit(&font, detail, DETAIL_SIZE, text_room);
    let text_width =
        line_width(&font, &title, TITLE_SIZE).max(line_width(&font, &detail, DETAIL_SIZE));
    let width = (TEXT_LEFT + text_width + PADDING).clamp(MIN_WIDTH, MAX_WIDTH);

    let pixel_width = (width * scale).ceil() as u16;
    let pixel_height = (HEIGHT * scale).ceil() as u16;
    let mut context = RenderContext::new(pixel_width, pixel_height);
    let scaled = |shape: BezPath| {
        let mut path = shape;
        path.apply_affine(vello_cpu::kurbo::Affine::scale(scale));
        path
    };

    // Inset by half a point so the 1-point border stays inside the bitmap.
    let outer = RoundedRect::new(0.0, 0.0, width, HEIGHT, RADIUS);
    let inner = RoundedRect::new(1.0, 1.0, width - 1.0, HEIGHT - 1.0, RADIUS - 1.0);
    context.set_paint(BORDER);
    context.fill_path(&scaled(outer.to_path(0.1)));
    context.set_paint(BACKGROUND);
    context.fill_path(&scaled(inner.to_path(0.1)));

    context.set_paint(ACCENT);
    let dot = Circle::new((PADDING + DOT_RADIUS, HEIGHT / 2.0), DOT_RADIUS);
    context.fill_path(&scaled(dot.to_path(0.1)));

    context.set_paint(TEXT);
    context.fill_path(&text_path(&font, &title, TITLE_SIZE, TITLE_BASELINE, scale));
    context.set_paint(MUTED);
    context.fill_path(&text_path(
        &font,
        &detail,
        DETAIL_SIZE,
        DETAIL_BASELINE,
        scale,
    ));

    let mut pixmap = Pixmap::new(pixel_width, pixel_height);
    let mut resources = Resources::new();
    context.flush();
    context.render(&mut pixmap, &mut resources);
    Banner {
        width: pixel_width,
        height: pixel_height,
        pixels: pixmap.data_as_u8_slice().to_vec(),
    }
}

fn glyph(font: &FontRef<'_>, character: char) -> GlyphId {
    let charmap = font.charmap();
    charmap
        .map(character)
        .or_else(|| charmap.map('?'))
        .unwrap_or(GlyphId::NOTDEF)
}

fn line_width(font: &FontRef<'_>, text: &str, size: f32) -> f64 {
    let metrics = font.glyph_metrics(Size::new(size), LocationRef::default());
    text.chars()
        .map(|character| f64::from(metrics.advance_width(glyph(font, character)).unwrap_or(0.0)))
        .sum()
}

/// Shortens `text` with an ellipsis until it fits in `room` points.
fn fit(font: &FontRef<'_>, text: &str, size: f32, room: f64) -> String {
    if line_width(font, text, size) <= room {
        return text.to_owned();
    }
    let mut characters: Vec<char> = text.chars().collect();
    while !characters.is_empty() {
        characters.pop();
        let candidate = format!("{}…", characters.iter().collect::<String>().trim_end());
        if line_width(font, &candidate, size) <= room {
            return candidate;
        }
    }
    "…".into()
}

/// Lays `text` out left to right from `TEXT_LEFT` on `baseline`, as one path
/// in physical pixels.
fn text_path(font: &FontRef<'_>, text: &str, size: f32, baseline: f64, scale: f64) -> BezPath {
    let pixel_size = Size::new(size * scale as f32);
    let metrics = font.glyph_metrics(pixel_size, LocationRef::default());
    let outlines = font.outline_glyphs();
    let mut path = BezPath::new();
    let mut x = TEXT_LEFT * scale;
    let y = baseline * scale;
    for character in text.chars() {
        let id = glyph(font, character);
        if let Some(outline) = outlines.get(id) {
            let mut pen = PathPen {
                path: &mut path,
                x,
                y,
            };
            let settings = DrawSettings::unhinted(pixel_size, LocationRef::default());
            let _ = outline.draw(settings, &mut pen);
        }
        x += f64::from(metrics.advance_width(id).unwrap_or(0.0));
    }
    path
}

/// Adapts skrifa's y-up glyph outlines to a y-down path at an origin.
struct PathPen<'a> {
    path: &'a mut BezPath,
    x: f64,
    y: f64,
}

impl PathPen<'_> {
    fn point(&self, x: f32, y: f32) -> (f64, f64) {
        (self.x + f64::from(x), self.y - f64::from(y))
    }
}

impl OutlinePen for PathPen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let point = self.point(x, y);
        self.path.move_to(point);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let point = self.point(x, y);
        self.path.line_to(point);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let (control, end) = (self.point(cx0, cy0), self.point(x, y));
        self.path.quad_to(control, end);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (first, second, end) = (self.point(cx0, cy0), self.point(cx1, cy1), self.point(x, y));
        self.path.curve_to(first, second, end);
    }

    fn close(&mut self) {
        self.path.close_path();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_is_opaque_inside_and_transparent_at_the_corners() {
        let banner = banner("Counter-Strike 2", "Profile on · 1600 DPI", 2.0);
        assert_eq!(banner.height, 120);
        assert!(banner.width >= 480);
        let alpha = |x: usize, y: usize| banner.pixels[(y * banner.width as usize + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 0);
        assert!(alpha(banner.width as usize / 2, 60) > 200);
    }

    #[test]
    fn long_titles_are_shortened_to_fit() {
        let font = FontRef::new(epaint_default_fonts::UBUNTU_LIGHT).unwrap();
        let fitted = fit(&font, &"Very Long Game Name ".repeat(10), TITLE_SIZE, 200.0);
        assert!(fitted.ends_with('…'));
        assert!(line_width(&font, &fitted, TITLE_SIZE) <= 200.0);
    }

    /// Writes the banner to `$OVERLAY_PREVIEW` for a visual check.
    #[test]
    #[ignore = "writes a preview image"]
    fn preview() {
        let path = std::env::var("OVERLAY_PREVIEW").expect("set OVERLAY_PREVIEW to a .png path");
        let banner = banner("Counter-Strike 2", "Profile on · 1600 DPI", 2.0);
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut encoder = png::Encoder::new(file, banner.width.into(), banner.height.into());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        // PNG wants straight alpha.
        let straight: Vec<u8> = banner
            .pixels
            .chunks(4)
            .flat_map(|pixel| {
                let alpha = pixel[3];
                let unpremultiply = |channel: u8| {
                    if alpha == 0 {
                        0
                    } else {
                        (u16::from(channel) * 255 / u16::from(alpha)) as u8
                    }
                };
                [
                    unpremultiply(pixel[0]),
                    unpremultiply(pixel[1]),
                    unpremultiply(pixel[2]),
                    alpha,
                ]
            })
            .collect();
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&straight)
            .unwrap();
    }
}
