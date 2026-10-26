//! Falls back to the bundled Node.js helper (`native-hid/`) for any brand
//! that doesn't have a dependency-free native Rust driver. The helper reuses
//! OpenMouse's own hardware-verified `@openmouse/protocol` WebHID driver
//! classes through a small adapter, instead of Bridge reimplementing every
//! vendor's wire protocol from scratch — see `native-hid/README.md`.

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::config::ApplicationProfile;

/// Generous: covers Node module resolution/startup plus the helper's own
/// per-candidate probe timeouts (a few seconds each, for a handful of HID
/// interfaces at most).
const HELPER_TIMEOUT: Duration = Duration::from_secs(10);
const HELPER_INPUT_LIMIT: usize = 4096;
const HELPER_OUTPUT_LIMIT: usize = 64 * 1024;

const EXIT_OK: i32 = 0;
const EXIT_NO_DRIVER: i32 = 3;

#[derive(Serialize)]
struct ApplyRequest<'a> {
    brand: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dpi: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "pollingRateHz")]
    polling_rate_hz: Option<u32>,
    /// The profile's opaque panel settings (`ProfileSettings.snapshot`,
    /// e.g. `{"lightforceSwitchMode":"Optical"}`). The helper applies the
    /// fields its driver supports and skips the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    settings: Option<&'a serde_json::Value>,
}

#[derive(Serialize)]
struct StatusRequest<'a> {
    brand: &'a str,
    action: &'static str,
}

/// The battery fields of `mouse-protocol`'s `MouseStatus`, as `apply.mjs`
/// prints them in status mode.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusReply {
    battery_percent: Option<f64>,
    battery_state: Option<String>,
    /// The driver's own model name (`MouseStatus.name`), when the driver
    /// reports one — better than the receiver-level name discovery gets from
    /// the HID descriptor.
    device_name: Option<String>,
    /// The settings currently in the device's onboard memory, when the
    /// driver can read them. `dpi` is a number or a per-stage list in
    /// `MouseStatus`; anything else parses as `None`.
    dpi: Option<serde_json::Value>,
    polling_rate_hz: Option<f64>,
    switch_mode: Option<String>,
}

/// A battery level read from the mouse over native HID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeBattery {
    pub percent: u8,
    pub charging: bool,
    /// The model name the driver itself reported, when it did.
    pub reported_name: Option<String>,
}

/// Everything the mouse reports about its own onboard-memory state: the
/// battery plus the settings currently in effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeStatus {
    pub battery: NativeBattery,
    pub dpi: Option<u32>,
    pub polling_rate_hz: Option<u32>,
    /// Logitech's LightForce switch mode; `None` for brands without one.
    pub switch_mode: Option<String>,
}

/// `MouseStatus.dpi` is either one number or a per-stage list; the stages
/// have no reliable "active" marker, so a list collapses to its first
/// stage — the one most drivers treat as the default.
fn coerce_dpi(value: Option<&serde_json::Value>) -> Option<u32> {
    match value? {
        serde_json::Value::Number(number) => number.as_u64().map(|dpi| dpi as u32),
        serde_json::Value::Array(stages) => stages
            .iter()
            .find_map(|stage| stage.as_u64().map(|dpi| dpi as u32)),
        _ => None,
    }
}

/// Pushes a profile's DPI/polling rate to the mouse via the Node helper.
/// Returns `Ok(true)` on a confirmed apply, `Ok(false)` when the helper has
/// no driver for this brand (most devices are still driven by the OpenMouse
/// web app over WebHID instead), and `Err` when a driver exists but the
/// apply itself failed (including the helper or Node.js being unavailable).
pub fn apply(profile: &ApplicationProfile) -> Result<bool> {
    let brand = profile.device.id.split(':').next().unwrap_or_default();
    apply_settings_with_snapshot(
        brand,
        profile.settings.dpi,
        profile.settings.polling_rate_hz,
        profile.settings.snapshot.as_ref(),
    )
}

/// Pushes an explicit settings request from the local HTTP API. This shares
/// the exact helper path used by automatic profiles, so native device control
/// and profile switching cannot drift into separate protocol implementations.
pub fn apply_settings(brand: &str, dpi: Option<u32>, polling_rate_hz: Option<u32>) -> Result<bool> {
    apply_settings_with_snapshot(brand, dpi, polling_rate_hz, None)
}

fn apply_settings_with_snapshot(
    brand: &str,
    dpi: Option<u32>,
    polling_rate_hz: Option<u32>,
    settings: Option<&serde_json::Value>,
) -> Result<bool> {
    let request = ApplyRequest {
        brand,
        dpi,
        polling_rate_hz,
        settings,
    };
    Ok(run_helper(&request)?.is_some())
}

/// Reads a `brand` mouse's status via the Node helper. `Ok(None)` means
/// the helper has no driver for the brand or the mouse reports no battery
/// (a wired mouse); `Err` means a driver exists but no mouse answered.
pub fn read_status(brand: &str) -> Result<Option<NativeStatus>> {
    let request = StatusRequest {
        brand,
        action: "status",
    };
    let Some(stdout) = run_helper(&request)? else {
        return Ok(None);
    };
    let reply: StatusReply = serde_json::from_slice(&stdout)
        .context("the native-hid helper returned an unreadable status")?;
    Ok(parse_status(reply))
}

fn parse_status(reply: StatusReply) -> Option<NativeStatus> {
    let battery = parse_battery(&reply)?;
    Some(NativeStatus {
        battery,
        dpi: coerce_dpi(reply.dpi.as_ref()),
        polling_rate_hz: reply
            .polling_rate_hz
            .and_then(|rate| u32::try_from(rate.round() as i64).ok()),
        switch_mode: reply.switch_mode.filter(|mode| !mode.trim().is_empty()),
    })
}

fn parse_battery(reply: &StatusReply) -> Option<NativeBattery> {
    let percent = reply.battery_percent?;
    if !percent.is_finite() {
        return None;
    }
    Some(NativeBattery {
        percent: percent.round().clamp(0.0, 100.0) as u8,
        charging: reply
            .battery_state
            .as_ref()
            .is_some_and(|state| state.starts_with("Charging")),
        reported_name: reply
            .device_name
            .clone()
            .filter(|name| !name.trim().is_empty()),
    })
}

#[cfg(test)]
pub(super) type TestHelper = Box<dyn FnMut(&serde_json::Value) -> Result<Option<Vec<u8>>>>;

#[cfg(test)]
thread_local! {
    static TEST_HELPER: std::cell::RefCell<Option<TestHelper>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn with_test_helper<R>(helper: TestHelper, operation: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_HELPER.with(|helper| *helper.borrow_mut() = None);
        }
    }
    TEST_HELPER.with(|slot| *slot.borrow_mut() = Some(helper));
    let _reset = Reset;
    operation()
}

/// Runs `apply.mjs` with `request` on stdin. Returns its stdout on success,
/// `None` when it has no driver for the brand, and `Err` otherwise.
pub(super) fn run_helper(request: &impl Serialize) -> Result<Option<Vec<u8>>> {
    #[cfg(test)]
    if let Some(result) = TEST_HELPER.with(|helper| {
        helper
            .borrow_mut()
            .as_mut()
            .map(|helper| helper(&serde_json::to_value(request)?))
    }) {
        return result;
    }

    let script = locate_apply_script()?;
    let payload = serde_json::to_vec(request).context("could not encode the native-hid request")?;

    if payload.len() > HELPER_INPUT_LIMIT {
        bail!("native-hid request exceeds the input limit");
    }

    let node = locate_node_binary();
    let mut command = Command::new(node.as_deref().unwrap_or_else(|| Path::new("node")));
    command
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);
    let child = command.spawn().with_context(|| match &node {
        Some(bundled) => format!(
            "could not start the bundled Node.js runtime at {}",
            bundled.display()
        ),
        None => format!(
            "could not start Node.js to run {} — is Node.js installed and on PATH?",
            script.display()
        ),
    })?;

    let output = wait_with_timeout(child, payload, HELPER_TIMEOUT)?;
    match output.status.code() {
        Some(EXIT_OK) => Ok(Some(output.stdout)),
        Some(EXIT_NO_DRIVER) => Ok(None),
        Some(code) => bail!(
            "native-hid helper exited with status {code}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        None => bail!("native-hid helper was terminated by a signal"),
    }
}

pub fn spawn_button_listener(brand: &str) -> Result<Child> {
    let script = locate_apply_script()?.with_file_name("button-listener.mjs");
    let node = locate_node_binary();
    let mut command = Command::new(node.as_deref().unwrap_or_else(|| Path::new("node")));
    command
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);
    command.spawn().with_context(|| {
        format!(
            "could not start {} for the {brand} button listener",
            script.display()
        )
    })
}

/// Finds `native-hid/src/apply.mjs` next to the running executable — how a
/// packaged Bridge release is expected to ship it — falling back to the
/// checkout's own source tree so `cargo run`/`cargo test` work without a
/// packaging step.
fn locate_apply_script() -> Result<PathBuf> {
    let packaged = installed_native_hid_dir().map(|dir| dir.join("src/apply.mjs"));
    let development = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/native-hid/src/apply.mjs"
    ));
    packaged
        .filter(|path| path.is_file())
        .or_else(|| development.is_file().then_some(development))
        .context(
            "could not find native-hid/src/apply.mjs next to the Bridge executable \
             or in the source checkout",
        )
}

/// Finds the Node.js runtime a packaged Bridge release bundles at
/// `native-hid/node/` next to the executable (see `.github/workflows/release.yml`).
/// `None` means no bundled runtime was found, so the caller should fall back
/// to `node` on `PATH` — the normal case for `cargo run`/`cargo test`, which
/// don't produce a packaged layout.
fn locate_node_binary() -> Option<PathBuf> {
    let dir = installed_native_hid_dir()?.join("node");
    let candidate = if cfg!(target_os = "windows") {
        dir.join("node.exe")
    } else {
        dir.join("bin/node")
    };
    candidate.is_file().then_some(candidate)
}

/// The `native-hid/` directory a packaged Bridge release ships next to its
/// executable, if the executable's location can be determined at all (it's
/// an `Option` rather than a hard error because the fallback paths this
/// module's callers use don't need it to succeed).
fn installed_native_hid_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("native-hid"))
}

fn read_capped(pipe: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.take((HELPER_OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > HELPER_OUTPUT_LIMIT {
        bail!("native-hid helper exceeded the output limit");
    }
    Ok(bytes)
}

fn wait_with_timeout(mut child: Child, payload: Vec<u8>, timeout: Duration) -> Result<Output> {
    let deadline = Instant::now() + timeout;
    let outcome = (|| {
        let stdin = child
            .stdin
            .take()
            .context("native-hid helper stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("native-hid helper stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("native-hid helper stderr unavailable")?;
        let (tx, rx) = std::sync::mpsc::channel::<(usize, Result<Vec<u8>>)>();
        for (index, pipe) in [
            (1, Box::new(stdout) as Box<dyn Read + Send>),
            (2, Box::new(stderr) as Box<dyn Read + Send>),
        ] {
            let tx = tx.clone();
            std::thread::Builder::new()
                .name(format!("openmouse-helper-output-{index}"))
                .spawn(move || {
                    let _ = tx.send((index, read_capped(pipe)));
                })
                .context("could not start native-hid output reader")?;
        }
        std::thread::Builder::new()
            .name("openmouse-helper-input".into())
            .spawn(move || {
                let mut stdin = stdin;
                let result = stdin
                    .write_all(&payload)
                    .map(|()| Vec::new())
                    .map_err(anyhow::Error::from);
                drop(stdin);
                let _ = tx.send((0, result));
            })
            .context("could not start native-hid input writer")?;
        let mut received: [Option<Vec<u8>>; 3] = [None, None, None];
        let mut status = None;
        loop {
            if status.is_none() {
                status = child
                    .try_wait()
                    .context("could not poll the native-hid helper")?;
            }
            if let Some(status) = status
                && received.iter().all(Option::is_some)
            {
                return Ok(Output {
                    status,
                    stdout: received[1].take().unwrap(),
                    stderr: received[2].take().unwrap(),
                });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "the native-hid helper did not finish within {}s",
                    timeout.as_secs()
                );
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(10))) {
                Ok((index, result)) => received[index] = Some(result?),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    if received.iter().any(Option::is_none) {
                        bail!("native-hid I/O worker stopped");
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
            }
        }
    })();
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_output_is_bounded_before_json_decoding() {
        assert_eq!(
            read_capped(std::io::Cursor::new(vec![1; HELPER_OUTPUT_LIMIT]))
                .unwrap()
                .len(),
            HELPER_OUTPUT_LIMIT
        );
        assert!(read_capped(std::io::Cursor::new(vec![1; HELPER_OUTPUT_LIMIT + 1])).is_err());
    }

    fn reply(percent: Option<f64>, state: Option<&str>) -> StatusReply {
        StatusReply {
            battery_percent: percent,
            battery_state: state.map(str::to_owned),
            device_name: None,
            dpi: None,
            polling_rate_hz: None,
            switch_mode: None,
        }
    }

    fn named_reply(name: &str) -> StatusReply {
        StatusReply {
            battery_percent: Some(41.0),
            battery_state: Some("Discharging".into()),
            device_name: Some(name.to_owned()),
            dpi: None,
            polling_rate_hz: None,
            switch_mode: None,
        }
    }

    #[test]
    fn parses_battery_from_status() {
        assert_eq!(
            parse_status(reply(Some(41.0), Some("Discharging"))),
            Some(NativeStatus {
                battery: NativeBattery {
                    percent: 41,
                    charging: false,
                    reported_name: None,
                },
                dpi: None,
                polling_rate_hz: None,
                switch_mode: None,
            })
        );
        assert_eq!(
            parse_status(reply(Some(87.6), Some("Charging slowly"))),
            Some(NativeStatus {
                battery: NativeBattery {
                    percent: 88,
                    charging: true,
                    reported_name: None,
                },
                dpi: None,
                polling_rate_hz: None,
                switch_mode: None,
            })
        );
        assert_eq!(parse_status(reply(None, Some("Unknown"))), None);
        assert_eq!(
            parse_status(reply(Some(140.0), None)).map(|status| status.battery.percent),
            Some(100)
        );
        // The driver's model name rides along for better tray labels.
        assert_eq!(
            parse_status(named_reply("G Pro X Superlight 2")),
            Some(NativeStatus {
                battery: NativeBattery {
                    percent: 41,
                    charging: false,
                    reported_name: Some("G Pro X Superlight 2".into()),
                },
                dpi: None,
                polling_rate_hz: None,
                switch_mode: None,
            })
        );
        // A blank name is treated as no name.
        assert_eq!(
            parse_status(named_reply("   ")).and_then(|status| status.battery.reported_name),
            None
        );
    }

    #[test]
    fn parses_the_onboard_settings_from_status() {
        let mut logitech = reply(Some(81.0), Some("Discharging"));
        logitech.device_name = Some("G Pro X Superlight 2".into());
        logitech.dpi = Some(serde_json::json!(800));
        logitech.polling_rate_hz = Some(1000.0);
        logitech.switch_mode = Some("Optical".into());
        assert_eq!(
            parse_status(logitech),
            Some(NativeStatus {
                battery: NativeBattery {
                    percent: 81,
                    charging: false,
                    reported_name: Some("G Pro X Superlight 2".into()),
                },
                dpi: Some(800),
                polling_rate_hz: Some(1000),
                switch_mode: Some("Optical".into()),
            })
        );
        // Drivers that report a stage list collapse to the first stage, and
        // blank switch modes read as none.
        let mut staged = reply(Some(81.0), None);
        staged.dpi = Some(serde_json::json!([400, 800, 1600]));
        staged.switch_mode = Some("  ".into());
        let status = parse_status(staged).expect("staged status parses");
        assert_eq!(status.dpi, Some(400));
        assert_eq!(status.switch_mode, None);
    }
}
