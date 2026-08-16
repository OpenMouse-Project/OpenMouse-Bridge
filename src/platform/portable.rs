use anyhow::{Result, bail};

pub const fn platform_name() -> &'static str {
    std::env::consts::OS
}

pub fn linux_distribution() -> Option<String> {
    if std::env::consts::OS != "linux" {
        return None;
    }
    let contents = std::fs::read_to_string("/etc/os-release").ok()?;
    let values = contents
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| *key == "ID" || *key == "ID_LIKE")
        .map(|(_, value)| value.trim_matches('"').to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    (!values.is_empty()).then_some(values)
}

pub fn autostart_enabled() -> bool {
    false
}

pub fn set_autostart(_enabled: bool) -> Result<()> {
    bail!("autostart is currently implemented only for Windows")
}
