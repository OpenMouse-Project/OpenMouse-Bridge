//! Falls back to the bundled Node.js helper (`native-hid/`) for any brand
//! that doesn't have a dependency-free native Rust driver. The helper reuses
//! OpenMouse's own hardware-verified `@openmouse/protocol` WebHID driver
//! classes through a small adapter, instead of Bridge reimplementing every
//! vendor's wire protocol from scratch — see `native-hid/README.md`.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::ApplicationProfile;

/// Generous: covers Node module resolution/startup plus the helper's own
/// per-candidate probe timeouts (a few seconds each, for a handful of HID
/// interfaces at most).
const HELPER_TIMEOUT: Duration = Duration::from_secs(10);

const EXIT_APPLIED: i32 = 0;
const EXIT_NO_DRIVER: i32 = 3;

#[derive(Serialize)]
struct ApplyRequest<'a> {
    brand: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dpi: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "pollingRateHz")]
    polling_rate_hz: Option<u32>,
}

/// Pushes a profile's DPI/polling rate to the mouse via the Node helper.
/// Returns `Ok(true)` on a confirmed apply, `Ok(false)` when the helper has
/// no driver for this brand (most devices are still driven by the OpenMouse
/// web app over WebHID instead), and `Err` when a driver exists but the
/// apply itself failed (including the helper or Node.js being unavailable).
pub fn apply(profile: &ApplicationProfile) -> Result<bool> {
    let script = locate_apply_script()?;
    let brand = profile.device.id.split(':').next().unwrap_or_default();
    let request = ApplyRequest {
        brand,
        dpi: profile.settings.dpi,
        polling_rate_hz: profile.settings.polling_rate_hz,
    };
    let payload =
        serde_json::to_vec(&request).context("could not encode the native-hid request")?;

    let node = locate_node_binary();
    let mut child = Command::new(node.as_deref().unwrap_or_else(|| Path::new("node")))
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| match &node {
            Some(bundled) => format!(
                "could not start the bundled Node.js runtime at {}",
                bundled.display()
            ),
            None => format!(
                "could not start Node.js to run {} — is Node.js installed and on PATH?",
                script.display()
            ),
        })?;

    // Dropping the returned handle after this write closes stdin, which is
    // how apply.mjs's stdin-read loop knows the request is complete.
    child
        .stdin
        .take()
        .context("the native-hid helper's stdin was unavailable")?
        .write_all(&payload)
        .context("could not send the profile to the native-hid helper")?;

    let output = wait_with_timeout(child, HELPER_TIMEOUT)?;
    match output.status.code() {
        Some(EXIT_APPLIED) => Ok(true),
        Some(EXIT_NO_DRIVER) => Ok(false),
        Some(code) => bail!(
            "native-hid helper exited with status {code}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        None => bail!("native-hid helper was terminated by a signal"),
    }
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

/// Waits for the child with a hard deadline, killing it if exceeded. Reads
/// stdout/stderr only after exit — safe here because the helper's own
/// output is a handful of short lines at most, never enough to fill a pipe
/// buffer and risk the classic write-blocks-because-nobody-is-reading
/// deadlock a long-running/chatty child could hit with this pattern.
fn wait_with_timeout(mut child: Child, timeout: Duration) -> Result<Output> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .context("could not poll the native-hid helper")?
        {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut pipe) = child.stdout.take() {
                let _ = pipe.read_to_end(&mut stdout);
            }
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_end(&mut stderr);
            }
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the native-hid helper did not finish within {}s",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
