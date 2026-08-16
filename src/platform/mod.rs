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
    notify_rust::Notification::new()
        .appname("OpenMouse Bridge")
        .summary(summary)
        .body(body)
        .show()?;
    Ok(())
}
