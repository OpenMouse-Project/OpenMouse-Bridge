use anyhow::Result;
#[cfg(target_os = "macos")]
use anyhow::{Context, anyhow};

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod portable;

#[cfg(target_os = "windows")]
pub use windows::{autostart_enabled, linux_distribution, platform_name, set_autostart};

#[cfg(target_os = "macos")]
pub use macos::{autostart_enabled, linux_distribution, platform_name, set_autostart};

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub use portable::{autostart_enabled, linux_distribution, platform_name, set_autostart};

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
