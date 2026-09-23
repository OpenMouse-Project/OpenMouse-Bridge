//! The on-screen banner, and its chime, for profile switches and low-battery
//! alerts.
//!
//! It is drawn by Bridge itself rather than sent as a system notification, so
//! it appears over a borderless game immediately and never takes its focus.

mod chime;
mod render;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod platform;

use std::time::{Duration, Instant};

use anyhow::Result;

const FADE: Duration = Duration::from_millis(180);
const HOLD: Duration = Duration::from_millis(2600);
const FRAME: Duration = Duration::from_millis(16);

pub struct Overlay {
    window: platform::Window,
    speaker: platform::Speaker,
    shown_at: Option<Instant>,
}

impl Overlay {
    pub fn new() -> Result<Self> {
        Ok(Self {
            window: platform::Window::new()?,
            speaker: platform::Speaker::default(),
            shown_at: None,
        })
    }

    /// Shows `title` over `detail`, replacing any banner already on screen.
    pub fn show(&mut self, title: &str, detail: &str) -> Result<()> {
        let banner = render::banner(title, detail, self.window.scale());
        self.window.present(&banner)?;
        self.shown_at = Some(Instant::now());
        Ok(())
    }

    /// Plays the chime at `volume` (0–100), cutting off one still playing.
    pub fn chime(&mut self, volume: u8) -> Result<()> {
        self.speaker.play(chime::wav(volume))
    }

    /// Advances the fade in, hold and fade out. Returns how soon it needs the
    /// next call, or `None` once the banner is gone.
    pub fn tick(&mut self) -> Option<Duration> {
        let elapsed = self.shown_at?.elapsed();
        let (opacity, next) = if elapsed < FADE {
            (elapsed.as_secs_f32() / FADE.as_secs_f32(), FRAME)
        } else if elapsed < FADE + HOLD {
            (1.0, FADE + HOLD - elapsed)
        } else if elapsed < FADE * 2 + HOLD {
            let out = elapsed - FADE - HOLD;
            (1.0 - out.as_secs_f32() / FADE.as_secs_f32(), FRAME)
        } else {
            self.window.hide();
            self.shown_at = None;
            return None;
        };
        self.window.set_opacity(opacity);
        Some(next)
    }
}
