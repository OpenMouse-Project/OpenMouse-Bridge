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
    let desktop = desktop_entry(
        executable
            .to_str()
            .context("Bridge executable path is not Unicode")?,
    )?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(&path, desktop).with_context(|| format!("could not write {}", path.display()))
}

fn autostart_path() -> Result<PathBuf> {
    let home = BaseDirs::new().context("the operating system did not provide a home directory")?;
    Ok(home.config_dir().join("autostart").join(DESKTOP_FILE_NAME))
}

fn desktop_entry(executable: &str) -> Result<String> {
    anyhow::ensure!(
        !executable
            .chars()
            .any(|character| character.is_control() || character == '='),
        "Bridge executable path cannot be represented in a desktop entry"
    );
    let executable = executable
        .chars()
        .fold(String::new(), |mut value, character| {
            match character {
                '%' => value.push_str("%%"),
                '\\' => value.push_str(r"\\\\"),
                '"' | '$' | '`' => {
                    value.push_str(r"\\");
                    value.push(character);
                }
                _ => value.push(character),
            }
            value
        });
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=OpenMouse Bridge\nExec=\"{executable}\"\nHidden=false\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_preserves_literal_exec_metacharacters() {
        let entry = desktop_entry("/opt/50%/A \"$`B\\bridge").unwrap();
        assert!(
            entry.contains(r#"Exec="/opt/50%%/A \\"\\$\\`B\\\\bridge""#),
            "{entry}"
        );
        assert!(desktop_entry("/opt/bad\nentry").is_err());
    }
}
