//! Native mouse access for the Bridge.
//!
//! The web app drives mice over WebHID, but some devices keep their
//! configuration channel on HID collections the browser refuses to touch
//! (protected keyboard/system-control usages). The Attack Shark X11 is one of
//! them: everything a page needs is hidden or blocked. A native HID handle is
//! not subject to that block, so the Bridge can talk to interface 2 directly.
//!
//! hidapi is blocking, and its handles are not `Sync`, so all device I/O runs
//! on one dedicated OS thread. The async world talks to it through a command
//! channel and never touches a raw handle.

pub mod attackshark;

use std::{
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use hidapi::{HidApi, HidDevice};
use serde::Serialize;
use tokio::{runtime::Handle, sync::oneshot};

use crate::service::{BatteryReading, BridgeService};

/// How often the worker polls for plug/unplug and drains battery reports.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Feature-report buffers are 65 bytes (id + 64) — the largest report here.
const REPORT_BUFFER: usize = 65;

/// A mouse the Bridge can see natively, as reported to the web app.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    /// Stable id, e.g. `"1d57:fa60"`.
    pub id: String,
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    /// `"wired"` or `"wireless"`.
    pub connection: &'static str,
    pub battery_percent: Option<u8>,
    pub polling_rate_hz: Option<u16>,
    pub supported_polling_rates: Vec<u16>,
    /// Why settings may be limited, when they are.
    pub note: &'static str,
}

/// Commands the async side sends to the blocking worker.
enum Command {
    List(oneshot::Sender<Vec<DeviceInfo>>),
    SetPolling {
        id: String,
        hz: u16,
        reply: oneshot::Sender<Result<u16>>,
    },
}

/// Handle to the device worker thread. Cloneable and cheap.
#[derive(Clone)]
pub struct DeviceManager {
    commands: Sender<Command>,
}

impl DeviceManager {
    /// Start the worker. Battery readings are forwarded into `service` so they
    /// reuse the existing low-battery notification pipeline. Returns `None`
    /// when hidapi is unavailable, so the Bridge still runs without it.
    pub fn start(service: BridgeService) -> Option<Self> {
        // Captured here (inside the async runtime) so the blocking worker can
        // hand battery readings back to the runtime without owning one.
        let runtime = match Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                tracing::error!("device worker needs a tokio runtime; native support is off");
                return None;
            }
        };
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("openmouse-devices".into())
            .spawn(move || worker(rx, service, runtime));
        match spawned {
            Ok(_) => Some(Self { commands: tx }),
            Err(error) => {
                tracing::error!(%error, "could not start the device worker");
                None
            }
        }
    }

    /// List every X11-family mouse currently attached.
    pub async fn list(&self) -> Vec<DeviceInfo> {
        let (tx, rx) = oneshot::channel();
        if self.commands.send(Command::List(tx)).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Set the polling rate on one device and return the value it confirmed.
    pub async fn set_polling(&self, id: String, hz: u16) -> Result<u16> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::SetPolling { id, hz, reply: tx })
            .map_err(|_| anyhow!("the device worker is not running"))?;
        rx.await
            .map_err(|_| anyhow!("the device worker dropped the request"))?
    }
}

/// One attached, opened control interface.
struct OpenDevice {
    info: DeviceInfo,
    handle: HidDevice,
}

fn worker(commands: Receiver<Command>, service: BridgeService, runtime: Handle) {
    let mut api = match HidApi::new() {
        Ok(api) => api,
        Err(error) => {
            tracing::error!(%error, "hidapi is unavailable; native device support is off");
            return;
        }
    };
    let mut devices: Vec<OpenDevice> = Vec::new();

    // Discover once up front so an already-attached mouse is ready immediately.
    refresh(&mut api, &mut devices);
    let mut last_refresh = Instant::now();

    loop {
        match commands.recv_timeout(POLL_INTERVAL) {
            Ok(Command::List(reply)) => {
                let _ = reply.send(devices.iter().map(|device| device.info.clone()).collect());
            }
            Ok(Command::SetPolling { id, hz, reply }) => {
                let _ = reply.send(set_polling(&mut devices, &id, hz));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Battery is drained on every wake so a fresh command still refreshes
        // it, but enumeration is throttled so a busy command stream cannot
        // spin hidapi.
        drain_battery(&mut devices, &service, &runtime);
        if last_refresh.elapsed() >= POLL_INTERVAL {
            refresh(&mut api, &mut devices);
            last_refresh = Instant::now();
        }
    }
}

/// Re-enumerate and reconcile the open-device list with what is attached now.
fn refresh(api: &mut HidApi, devices: &mut Vec<OpenDevice>) {
    if let Err(error) = api.refresh_devices() {
        tracing::debug!(%error, "hidapi refresh failed");
        return;
    }

    // Drop devices that are no longer present.
    let present: Vec<(u16, u16)> = api
        .device_list()
        .filter(|entry| attackshark::is_x11(entry.vendor_id(), entry.product_id()))
        .map(|entry| (entry.vendor_id(), entry.product_id()))
        .collect();
    devices.retain(|device| present.contains(&(device.info.vendor_id, device.info.product_id)));

    for entry in api.device_list() {
        let (vid, pid) = (entry.vendor_id(), entry.product_id());
        if !attackshark::is_x11(vid, pid) {
            continue;
        }
        // Only the control interface answers; the boot mouse/keyboard entries
        // are the wrong endpoint. hidapi reports -1 when it cannot tell, in
        // which case we fall back to the system-control usage page (0x0c).
        let is_control = entry.interface_number() == attackshark::CONTROL_INTERFACE
            || (entry.interface_number() < 0 && entry.usage_page() == 0x000c);
        if !is_control {
            continue;
        }
        if devices
            .iter()
            .any(|device| device.info.vendor_id == vid && device.info.product_id == pid)
        {
            continue;
        }
        match entry.open_device(api) {
            Ok(handle) => {
                // Battery arrives unprompted; never block waiting for it.
                let _ = handle.set_blocking_mode(false);
                let info = DeviceInfo {
                    id: format!("{vid:04x}:{pid:04x}"),
                    name: attackshark::model_name(pid).to_owned(),
                    vendor_id: vid,
                    product_id: pid,
                    connection: if attackshark::is_wireless(pid) {
                        "wireless"
                    } else {
                        "wired"
                    },
                    battery_percent: None,
                    polling_rate_hz: read_polling(&handle),
                    supported_polling_rates: attackshark::supported_polling_rates(),
                    note: "Battery and polling rate are read natively; other settings still require the desktop driver.",
                };
                tracing::info!(
                    device = %info.id,
                    interface = entry.interface_number(),
                    usage_page = format_args!("{:#06x}", entry.usage_page()),
                    usage = format_args!("{:#06x}", entry.usage()),
                    "opened Attack Shark control interface",
                );
                devices.push(OpenDevice { info, handle });
            }
            Err(error) => {
                tracing::debug!(%error, vid, pid, "could not open Attack Shark control interface");
            }
        }
    }
}

/// Ask the mouse for its polling rate and decode the reply. Best-effort.
///
/// The X11's HID descriptor declares no feature reports, so on Windows the HID
/// API can reject these calls outright (HidD_Set/GetFeature validate the buffer
/// length against the descriptor). We log the exact error so a failing unit
/// tells us whether the HID path is viable or whether raw USB (WinUSB/Zadig) is
/// required, rather than silently reporting "unknown".
fn read_polling(handle: &HidDevice) -> Option<u16> {
    if let Err(error) = handle.send_feature_report(&attackshark::polling_read_request()) {
        tracing::warn!(%error, "polling read-request (feature report 0xa0) was refused");
        return None;
    }
    let mut buffer = [0u8; REPORT_BUFFER];
    buffer[0] = attackshark::POLLING_REPORT_ID;
    let read = match handle.get_feature_report(&mut buffer) {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(%error, "polling read-back (feature report 0x06) was refused");
            return None;
        }
    };
    let parsed = attackshark::parse_polling_reply(&buffer[..read]);
    if parsed.is_none() {
        tracing::warn!(bytes = ?&buffer[..read], "polling read-back did not decode");
    }
    parsed
}

fn set_polling(devices: &mut [OpenDevice], id: &str, hz: u16) -> Result<u16> {
    let device = devices
        .iter_mut()
        .find(|device| device.info.id == id)
        .ok_or_else(|| anyhow!("no attached device with id {id}"))?;
    let packet = attackshark::polling_packet(hz)
        .ok_or_else(|| anyhow!("{hz} Hz is not a supported polling rate"))?;
    device
        .handle
        .send_feature_report(&packet)
        .map_err(|error| {
            anyhow!(
                "the mouse refused the polling command (HID feature report 0x06): {error}. \
             This mouse declares no HID feature reports, so Windows likely needs the \
             interface bound to WinUSB (Zadig) and a raw-USB transport."
            )
        })?;

    // Try to confirm the mouse kept the new rate. The read-back can fail on
    // this device even when the write lands, so distinguish the two: a hard
    // mismatch is an error, but an unreadable rate is reported as "sent,
    // unverified" rather than a false success.
    match read_polling(&device.handle) {
        Some(confirmed) if confirmed == hz => {
            device.info.polling_rate_hz = Some(confirmed);
            Ok(confirmed)
        }
        Some(other) => Err(anyhow!("the mouse kept {other} Hz instead of {hz} Hz")),
        None => {
            // Write was accepted but the rate is not read-backable here.
            device.info.polling_rate_hz = Some(hz);
            tracing::info!(
                hz,
                "polling command sent; rate is not read-backable on this unit"
            );
            Ok(hz)
        }
    }
}

/// Drain any pending battery input reports and push them into the service so
/// the existing low-battery notifier fires. Non-blocking; skips quiet devices.
fn drain_battery(devices: &mut [OpenDevice], service: &BridgeService, runtime: &Handle) {
    for device in devices.iter_mut() {
        let mut buffer = [0u8; REPORT_BUFFER];
        // Read until the queue is empty so we always keep the freshest value.
        let mut latest: Option<u8> = None;
        while let Ok(read) = device.handle.read_timeout(&mut buffer, 0) {
            if read == 0 {
                break;
            }
            if let Some(percent) = attackshark::parse_battery(&buffer[..read]) {
                latest = Some(percent);
            }
        }
        let Some(percent) = latest else { continue };
        if device.info.battery_percent == Some(percent) {
            continue;
        }
        device.info.battery_percent = Some(percent);
        let reading = BatteryReading {
            device_id: device.info.id.clone(),
            device_name: device.info.name.clone(),
            percent,
            charging: false,
        };
        let service = service.clone();
        // record_battery is async; hop onto the runtime without blocking here.
        runtime.spawn(async move {
            if let Err(error) = service.record_battery(reading).await {
                tracing::debug!(%error, "could not record native battery reading");
            }
        });
    }
}
