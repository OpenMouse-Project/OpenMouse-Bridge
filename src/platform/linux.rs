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
    let desktop = launch_entry()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(&path, desktop).with_context(|| format!("could not write {}", path.display()))
}

/// Points an existing login entry at the Bridge that is running, when it
/// names something else: an AppImage entry written before the `$APPIMAGE`
/// fix names a `/tmp/.mount_…` path that is gone by the next login, and an
/// AppImage moved or renamed leaves its entry behind. Leaves a turned-off
/// entry (none) alone. Returns whether it rewrote the entry.
pub fn repair_autostart() -> Result<bool> {
    let path = autostart_path()?;
    let Ok(current) = fs::read_to_string(&path) else {
        return Ok(false);
    };
    let wanted = repair_entry(&current, &launch_entry()?);
    if current == wanted {
        return Ok(false);
    }
    fs::write(&path, wanted).with_context(|| format!("could not write {}", path.display()))?;
    Ok(true)
}

/// Preserve desktop startup preferences and custom keys; only refresh Exec.
fn repair_entry(current: &str, launch: &str) -> String {
    let exec = launch
        .lines()
        .find(|line| line.starts_with("Exec="))
        .unwrap();
    let mut desktop = false;
    current
        .split_inclusive('\n')
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                desktop = trimmed == "[Desktop Entry]";
            }
            if desktop && line.starts_with("Exec=") {
                format!("{exec}{}", if line.ends_with('\n') { "\n" } else { "" })
            } else {
                line.to_owned()
            }
        })
        .collect()
}

/// The login entry for the Bridge that is running.
fn launch_entry() -> Result<String> {
    let executable = launch_path(env::var_os("APPIMAGE"), env::current_exe())?;
    desktop_entry(
        executable
            .to_str()
            .context("Bridge executable path is not Unicode")?,
    )
}

/// What starts this Bridge again. In an AppImage the executable is inside a
/// mount under `/tmp` that only exists while it runs; the runtime sets
/// `$APPIMAGE` to the image file itself, which is what has to be launched.
fn launch_path(
    appimage: Option<std::ffi::OsString>,
    current_exe: std::io::Result<PathBuf>,
) -> Result<PathBuf> {
    if let Some(image) = appimage.map(PathBuf::from)
        && image.is_absolute()
        && image.is_file()
    {
        return Ok(image);
    }
    current_exe.context("could not locate the Bridge executable")
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

    #[test]
    fn repair_preserves_disabled_flags_and_custom_keys() {
        let entry = "[Desktop Entry]\nExec=/old\nHidden=true\nX-GNOME-Autostart-enabled=false\nOnlyShowIn=KDE;\n";
        assert_eq!(
            repair_entry(entry, "[Desktop Entry]\nExec=/new\n"),
            entry.replace("Exec=/old", "Exec=/new")
        );
    }

    #[test]
    fn an_appimage_launches_the_image_not_its_mount() {
        let image = std::env::temp_dir().join(format!("openmouse-{}.AppImage", std::process::id()));
        fs::write(&image, b"").unwrap();
        let mount = PathBuf::from("/tmp/.mount_AbC123/usr/bin/openmouse-bridge");
        assert_eq!(
            launch_path(Some(image.clone().into()), Ok(mount.clone())).unwrap(),
            image
        );
        // Not an AppImage, or a `$APPIMAGE` that names no file: the executable.
        assert_eq!(launch_path(None, Ok(mount.clone())).unwrap(), mount);
        assert_eq!(
            launch_path(Some("/nowhere/x.AppImage".into()), Ok(mount.clone())).unwrap(),
            mount
        );
        assert_eq!(
            launch_path(Some("relative.AppImage".into()), Ok(mount.clone())).unwrap(),
            mount
        );
        let _ = fs::remove_file(image);
    }
}
