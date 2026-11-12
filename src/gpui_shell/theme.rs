//! The Matt dark palette from OpenMouse Desktop (`gpui/src/theme.rs` there),
//! trimmed to what Bridge needs for now: the dark shell with the green accent.
//! Theme choice, vibrancy and the wallpaper-derived accent are not ported.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Hsla};

/// Default accent `#5dde89`.
const ACCENT: (u8, u8, u8) = (93, 222, 137);
/// Text on the accent, from Desktop's `readable_ink` for a light accent.
const ACCENT_INK: (u8, u8, u8) = (16, 16, 18);

fn to_hsla(rgb: (u8, u8, u8)) -> Hsla {
    let (r, g, b) = (
        rgb.0 as f32 / 255.0,
        rgb.1 as f32 / 255.0,
        rgb.2 as f32 / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return Hsla {
            h: 0.0,
            s: 0.0,
            l,
            a: 1.0,
        };
    }
    let d = max - min;
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if (max - r).abs() < f32::EPSILON {
        60.0 * (((g - b) / d) % 6.0)
    } else if (max - g).abs() < f32::EPSILON {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    Hsla {
        h: ((h + 360.0) % 360.0) / 360.0,
        s: s.clamp(0.0, 1.0),
        l: l.clamp(0.0, 1.0),
        a: 1.0,
    }
}

/// Paints the dark shell onto the live theme.
pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let background = to_hsla((13, 13, 15)); // --sidebar #0d0d0f
    let border = to_hsla((41, 41, 45)); // --line #29292d
    let muted = to_hsla((133, 133, 138)); // --muted #85858a
    let muted_foreground = to_hsla((119, 119, 124)); // --faint #77777c
    let foreground = to_hsla((247, 247, 247)); // --text #f7f7f7
    let accent = to_hsla(ACCENT);
    let accent_ink = to_hsla(ACCENT_INK);
    Theme::update(cx, |theme| {
        let c = &mut theme.colors;
        c.background = background;
        c.foreground = foreground;
        c.muted = muted;
        c.muted_foreground = muted_foreground;
        c.sidebar = background;
        c.border = border;
        c.accent = accent;
        c.accent_foreground = accent_ink;
        c.primary = accent;
        c.primary_foreground = accent_ink;
        c.title_bar = background;
        c.title_bar_border = border;
        c.scrollbar = Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.0,
            a: 0.0,
        };
        c.scrollbar_thumb = border;
        c.scrollbar_thumb_hover = muted;
    });
}
