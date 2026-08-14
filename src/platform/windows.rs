use std::{env, process::Command};

use anyhow::{Context, Result, bail};

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "OpenMouse Bridge";

pub const fn platform_name() -> &'static str {
    "windows"
}

pub fn autostart_enabled() -> bool {
    Command::new("reg")
        .args(["query", RUN_KEY, "/v", VALUE_NAME])
        .output()
        .is_ok_and(|output| output.status.success())
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    let mut command = Command::new("reg");
    if enabled {
        let executable = env::current_exe().context("could not locate the Bridge executable")?;
        let value = format!("\"{}\"", executable.display());
        command.args([
            "add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &value, "/f",
        ]);
    } else {
        command.args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"]);
    }
    let output = command
        .output()
        .context("could not run the Windows registry tool")?;
    if !output.status.success() {
        bail!(
            "Windows rejected the autostart change: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
