use anyhow::Result;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(not(target_os = "windows"))]
mod portable;

#[cfg(target_os = "windows")]
pub use windows::{autostart_enabled, platform_name, set_autostart};

#[cfg(not(target_os = "windows"))]
pub use portable::{autostart_enabled, platform_name, set_autostart};

pub fn notify(summary: &str, body: &str) -> Result<()> {
    notify_rust::Notification::new()
        .appname("OpenMouse Bridge")
        .summary(summary)
        .body(body)
        .show()?;
    Ok(())
}
