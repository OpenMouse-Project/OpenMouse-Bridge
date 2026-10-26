use anyhow::Result;
#[cfg(target_os = "macos")]
use anyhow::{Context, anyhow};

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(any(target_os = "linux", test))]
pub mod linux;

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
mod portable;

#[cfg(target_os = "windows")]
pub use windows::{
    autostart_enabled, battery_icon_rgba, light_tray_theme, linux_distribution, platform_name,
    set_autostart, tray_icon_size,
};

#[cfg(target_os = "macos")]
pub use macos::{autostart_enabled, linux_distribution, platform_name, set_autostart};

#[cfg(target_os = "linux")]
pub use linux::{autostart_enabled, linux_distribution, platform_name, set_autostart};

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub use portable::{autostart_enabled, linux_distribution, platform_name, set_autostart};

/// The pixel size a tray icon should be drawn at, in physical pixels.
/// Windows reports the notification area's small-icon size for the current
/// system DPI (16 at 100%, 20/24/32 as scaling rises); other desktops get a
/// fixed size that reads well in menu bars and appindicator hosts.
#[cfg(not(target_os = "windows"))]
pub fn tray_icon_size() -> u32 {
    22
}

/// Whether the shell paints its surfaces light. Only Windows reports it;
/// macOS menu bars and Linux appindicators are dark in practice.
#[cfg(not(target_os = "windows"))]
pub fn light_tray_theme() -> bool {
    false
}

/// What the tray battery icon shows. The Windows 11 taskbar only renders
/// square tray icons correctly, so the glyph and the number are two
/// mutually exclusive styles chosen in the control panel, not one wide
/// display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayBatteryStyle {
    /// The native Segoe battery glyph alone, filling the square icon.
    Glyph,
    /// The percentage number alone in bold text, filling the square icon.
    Percentage,
}

impl TrayBatteryStyle {
    /// The config value this style is stored as.
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Glyph => "glyph",
            Self::Percentage => "percentage",
        }
    }

    /// Reads a stored config value; anything unknown falls back to the
    /// default glyph.
    pub fn from_config_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "percentage" => Self::Percentage,
            _ => Self::Glyph,
        }
    }
}

/// Renders the tray battery display as an RGBA buffer in `ink`. Windows
/// draws either the Segoe Fluent battery glyph or the percentage number in
/// bold UI text, per `style`; other desktops get a simple monochrome
/// fallback: a bold outline battery with the charge fill, no anti-aliasing
/// (the style is ignored there).
#[cfg(not(target_os = "windows"))]
pub fn battery_icon_rgba(
    percent: u8,
    charging: bool,
    style: TrayBatteryStyle,
    size: u32,
    ink: (u8, u8, u8),
) -> Result<(Vec<u8>, u32, u32)> {
    let _ = style;
    let size = size.max(8);
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let mut put = |x0: u32, y0: u32, x1: u32, y1: u32| {
        for y in y0..y1.min(size) {
            for x in x0..x1.min(size) {
                let index = ((y * size + x) * 4) as usize;
                rgba[index] = ink.0;
                rgba[index + 1] = ink.1;
                rgba[index + 2] = ink.2;
                rgba[index + 3] = u8::MAX;
            }
        }
    };
    let stroke = (size / 10).max(1);
    let body_height = size * 2 / 3;
    let body_top = (size - body_height) / 2;
    let body_bottom = body_top + body_height;
    let nub_width = (size / 16).max(1);
    let body_width = size.saturating_sub(nub_width + 2);
    // The shell, a nub on the right, and the charge fill inside.
    put(0, body_top, body_width, body_top + stroke);
    put(0, body_bottom - stroke, body_width, body_bottom);
    put(0, body_top, stroke, body_bottom);
    put(body_width - stroke, body_top, body_width, body_bottom);
    put(
        body_width + 1,
        size / 3,
        body_width + 1 + nub_width,
        size * 2 / 3,
    );
    let fill = if charging {
        100
    } else {
        u32::from(percent.min(100))
    };
    let inner_x0 = stroke;
    let inner_x1 = body_width - stroke;
    let inner_width = inner_x1 - inner_x0;
    let fill_columns = fill * inner_width / 100;
    put(
        inner_x0,
        body_top + stroke,
        inner_x0 + fill_columns,
        body_bottom - stroke,
    );
    Ok((rgba, size, size))
}

pub fn notify(summary: &str, body: &str) -> Result<()> {
    // notify-rust resolves a bundle identifier on macOS. Run as a bare binary
    // (e.g. `cargo run`) there is none, and macOS pops a blocking "Choose
    // Application" dialog instead of a notification. Use osascript there, which
    // posts a banner from any process; the signed .app uses notify-rust so the
    // notification is attributed to OpenMouse Bridge.
    #[cfg(target_os = "macos")]
    if !macos_is_bundled() {
        return macos_script_notify(summary, body);
    }
    notify_rust::Notification::new()
        .appname("OpenMouse Bridge")
        .summary(summary)
        .body(body)
        .show()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_is_bundled() -> bool {
    std::env::current_exe()
        .map(|path| path.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn macos_script_notify(summary: &str, body: &str) -> Result<()> {
    fn escape(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape(body),
        escape(summary)
    );
    let status = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
        .context("could not run osascript for the notification")?;
    if !status.success() {
        return Err(anyhow!("osascript could not post the notification"));
    }
    Ok(())
}
