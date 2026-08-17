use anyhow::Result;

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
    // Application" dialog instead of a notification. Only notify from a real
    // .app bundle; the shipped, signed build satisfies this.
    #[cfg(target_os = "macos")]
    if !macos_is_bundled() {
        tracing::debug!("Skipping notification; Bridge is not running as a .app bundle");
        return Ok(());
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
