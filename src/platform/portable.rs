use anyhow::{Result, bail};

pub const fn platform_name() -> &'static str {
    std::env::consts::OS
}

pub fn autostart_enabled() -> bool {
    false
}

pub fn set_autostart(_enabled: bool) -> Result<()> {
    bail!("autostart is currently implemented only for Windows")
}
