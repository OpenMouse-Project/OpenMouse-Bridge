//! Linux hidraw permissions: whether Bridge can reach the mice it has drivers
//! for, and installing the udev rule that lets it.
//!
//! `/dev/hidraw*` nodes are root-only unless a udev rule opens them up.
//! `install-linux.sh` installs that rule, but an AppImage or a source build
//! never runs it, so Bridge checks for itself and offers to install the same
//! rule through `pkexec` (the desktop's own password prompt).

/// Where the rule goes; the same file `install-linux.sh` writes and removes.
pub const RULE_PATH: &str = "/etc/udev/rules.d/69-openmouse-bridge.rules";

/// USB vendor ids the rule opens hidraw access for. Kept equal to
/// `VENDOR_IDS` in `install-linux.sh` (a test checks).
pub const VENDOR_IDS: &[u16] = &[
    0x045e, 0x046d, 0x0483, 0x093a, 0x1532, 0x1915, 0x1caa, 0x1d57, 0x248a, 0x25a7, 0x2fe3, 0x31e3,
    0x3151, 0x3367, 0x3434, 0x3554, 0x361d, 0x36a7, 0x3710, 0x373b, 0x373e, 0x3879,
];

/// The udev rule: the same rules `install-linux.sh` writes.
pub fn rule() -> String {
    let mut rule = String::from(
        "# OpenMouse Bridge: let the signed-in user reach gaming mice over hidraw.\n\
         # Installed by OpenMouse Bridge or install-linux.sh; safe to delete on uninstall.\n",
    );
    for vendor in VENDOR_IDS {
        rule.push_str(&format!(
            "SUBSYSTEM==\"hidraw\", ATTRS{{idVendor}}==\"{vendor:04x}\", MODE=\"0666\", TAG+=\"uaccess\"\n"
        ));
    }
    rule.push_str(
        "# The button listener presses keys and side buttons through a virtual keyboard and mouse.\n\
         KERNEL==\"uinput\", MODE=\"0660\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"\n",
    );
    rule
}

/// The root shell script that installs [`rule`] (passed as `$1`) and applies
/// it to devices already plugged in, so no replug or reboot is needed.
const INSTALL_SCRIPT: &str = r#"set -eu
printf '%s' "$1" > /etc/udev/rules.d/69-openmouse-bridge.rules
chmod 0644 /etc/udev/rules.d/69-openmouse-bridge.rules
udevadm control --reload-rules
udevadm trigger --subsystem-match=hidraw --action=change
udevadm trigger --subsystem-match=misc --sysname-match=uinput --action=change || true
udevadm settle --timeout=5 || true
"#;

/// The terminal command that installs a saved copy of the rule at `copy`, for
/// when `pkexec` is missing or fails.
pub fn manual_command(copy: &std::path::Path) -> String {
    format!(
        "sudo install -m 0644 '{}' {RULE_PATH} && sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=hidraw --action=change",
        copy.display()
    )
}

/// Saves the rule under the user's cache directory for [`manual_command`].
pub fn save_rule_copy() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context as _;
    let dirs = directories::BaseDirs::new().context("could not find the cache directory")?;
    let dir = dirs.cache_dir().join("openmouse-bridge");
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    let path = dir.join("69-openmouse-bridge.rules");
    std::fs::write(&path, rule()).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// The vendor id from a hidraw device's `uevent` when it sits on the USB bus
/// (`HID_ID=0003:0000046D:0000C54D`). Bluetooth and other buses are ignored:
/// the rule matches the USB `idVendor` attribute, so it cannot help them.
fn usb_vendor(uevent: &str) -> Option<u16> {
    let id = uevent
        .lines()
        .find_map(|line| line.strip_prefix("HID_ID="))?;
    let mut parts = id.split(':');
    let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
    let vendor = u32::from_str_radix(parts.next()?, 16).ok()?;
    (bus == 0x0003)
        .then_some(vendor)
        .and_then(|v| u16::try_from(v).ok())
}

#[cfg(target_os = "linux")]
mod imp {
    use std::{ffi::CString, fs, os::unix::ffi::OsStrExt, path::Path, process::Command};

    use anyhow::{Context, Result, bail};

    use super::{INSTALL_SCRIPT, VENDOR_IDS, rule, usb_vendor};

    /// The `/dev/hidraw*` nodes of supported USB mice that Bridge cannot open
    /// for reading and writing. Empty when all are reachable or none are
    /// plugged in.
    pub fn blocked_devices() -> Vec<String> {
        let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
            return Vec::new();
        };
        let mut blocked: Vec<String> = entries
            .flatten()
            .filter(|entry| {
                fs::read_to_string(entry.path().join("device/uevent"))
                    .ok()
                    .and_then(|uevent| usb_vendor(&uevent))
                    .is_some_and(|vendor| VENDOR_IDS.contains(&vendor))
            })
            .map(|entry| Path::new("/dev").join(entry.file_name()))
            .filter(|node| !accessible(node))
            .map(|node| node.display().to_string())
            .collect();
        blocked.sort();
        blocked
    }

    /// Whether this process may open `node` read-write. Only a permission
    /// error counts as blocked: a node that vanished mid-scan is not.
    fn accessible(node: &Path) -> bool {
        let Ok(path) = CString::new(node.as_os_str().as_bytes()) else {
            return true;
        };
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        if unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::EACCES)
    }

    /// Installs the rule as root through `pkexec`, which shows the desktop's
    /// password prompt. Blocks until the user answers.
    pub fn install_rule() -> Result<()> {
        let output = Command::new("pkexec")
            .args(["/bin/sh", "-c", INSTALL_SCRIPT, "openmouse-bridge-udev"])
            .arg(rule())
            .output()
            .context("could not run pkexec; is polkit installed?")?;
        match output.status.code() {
            Some(0) => Ok(()),
            // pkexec: 126 when the user dismissed the prompt, 127 when not
            // authorized or no polkit agent is running.
            Some(126) => bail!("the password prompt was cancelled"),
            Some(127) => bail!("not authorized, or no polkit agent is running"),
            _ => bail!(
                "installing the udev rule failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use anyhow::{Result, bail};

    pub fn blocked_devices() -> Vec<String> {
        Vec::new()
    }

    pub fn install_rule() -> Result<()> {
        bail!("udev rules are Linux-only")
    }
}

pub use imp::{blocked_devices, install_rule};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_ids_match_the_installer() {
        let script = include_str!("../install-linux.sh");
        let line = script
            .lines()
            .find_map(|line| line.strip_prefix("VENDOR_IDS=\""))
            .expect("install-linux.sh defines VENDOR_IDS");
        let installer: Vec<u16> = line
            .trim_end_matches('"')
            .split_whitespace()
            .map(|id| u16::from_str_radix(id, 16).unwrap())
            .collect();
        assert_eq!(installer, VENDOR_IDS);
    }

    #[test]
    fn rule_has_one_hidraw_line_per_vendor_and_the_uinput_line() {
        let rule = rule();
        assert_eq!(
            rule.lines()
                .filter(|line| line.starts_with("SUBSYSTEM==\"hidraw\""))
                .count(),
            VENDOR_IDS.len()
        );
        assert!(rule.contains(r#"ATTRS{idVendor}=="046d", MODE="0666", TAG+="uaccess""#));
        assert!(rule.contains(r#"KERNEL=="uinput""#));
    }

    #[test]
    fn only_usb_vendors_count() {
        assert_eq!(
            usb_vendor("DRIVER=logitech-djreceiver\nHID_ID=0003:0000046D:0000C54D\n"),
            Some(0x046d)
        );
        // Bluetooth: the USB idVendor rule cannot open it up.
        assert_eq!(usb_vendor("HID_ID=0005:0000046D:0000B023\n"), None);
        assert_eq!(usb_vendor("HID_NAME=no id\n"), None);
    }
}
