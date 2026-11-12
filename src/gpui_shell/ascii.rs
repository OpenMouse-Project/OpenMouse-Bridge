//! The ASCII density field painted behind the panel, ported from OpenMouse
//! Desktop (`render_ascii_background` and its noise helpers in `gpui/src/app.rs`).
//! Deterministic, no animation after the entrance sweep, and only regenerated
//! when the character grid changes size.

use std::time::Duration;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

const RAMP: &[u8] = b" .:-=+*x#%@";
/// Cell width as a fraction of the font size. Deliberately under every common
/// monospace advance, so the last columns overflow and get clipped instead of
/// leaving a bare strip on the right.
const CELL_RATIO: f32 = 0.5;
/// Ink opacity at the corner the field starts from.
const ALPHA: f32 = 0.09;
/// Desktop's ink is tuned for a large window; on the small panel it needs more
/// to read at all (Desktop applies the same idea as a gain on OLED black).
const GAIN: f32 = 2.4;
const SWEEP_MS: u64 = 520;
/// How far a row starts to the right of its resting place, in px.
const SWEEP_OFFSET: f32 = 220.0;
/// Share of the sweep spent waiting for the wave to reach the last row.
const SWEEP_STAGGER: f32 = 0.55;
/// How far the falloff reaches, as a fraction of the corner-to-corner distance.
const FADE_REACH: f32 = 0.9;
/// Number of alpha bands per row; each is its own text run.
const BANDS: usize = 6;

/// Falloff from the top-right corner, slightly wider than tall. `u` runs
/// left to right, `v` top to bottom; returns 0..1.
fn falloff(u: f32, v: f32) -> f32 {
    let dx = 1.0 - u;
    let distance = (dx * dx + v * v * 0.8).sqrt();
    let s = (1.0 - distance / FADE_REACH).clamp(0.0, 1.0);
    s * s * (3.0 - 2.0 * s)
}

/// Ink opacity for one cell region; fades with the density in both directions.
fn cell_alpha(u: f32, v: f32) -> f32 {
    ALPHA * falloff(u, v).sqrt()
}

/// Deterministic lattice hash to 0..1.
fn hash(x: i32, y: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    (h & 0xFFFF) as f32 / 65535.0
}

/// Smooth value noise, 0..1.
fn value_noise(x: f32, y: f32) -> f32 {
    let (xi, yi) = (x.floor(), y.floor());
    let (fx, fy) = (x - xi, y - yi);
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let (xi, yi) = (xi as i32, yi as i32);
    let top = hash(xi, yi) + (hash(xi + 1, yi) - hash(xi, yi)) * sx;
    let bottom = hash(xi, yi + 1) + (hash(xi + 1, yi + 1) - hash(xi, yi + 1)) * sx;
    top + (bottom - top) * sy
}

/// Three-octave fractal noise, 0..1.
fn fbm(x: f32, y: f32) -> f32 {
    let (mut sum, mut amp, mut freq, mut norm) = (0.0, 1.0, 1.0, 0.0);
    for octave in 0..3 {
        let o = octave as f32 * 17.0;
        sum += amp * value_noise(x * freq + o, y * freq - o);
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm
}

/// Interleaved gradient noise, 0..1: a dither without a visible grid.
fn dither(x: usize, y: usize) -> f32 {
    (52.982_92 * ((0.067_110_56 * x as f32 + 0.005_837_15 * y as f32).fract())).fract()
}

/// One character row per screen row. Density is highest at the top-right
/// corner and falls away toward the bottom-left, broken into cloud-like
/// contours by fractal noise.
fn build_rows(cols: usize, rows: usize) -> Vec<SharedString> {
    let last = (RAMP.len() - 1) as f32;
    let mut out = Vec::with_capacity(rows);
    for y in 0..rows {
        let v = (y as f32 + 0.5) / rows as f32;
        let mut line = String::with_capacity(cols);
        for x in 0..cols {
            let u = (x as f32 + 0.5) / cols as f32;
            let cloud = fbm(x as f32 * 0.07, y as f32 * 0.14);
            let t = (falloff(u, v) * (0.3 + 1.2 * cloud))
                .clamp(0.0, 1.0)
                .powf(1.25);
            let dithered = t * last + (dither(x, y) - 0.5) * 0.9;
            line.push(RAMP[dithered.round().clamp(0.0, last) as usize] as char);
        }
        out.push(line.into());
    }
    out
}

/// The generated rows, kept between frames.
#[derive(Default)]
pub struct Field {
    rows: Vec<SharedString>,
    dims: (usize, usize),
}

impl Field {
    /// The field as an absolutely positioned layer to put behind the content.
    pub fn element(&mut self, window: &Window, cx: &App) -> AnyElement {
        let font_size = cx.theme().mono_font_size;
        let line_height = font_size.as_f32();
        let viewport = window.viewport_size();
        let cols = ((viewport.width.as_f32() / (line_height * CELL_RATIO)).ceil() as usize).max(1);
        let grid_rows = ((viewport.height.as_f32() / line_height).ceil() as usize).max(1);
        if self.dims != (cols, grid_rows) {
            self.rows = build_rows(cols, grid_rows);
            self.dims = (cols, grid_rows);
        }
        let font_family = cx.theme().mono_font_family.clone();
        let color = cx.theme().primary;
        let total = self.rows.len();
        let rows: Vec<(SharedString, f32)> = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, line)| (line.clone(), (index as f32 + 0.5) / total as f32))
            .collect();
        div()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .w_full()
            .h_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .with_animation(
                "ascii-field",
                Animation::new(Duration::from_millis(SWEEP_MS)),
                move |field, delta| {
                    field.children(rows.iter().map(|(line, position)| {
                        // The wave reaches each row in turn; a row holds its
                        // start offset until then.
                        let local = ((delta - position * SWEEP_STAGGER) / (1.0 - SWEEP_STAGGER))
                            .clamp(0.0, 1.0);
                        let eased = 1.0 - (1.0 - local).powi(3);
                        div()
                            .h(px(line_height))
                            .flex_none()
                            .left(px((1.0 - eased) * SWEEP_OFFSET))
                            .font_family(font_family.clone())
                            .text_size(font_size)
                            .line_height(px(line_height))
                            .whitespace_nowrap()
                            .child(div().flex().flex_none().children((0..BANDS).filter_map(
                                |band| {
                                    let start = band * line.len() / BANDS;
                                    let end = (band + 1) * line.len() / BANDS;
                                    if start == end {
                                        return None;
                                    }
                                    let u = ((start + end) as f32 * 0.5) / line.len() as f32;
                                    Some(
                                        div()
                                            .flex_none()
                                            .text_color(Hsla {
                                                a: (cell_alpha(u, *position) * GAIN).min(1.0),
                                                ..color
                                            })
                                            .child(SharedString::from(
                                                line[start..end].to_string(),
                                            )),
                                    )
                                },
                            )))
                    }))
                },
            )
            .into_any_element()
    }
}
