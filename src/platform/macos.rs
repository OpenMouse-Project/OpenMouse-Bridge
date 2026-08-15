use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::BaseDirs;

const LABEL: &str = "io.OpenMouse.OpenMouse-Bridge";

pub const fn platform_name() -> &'static str {
    "macos"
}

pub fn autostart_enabled() -> bool {
    launch_agent_path().is_ok_and(|path| path.exists())
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    let path = launch_agent_path()?;
    if !enabled {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
        }
        return Ok(());
    }

    let executable = env::current_exe().context("could not locate the Bridge executable")?;
    let executable = xml_escape(&executable.to_string_lossy());
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{executable}</string></array>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
"#
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(&path, plist).with_context(|| format!("could not write {}", path.display()))
}

fn launch_agent_path() -> Result<PathBuf> {
    let home = BaseDirs::new().context("macOS did not provide a home directory")?;
    Ok(home
        .home_dir()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_paths_are_escaped_for_plists() {
        assert_eq!(xml_escape("A&B <Bridge>"), "A&amp;B &lt;Bridge&gt;");
    }
}
