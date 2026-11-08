//! Linux hidraw permissions: whether Bridge can reach the mice plugged in,
//! and installing the udev rule that lets it.
//!
//! `/dev/hidraw*` nodes are root-only unless a udev rule opens them up.
//! `install-linux.sh` installs that rule, but an AppImage or a source build
//! never runs it, so Bridge checks for itself and offers to install the same
//! rule through `pkexec` (the desktop's own password prompt).
//!
//! The rule is not a vendor list: it opens every hidraw node of a USB device
//! that has a mouse interface (a mouse, or the receiver it pairs with). That
//! includes the device's vendor-defined interface, which is where settings
//! are read and written, so a new brand works without a rule change.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Where the rule goes; the same file `install-linux.sh` writes and removes.
pub const RULE_PATH: &str = "/etc/udev/rules.d/69-openmouse-bridge.rules";

/// Where the rule's helper goes. Next to the rules directory rather than
/// under `/usr`, which is read-only on image-based distros.
pub const HELPER_PATH: &str = "/etc/udev/openmouse-bridge-is-mouse";

/// The udev rule. `install-linux.sh` writes the same text (a test checks).
///
/// `uaccess` alone, without `MODE="0666"`: the user at the seat gets access
/// through an ACL, other accounts on the machine do not. That matters because
/// a receiver or keyboard with a mouse interface also exposes its keyboard
/// interface here.
pub const RULE: &str = r#"# OpenMouse Bridge: let the signed-in user reach mice over hidraw.
# Installed by OpenMouse Bridge or install-linux.sh; safe to delete on uninstall.
# Every hidraw node of a USB device with a mouse interface (a mouse or its
# receiver), including the vendor interface its settings go through.
SUBSYSTEM=="hidraw", SUBSYSTEMS=="usb", PROGRAM="/bin/sh /etc/udev/openmouse-bridge-is-mouse %S%p", TAG+="uaccess"
# The button listener presses keys and side buttons through a virtual keyboard and mouse.
KERNEL=="uinput", MODE="0660", TAG+="uaccess", OPTIONS+="static_node=uinput"
"#;

/// The rule's helper: exits 0 when the hidraw node at sysfs path `$1` belongs
/// to a USB device with a mouse interface. [`usb_device`] and
/// [`has_mouse_interface`] are the same check in Rust; a test runs both on
/// the same fake sysfs trees. `install-linux.sh` writes the same text.
pub const HELPER: &str = r##"#!/bin/sh
# OpenMouse Bridge udev helper for /etc/udev/rules.d/69-openmouse-bridge.rules;
# safe to delete with it. Succeeds when the hidraw device at sysfs path $1
# belongs to a USB device with a mouse interface: a boot-protocol mouse, or a
# HID interface whose report descriptor has a Generic Desktop Mouse collection.
dev=$(readlink -f "$1/device") || exit 1
while [ ! -f "$dev/bNumInterfaces" ]; do
    [ "$dev" != / ] || exit 1
    dev=$(dirname "$dev")
done
interfaces="$dev/${dev##*/}:"
# On hotplug the interfaces register one by one: give the rest up to a second.
want=$(($(cat "$dev/bNumInterfaces")))
tries=0
while [ "$tries" -lt 10 ]; do
    set -- "$interfaces"*
    if [ -e "$1" ] && [ "$#" -ge "$want" ]; then break; fi
    tries=$((tries + 1))
    sleep 0.1
done
for interface in "$interfaces"*; do
    [ "$(cat "$interface/bInterfaceClass" 2>/dev/null)" = 03 ] || continue
    [ "$(cat "$interface/bInterfaceProtocol" 2>/dev/null)" = 02 ] && exit 0
    for descriptor in "$interface"/*/report_descriptor; do
        [ -r "$descriptor" ] || continue
        od -An -tx1 -v "$descriptor" | tr -s ' \n' '  ' | grep -q ' 05 01 09 02 a1 01' && exit 0
    done
done
exit 1
"##;

/// The root shell script that installs [`HELPER`] (`$1`) and [`RULE`] (`$2`)
/// and applies them to devices already plugged in, so no replug or reboot is
/// needed. The helper goes first so the rule never runs without it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const INSTALL_SCRIPT: &str = r#"set -eu
printf '%s' "$1" > /etc/udev/openmouse-bridge-is-mouse
chmod 0644 /etc/udev/openmouse-bridge-is-mouse
printf '%s' "$2" > /etc/udev/rules.d/69-openmouse-bridge.rules
chmod 0644 /etc/udev/rules.d/69-openmouse-bridge.rules
udevadm control --reload-rules
udevadm trigger --subsystem-match=hidraw --action=change
udevadm trigger --subsystem-match=misc --sysname-match=uinput --action=change || true
udevadm settle --timeout=5 || true
"#;

const HELPER_FILE: &str = "openmouse-bridge-is-mouse";
const RULE_FILE: &str = "69-openmouse-bridge.rules";

/// The terminal command that installs the copies [`save_rule_files`] wrote to
/// `dir`, for when `pkexec` is missing or fails.
pub fn manual_command(dir: &Path) -> String {
    format!(
        "sudo install -m 0644 '{helper}' {HELPER_PATH} && sudo install -m 0644 '{rule}' {RULE_PATH} && sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=hidraw --action=change",
        helper = dir.join(HELPER_FILE).display(),
        rule = dir.join(RULE_FILE).display(),
    )
}

/// Saves the rule and its helper under the user's cache directory for
/// [`manual_command`]; returns that directory.
pub fn save_rule_files() -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;
    let dirs = directories::BaseDirs::new().context("could not find the cache directory")?;
    let dir = dirs.cache_dir().join("openmouse-bridge");
    fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    for (name, text) in [(HELPER_FILE, HELPER), (RULE_FILE, RULE)] {
        let path = dir.join(name);
        fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))?;
    }
    Ok(dir)
}

/// The USB device a hidraw node (`/sys/class/hidraw/hidrawN`) belongs to:
/// the nearest ancestor of its HID device with `bNumInterfaces`. Walking up
/// rather than taking the grandparent also covers the per-mouse nodes a
/// receiver driver nests under its own. `None` off USB (Bluetooth, I2C
/// touchpads): the rule only matches USB.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn usb_device(hidraw: &Path) -> Option<PathBuf> {
    let device = fs::canonicalize(hidraw.join("device")).ok()?;
    device
        .ancestors()
        .find(|dir| dir.join("bNumInterfaces").is_file())
        .map(Path::to_path_buf)
}

/// Whether the USB device at `usb` (a sysfs directory) has a HID interface
/// that is a boot-protocol mouse or describes a Generic Desktop Mouse.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn has_mouse_interface(usb: &Path) -> bool {
    let Some(prefix) = usb
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{name}:"))
    else {
        return false;
    };
    let Ok(entries) = fs::read_dir(usb) else {
        return false;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .map(|entry| entry.path())
        .any(|interface| {
            attribute(&interface, "bInterfaceClass").as_deref() == Some("03")
                && (attribute(&interface, "bInterfaceProtocol").as_deref() == Some("02")
                    || describes_mouse(&interface))
        })
}

fn attribute(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name))
        .ok()
        .map(|value| value.trim().to_owned())
}

/// Whether any HID device under `interface` has a mouse report descriptor.
fn describes_mouse(interface: &Path) -> bool {
    fs::read_dir(interface).is_ok_and(|entries| {
        entries
            .flatten()
            .filter_map(|entry| fs::read(entry.path().join("report_descriptor")).ok())
            .any(|descriptor| has_mouse_collection(&descriptor))
    })
}

/// Usage Page (Generic Desktop), Usage (Mouse), Collection (Application).
fn has_mouse_collection(descriptor: &[u8]) -> bool {
    descriptor
        .windows(6)
        .any(|window| window == [0x05, 0x01, 0x09, 0x02, 0xa1, 0x01])
}

#[cfg(target_os = "linux")]
mod imp {
    use std::{
        collections::HashMap, ffi::CString, fs, os::unix::ffi::OsStrExt, path::Path,
        process::Command,
    };

    use anyhow::{Context, Result, bail};

    use super::{HELPER, INSTALL_SCRIPT, RULE, has_mouse_interface, usb_device};

    /// The `/dev/hidraw*` nodes of USB mice and receivers that Bridge cannot
    /// open for reading and writing. Empty when all are reachable or none are
    /// plugged in.
    pub fn blocked_devices() -> Vec<String> {
        let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
            return Vec::new();
        };
        let mut mice = HashMap::new();
        let mut blocked: Vec<String> = entries
            .flatten()
            .filter(|entry| {
                usb_device(&entry.path()).is_some_and(|usb| {
                    *mice
                        .entry(usb)
                        .or_insert_with_key(|usb| has_mouse_interface(usb))
                })
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
            .args([HELPER, RULE])
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

    /// The text between `<<'MARKER'` and `MARKER` in install-linux.sh.
    fn installer_heredoc(marker: &str) -> String {
        // A Windows checkout has CRLF line endings.
        let script = include_str!("../install-linux.sh").replace("\r\n", "\n");
        let open = format!("<<'{marker}'\n");
        let start = script.find(&open).expect("installer heredoc start") + open.len();
        let end = start
            + script[start..]
                .find(&format!("\n{marker}\n"))
                .expect("installer heredoc end");
        format!("{}\n", &script[start..end])
    }

    #[test]
    fn the_installer_writes_the_same_rule_and_helper() {
        assert_eq!(installer_heredoc("OPENMOUSE_UDEV_RULE"), RULE);
        assert_eq!(installer_heredoc("OPENMOUSE_UDEV_HELPER"), HELPER);
    }

    #[test]
    fn the_rule_calls_the_helper_and_grants_the_seat_only() {
        assert!(RULE.contains(&format!("PROGRAM=\"/bin/sh {HELPER_PATH} %S%p\"")));
        assert!(INSTALL_SCRIPT.contains(HELPER_PATH));
        assert!(INSTALL_SCRIPT.contains(RULE_PATH));
        assert!(!RULE.contains("0666"));
        assert!(RULE.contains(r#"KERNEL=="uinput""#));
    }

    #[test]
    fn mouse_collections_are_found_anywhere_in_the_descriptor() {
        assert!(has_mouse_collection(&[
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01
        ]));
        assert!(has_mouse_collection(&[
            0x06, 0x00, 0xff, 0xc0, 0x05, 0x01, 0x09, 0x02, 0xa1, 0x01
        ]));
        // Keyboard, and a vendor page.
        assert!(!has_mouse_collection(&[0x05, 0x01, 0x09, 0x06, 0xa1, 0x01]));
        assert!(!has_mouse_collection(&[
            0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01
        ]));
    }

    #[cfg(unix)]
    mod sysfs {
        use std::{os::unix::fs::symlink, path::PathBuf};

        use super::super::*;

        const MOUSE: &[u8] = &[0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0xc0];
        const KEYBOARD: &[u8] = &[0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0xc0];
        const VENDOR: &[u8] = &[0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0xc0];

        /// A HID interface for [`Tree::usb`]: protocol and report descriptor.
        struct Interface(&'static str, &'static [u8]);

        /// A fake sysfs tree under the temp dir, removed on drop.
        struct Tree(PathBuf);

        impl Tree {
            fn new(name: &str) -> Self {
                let root = std::env::temp_dir()
                    .join(format!("openmouse-udev-{}-{name}", std::process::id()));
                let _ = fs::remove_dir_all(&root);
                fs::create_dir_all(&root).unwrap();
                Self(root)
            }

            /// USB device `1-2` with `interfaces`; returns the hidraw class
            /// entry of each, like `/sys/class/hidraw/hidrawN`.
            fn usb(&self, interfaces: &[Interface]) -> Vec<PathBuf> {
                let usb = self.0.join("devices/usb1/1-2");
                fs::create_dir_all(&usb).unwrap();
                fs::write(
                    usb.join("bNumInterfaces"),
                    format!("{:>2}\n", interfaces.len()),
                )
                .unwrap();
                interfaces
                    .iter()
                    .enumerate()
                    .map(|(index, Interface(protocol, descriptor))| {
                        let interface = usb.join(format!("1-2:1.{index}"));
                        let hid = interface.join(format!("0003:1234:5678.000{index}"));
                        fs::create_dir_all(&hid).unwrap();
                        fs::write(interface.join("bInterfaceClass"), "03\n").unwrap();
                        fs::write(
                            interface.join("bInterfaceProtocol"),
                            format!("{protocol}\n"),
                        )
                        .unwrap();
                        fs::write(hid.join("report_descriptor"), descriptor).unwrap();
                        self.hidraw(&hid, &format!("hidraw{index}"))
                    })
                    .collect()
            }

            /// A hidraw node under the HID device `hid`, and its class entry.
            fn hidraw(&self, hid: &Path, name: &str) -> PathBuf {
                let node = hid.join("hidraw").join(name);
                fs::create_dir_all(&node).unwrap();
                symlink(hid, node.join("device")).unwrap();
                let class = self.0.join("class/hidraw");
                fs::create_dir_all(&class).unwrap();
                symlink(&node, class.join(name)).unwrap();
                class.join(name)
            }
        }

        impl Drop for Tree {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        /// The Rust check and the udev helper, which must agree.
        fn opened(hidraw: &Path) -> bool {
            let rust = usb_device(hidraw).is_some_and(|usb| has_mouse_interface(&usb));
            #[cfg(target_os = "linux")]
            {
                let node = fs::canonicalize(hidraw).unwrap();
                let shell = std::process::Command::new("sh")
                    .args(["-c", HELPER, "sh"])
                    .arg(node)
                    .status()
                    .unwrap()
                    .success();
                assert_eq!(
                    rust,
                    shell,
                    "Rust and the udev helper disagree on {}",
                    hidraw.display()
                );
            }
            rust
        }

        #[test]
        fn every_interface_of_a_receiver_with_a_boot_mouse_opens() {
            // Logitech's receiver: boot mouse, keyboard, vendor (HID++).
            let tree = Tree::new("receiver");
            let nodes = tree.usb(&[
                Interface("02", MOUSE),
                Interface("01", KEYBOARD),
                Interface("00", VENDOR),
            ]);
            assert!(nodes.iter().all(|node| opened(node)));
            // The per-mouse node its driver nests under the vendor interface.
            let hid = fs::canonicalize(nodes[2].join("device")).unwrap();
            let child = hid.join("0003:1234:40B7.0009");
            fs::create_dir_all(&child).unwrap();
            assert!(opened(&tree.hidraw(&child, "hidraw9")));
        }

        #[test]
        fn a_mouse_without_boot_protocol_opens_by_its_descriptor() {
            let tree = Tree::new("descriptor");
            let nodes = tree.usb(&[Interface("00", VENDOR), Interface("00", MOUSE)]);
            assert!(nodes.iter().all(|node| opened(node)));
        }

        #[test]
        fn a_keyboard_without_a_mouse_interface_stays_closed() {
            let tree = Tree::new("keyboard");
            let nodes = tree.usb(&[Interface("01", KEYBOARD), Interface("00", VENDOR)]);
            assert!(nodes.iter().all(|node| !opened(node)));
        }

        #[test]
        fn a_node_off_usb_stays_closed() {
            // An I2C touchpad: a mouse descriptor, but no USB device above it.
            let tree = Tree::new("i2c");
            let hid = tree.0.join("devices/i2c-1/0018:04F3:31F8.0001");
            fs::create_dir_all(&hid).unwrap();
            fs::write(hid.join("report_descriptor"), MOUSE).unwrap();
            assert!(!opened(&tree.hidraw(&hid, "hidraw1")));
        }
    }
}
