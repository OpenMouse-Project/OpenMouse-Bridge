use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::BaseDirs;

const DESKTOP_FILE_NAME: &str = "io.openmouse.bridge.desktop";

pub const fn platform_name() -> &'static str {
    "linux"
}

pub fn linux_distribution() -> Option<String> {
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
    autostart_path().is_ok_and(|path| path.exists())
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    let path = autostart_path()?;
    if !enabled {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
        }
        return Ok(());
    }
    let executable = env::current_exe().context("could not locate the Bridge executable")?;
    let desktop = format!(
        "[Desktop Entry]\nType=Application\nName=OpenMouse Bridge\nExec=\"{}\"\nHidden=false\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
        executable.display(),
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(&path, desktop).with_context(|| format!("could not write {}", path.display()))
}

fn autostart_path() -> Result<PathBuf> {
    let home = BaseDirs::new().context("the operating system did not provide a home directory")?;
    Ok(home
        .home_dir()
        .join(".config/autostart")
        .join(DESKTOP_FILE_NAME))
}

#[cfg(test)]
mod tests {
    #[test]
    fn desktop_entry_quotes_executable_paths() {
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=OpenMouse Bridge\nExec=\"{}\"\n",
            "/opt/openmouse/openmouse-bridge"
        );
        assert!(entry.contains("Exec=\"/opt/openmouse/openmouse-bridge\""));
    }
}
