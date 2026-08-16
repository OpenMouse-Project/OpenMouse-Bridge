//! Native mouse access for the Bridge.
//!
//! Some mice keep their configuration channel where the browser cannot reach
//! it. The Attack Shark X11 is the hard case: its HID descriptor declares no
//! feature reports, so neither WebHID nor the OS HID API can move config data
//! to it (Windows' `HidD_SetFeature` returns ERROR_INVALID_FUNCTION). The
//! reference driver (dressedinblack5/attack-shark-x11-electron) works around
//! this by bypassing HID entirely: it claims USB interface 2 and sends raw
//! control transfers. We do the same here with `nusb`.
//!
//! Platform note: claiming a USB interface for raw access needs a suitable
//! kernel driver. On Linux the kernel HID driver is detached automatically; on
//! Windows interface 2 must be bound to WinUSB (via Zadig) or the claim fails —
//! in which case the device is still listed, with a note explaining why it is
//! not yet controllable.
//!
//! `nusb` is async, so the worker is a tokio task and talks to the HTTP side
//! through a command channel.

pub mod attackshark;

use std::{collections::HashSet, time::Duration};

use anyhow::{Result, anyhow};
use nusb::transfer::{ControlOut, ControlType, Recipient, RequestBuffer};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use crate::service::{BatteryReading, BridgeService};

/// How often the worker re-enumerates and samples battery.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Upper bound on a single control transfer.
const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);
/// How long to wait for a battery packet each cycle before giving up.
const BATTERY_READ_WINDOW: Duration = Duration::from_millis(250);
/// Battery interrupt reads use a 64-byte buffer (the endpoint's packet size).
const BATTERY_BUFFER: usize = 64;
/// Default DPI stages shown before the user sets their own.
const DEFAULT_DPI_STAGES: [u16; attackshark::DPI_STAGE_COUNT] = [400, 800, 1600, 3200, 6400, 12000];

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
    /// True when interface 2 was claimed and commands can be sent.
    pub controllable: bool,
    pub battery_percent: Option<u8>,
    pub polling_rate_hz: Option<u16>,
    pub supported_polling_rates: Vec<u16>,
    /// The six DPI stages, as last set through the Bridge. The mouse does not
    /// report its own, so these start at defaults and track what we write.
    pub dpi_stages: Vec<u16>,
    /// Active DPI stage, 1-based.
    pub active_dpi_stage: u8,
    pub dpi_min: u16,
    pub dpi_max: u16,
    pub dpi_step: u16,
    /// User-facing explanation of the device's current state.
    pub note: &'static str,
}

/// Commands the async HTTP side sends to the device worker.
enum Command {
    List(oneshot::Sender<Vec<DeviceInfo>>),
    SetPolling {
        id: String,
        hz: u16,
        reply: oneshot::Sender<Result<u16>>,
    },
    SetDpi {
        id: String,
        stages: Vec<u16>,
        active_stage: u8,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// Handle to the device worker. Cloneable and cheap.
#[derive(Clone)]
pub struct DeviceManager {
    commands: mpsc::Sender<Command>,
}

impl DeviceManager {
    /// Start the worker as a tokio task. Battery readings are forwarded into
    /// `service` so they reuse the existing low-battery notifier. Returns
    /// `None` if called outside a tokio runtime.
    pub fn start(service: BridgeService) -> Option<Self> {
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::error!("device worker needs a tokio runtime; native support is off");
            return None;
        }
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(worker(rx, service));
        Some(Self { commands: tx })
    }

    /// List every X11-family mouse currently attached.
    pub async fn list(&self) -> Vec<DeviceInfo> {
        let (tx, rx) = oneshot::channel();
        if self.commands.send(Command::List(tx)).await.is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Set the polling rate on one device and return the value written.
    pub async fn set_polling(&self, id: String, hz: u16) -> Result<u16> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::SetPolling { id, hz, reply: tx })
            .await
            .map_err(|_| anyhow!("the device worker is not running"))?;
        rx.await
            .map_err(|_| anyhow!("the device worker dropped the request"))?
    }

    /// Set the six DPI stages and active stage on one device.
    pub async fn set_dpi(&self, id: String, stages: Vec<u16>, active_stage: u8) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::SetDpi {
                id,
                stages,
                active_stage,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("the device worker is not running"))?;
        rx.await
            .map_err(|_| anyhow!("the device worker dropped the request"))?
    }
}

/// One attached device. `interface` is `None` when interface 2 could not be
/// claimed (e.g. not yet bound to WinUSB on Windows) — the device is still
/// listed so the UI can explain the situation.
struct OpenDevice {
    info: DeviceInfo,
    interface: Option<nusb::Interface>,
}

async fn worker(mut commands: mpsc::Receiver<Command>, service: BridgeService) {
    let mut devices: Vec<OpenDevice> = Vec::new();
    refresh(&mut devices);
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::List(reply)) => {
                    let _ = reply.send(devices.iter().map(|device| device.info.clone()).collect());
                }
                Some(Command::SetPolling { id, hz, reply }) => {
                    let _ = reply.send(set_polling(&mut devices, &id, hz).await);
                }
                Some(Command::SetDpi { id, stages, active_stage, reply }) => {
                    let _ = reply.send(set_dpi(&mut devices, &id, &stages, active_stage).await);
                }
                None => break,
            },
            _ = ticker.tick() => {
                poll_battery(&mut devices, &service).await;
                refresh(&mut devices);
            }
        }
    }
}

/// Re-enumerate and reconcile the open-device list with what is attached now.
fn refresh(devices: &mut Vec<OpenDevice>) {
    let list = match nusb::list_devices() {
        Ok(list) => list,
        Err(error) => {
            tracing::debug!(%error, "nusb enumeration failed");
            return;
        }
    };

    let entries: Vec<nusb::DeviceInfo> = list
        .filter(|entry| attackshark::is_x11(entry.vendor_id(), entry.product_id()))
        .collect();

    // Drop devices that are no longer present.
    let present: HashSet<(u16, u16)> = entries
        .iter()
        .map(|entry| (entry.vendor_id(), entry.product_id()))
        .collect();
    devices.retain(|device| present.contains(&(device.info.vendor_id, device.info.product_id)));

    for entry in entries {
        let (vid, pid) = (entry.vendor_id(), entry.product_id());
        if devices
            .iter()
            .any(|device| device.info.vendor_id == vid && device.info.product_id == pid)
        {
            continue;
        }

        let id = format!("{vid:04x}:{pid:04x}");
        let (interface, controllable, note) = match claim(&entry) {
            Ok(interface) => {
                tracing::info!(device = %id, "claimed Attack Shark interface 2 for native control");
                (
                    Some(interface),
                    true,
                    "Connected. Polling rate is set natively over USB.",
                )
            }
            Err(error) => {
                tracing::warn!(%error, vid, pid, "could not claim interface 2; on Windows bind it to WinUSB with Zadig");
                (
                    None,
                    false,
                    "Detected, but interface 2 is not claimable. On Windows, bind it to WinUSB with Zadig, then reconnect.",
                )
            }
        };

        devices.push(OpenDevice {
            info: DeviceInfo {
                id,
                name: attackshark::model_name(pid).to_owned(),
                vendor_id: vid,
                product_id: pid,
                connection: if attackshark::is_wireless(pid) {
                    "wireless"
                } else {
                    "wired"
                },
                controllable,
                battery_percent: None,
                polling_rate_hz: None,
                supported_polling_rates: attackshark::supported_polling_rates(),
                // The mouse does not report its stages, so start from sensible
                // defaults; they update as the user writes new ones.
                dpi_stages: DEFAULT_DPI_STAGES.to_vec(),
                active_dpi_stage: 2,
                dpi_min: attackshark::DPI_MIN,
                dpi_max: attackshark::DPI_MAX,
                dpi_step: attackshark::DPI_STEP,
                note,
            },
            interface,
        });
    }
}

/// Open the device and claim interface 2 for raw control transfers. On Linux
/// this detaches the kernel HID driver first; on Windows it requires WinUSB.
fn claim(entry: &nusb::DeviceInfo) -> Result<nusb::Interface> {
    let device = entry.open()?;
    let interface = device.detach_and_claim_interface(attackshark::CONTROL_INTERFACE)?;
    Ok(interface)
}

/// Send the polling-rate command as a HID SET_REPORT control transfer, exactly
/// as the reference driver does (bmRequestType 0x21, bRequest 0x09, wValue
/// 0x0306, wIndex 2). A completed transfer is the mouse acknowledging it.
async fn set_polling(devices: &mut [OpenDevice], id: &str, hz: u16) -> Result<u16> {
    let device = devices
        .iter_mut()
        .find(|device| device.info.id == id)
        .ok_or_else(|| anyhow!("no attached device with id {id}"))?;
    let interface = device.interface.as_ref().ok_or_else(|| {
        anyhow!(
            "this mouse is detected but interface 2 is not claimable; on Windows bind it to \
             WinUSB with Zadig first"
        )
    })?;
    let packet = attackshark::polling_packet(hz)
        .ok_or_else(|| anyhow!("{hz} Hz is not a supported polling rate"))?;

    let transfer = interface.control_out(ControlOut {
        control_type: ControlType::Class,
        recipient: Recipient::Interface,
        request: attackshark::SET_REPORT_REQUEST,
        value: attackshark::POLLING_WVALUE,
        index: u16::from(attackshark::CONTROL_INTERFACE),
        data: &packet,
    });
    let completion = tokio::time::timeout(CONTROL_TIMEOUT, transfer)
        .await
        .map_err(|_| anyhow!("the polling command timed out"))?;
    completion
        .status
        .map_err(|error| anyhow!("the mouse rejected the polling command: {error}"))?;

    device.info.polling_rate_hz = Some(hz);
    tracing::info!(device = %device.info.id, hz, "set polling rate over USB");
    Ok(hz)
}

/// Send the DPI/stage command as a HID SET_REPORT control transfer (wValue
/// 0x0304). The wireless adapter takes the full 56-byte report; wired X11/R1
/// take the 52-byte form (checksum-terminated), matching the reference driver.
async fn set_dpi(
    devices: &mut [OpenDevice],
    id: &str,
    stages: &[u16],
    active_stage: u8,
) -> Result<()> {
    let device = devices
        .iter_mut()
        .find(|device| device.info.id == id)
        .ok_or_else(|| anyhow!("no attached device with id {id}"))?;
    let interface = device.interface.as_ref().ok_or_else(|| {
        anyhow!(
            "this mouse is detected but interface 2 is not claimable; on Windows bind it to \
             WinUSB with Zadig first"
        )
    })?;

    let stage_array: [u16; attackshark::DPI_STAGE_COUNT] = stages
        .try_into()
        .map_err(|_| anyhow!("expected {} DPI stages", attackshark::DPI_STAGE_COUNT))?;
    let packet =
        attackshark::dpi_packet(stage_array, active_stage, false, true).ok_or_else(|| {
            anyhow!(
                "invalid DPI request: stages must be {}–{} and the active stage 1–{}",
                attackshark::DPI_MIN,
                attackshark::DPI_MAX,
                attackshark::DPI_STAGE_COUNT
            )
        })?;
    // Wired variants expect the shorter, checksum-terminated report.
    let data: &[u8] = if attackshark::is_wireless(device.info.product_id) {
        &packet
    } else {
        &packet[..52]
    };

    let transfer = interface.control_out(ControlOut {
        control_type: ControlType::Class,
        recipient: Recipient::Interface,
        request: attackshark::SET_REPORT_REQUEST,
        value: attackshark::DPI_WVALUE,
        index: u16::from(attackshark::CONTROL_INTERFACE),
        data,
    });
    let completion = tokio::time::timeout(CONTROL_TIMEOUT, transfer)
        .await
        .map_err(|_| anyhow!("the DPI command timed out"))?;
    completion
        .status
        .map_err(|error| anyhow!("the mouse rejected the DPI command: {error}"))?;

    device.info.dpi_stages = stage_array.to_vec();
    device.info.active_dpi_stage = active_stage;
    tracing::info!(device = %device.info.id, ?stage_array, active_stage, "set DPI over USB");
    Ok(())
}

/// Best-effort battery sample: read one interrupt packet from the battery
/// endpoint, and push a change into the service's low-battery notifier.
async fn poll_battery(devices: &mut [OpenDevice], service: &BridgeService) {
    for device in devices.iter_mut() {
        if !attackshark::is_wireless(device.info.product_id) {
            continue;
        }
        let Some(interface) = device.interface.as_ref() else {
            continue;
        };

        let transfer = interface.interrupt_in(
            attackshark::BATTERY_ENDPOINT,
            RequestBuffer::new(BATTERY_BUFFER),
        );
        let Ok(completion) = tokio::time::timeout(BATTERY_READ_WINDOW, transfer).await else {
            continue; // No battery packet this cycle.
        };
        if completion.status.is_err() {
            continue;
        }
        let Some(percent) = attackshark::parse_battery(&completion.data) else {
            continue;
        };
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
        if let Err(error) = service.record_battery(reading).await {
            tracing::debug!(%error, "could not record native battery reading");
        }
    }
}
