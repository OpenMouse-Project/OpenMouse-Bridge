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
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

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

    /// Plays the notification sound at `volume` (0–100), `repeats` times in a
    /// row, cutting off one still playing. It is the `.wav` file at
    /// `sound_path` (a bare name is looked up beside `config.json`) or, when
    /// that is empty, the built-in chime. If the file cannot be used the chime
    /// plays instead and the reason comes back as `Some`.
    pub fn chime(&mut self, volume: u8, repeats: u32, sound_path: &str) -> Result<Option<String>> {
        let mut problem = None;
        let mut wav = None;
        if !sound_path.trim().is_empty() {
            let config_dir: Option<PathBuf> = openmouse_bridge::config::config_path()
                .ok()
                .and_then(|path| path.parent().map(Path::to_path_buf));
            let path = chime::resolve_sound_path(sound_path, config_dir.as_deref());
            match chime::custom_wav(&path, volume, repeats) {
                Ok(bytes) => wav = Some(bytes),
                Err(reason) => {
                    problem = Some(format!("Notification sound not used: {reason}"));
                }
            }
        }
        let wav = wav.unwrap_or_else(|| chime::wav(volume, repeats));
        self.speaker.play(wav)?;
        Ok(problem)
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
