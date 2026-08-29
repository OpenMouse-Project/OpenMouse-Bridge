//! Native HID driver for real Pulsar-vendor (0x3710) mice, so Bridge can push
//! DPI and polling-rate changes to the mouse on its own — without a browser
//! tab open and holding a WebHID connection.
//!
//! The wire protocol here is ported byte-for-byte from
//! `mouse-protocol/src/pulsar/index.ts` and
//! `mouse-protocol/src/drivers/pulsar/pulsar-hid.ts`, the hardware-verified
//! WebHID driver OpenMouse's web app uses for the same mice (including the
//! Pulsar X2 CrazyLight). Only DPI and polling rate are implemented here;
//! everything else (LOD, motion sync, RGB, ...) still lives in the web app.

use std::{
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use hidapi::HidApi;

use crate::config::ApplicationProfile;

/// This driver's slot in `crate::drivers`' brand registry — matches the
/// `"Pulsar:"` prefix the OpenMouse web app writes into `device.id`.
pub const BRAND: &str = "pulsar";

/// Opens the connected Pulsar mouse and pushes a profile's DPI/polling rate
/// to it. Called by `crate::drivers::apply_profile` once it has matched the
/// profile's device to this brand.
pub fn apply(profile: &ApplicationProfile) -> Result<()> {
    let device = PulsarDevice::open()?;
    if let Some(dpi) = profile.settings.dpi {
        device.set_dpi(dpi)?;
    }
    if let Some(rate) = profile.settings.polling_rate_hz {
        device.set_polling_rate(rate)?;
    }
    Ok(())
}

/// Real Pulsar-branded mice and dongles. The Pulsar 4K Wireless Receiver
/// shares a report-8 16-byte protocol but enumerates under a different,
/// shared vendor id (0x3554) that other brands also use — out of scope here.
pub const VENDOR_ID: u16 = 0x3710;

const CONFIG_REPORT_ID: u8 = 0x08;
const PACKET_LENGTH: usize = 16;
const EXCHANGE_TIMEOUT: Duration = Duration::from_millis(1200);

mod command {
    pub const ENCRYPTION_DATA: u8 = 0x01;
    pub const DEVICE_ONLINE: u8 = 0x03;
    pub const WRITE_FLASH_DATA: u8 = 0x07;
    pub const READ_FLASH_DATA: u8 = 0x08;
}

mod flash {
    pub const REPORT_RATE: u16 = 0;
    pub const CURRENT_DPI: u16 = 4;
    pub const DPI_VALUES: u16 = 12;
}

const POLLING_RATES: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

fn packet_checksum(packet: &[u8; PACKET_LENGTH]) -> u8 {
    let mut sum: u32 = CONFIG_REPORT_ID as u32;
    for byte in &packet[..PACKET_LENGTH - 1] {
        sum += *byte as u32;
    }
    (0x55u32.wrapping_sub(sum & 0xff) & 0xff) as u8
}

fn data_checksum(data: &[u8]) -> u8 {
    let sum: u32 = data.iter().map(|byte| *byte as u32).sum();
    (0x55u32.wrapping_sub(sum & 0xff) & 0xff) as u8
}

fn decode_polling_rate(encoded: u8) -> Option<u32> {
    if encoded == 0 {
        return None;
    }
    Some(if encoded >= 16 {
        encoded as u32 / 16 * 2000
    } else {
        1000 / encoded as u32
    })
}

fn encode_polling_rate(rate_hz: u32) -> Result<u8> {
    if !POLLING_RATES.contains(&rate_hz) {
        bail!("{rate_hz} Hz is not a supported Pulsar polling rate");
    }
    Ok(if rate_hz <= 1000 {
        (1000 / rate_hz) as u8
    } else {
        (rate_hz / 2000 * 16) as u8
    })
}

/// The 10-step low range plus a dpiEx-flagged high range real Pulsar-vendor
/// flash mice (including the X2 CrazyLight, CID 0x57) use. Do not apply this
/// to the Pulsar 4K Wireless Receiver (vendor 0x3554) — it uses a flat
/// 50-step encoding with no dpiEx branching (`pulsarVgn*` in the TS source).
fn decode_dpi(data: &[u8]) -> Option<u32> {
    if data.len() < 4 {
        return None;
    }
    let (low, duplicate, flags, checksum) = (data[0], data[1], data[2], data[3]);
    if low != duplicate {
        return None;
    }
    let sum = (low as u32 + duplicate as u32 + flags as u32 + checksum as u32) & 0xff;
    if sum != 0x55 {
        return None;
    }
    let raw = low as u32 + (((flags & 0x0c) as u32 >> 2) << 8);
    let mut dpi = (raw + 1) * 10;
    if flags & 0x02 != 0 {
        dpi = dpi * 5 + 10000;
    }
    if flags & 0x01 != 0 {
        dpi *= 2;
    }
    Some(dpi)
}

fn encode_dpi(dpi: u32) -> [u8; 4] {
    let (raw, dpi_ex): (u32, u8) = if dpi >= 30100 {
        ((dpi / 2 - 10050) / 50, 0x33)
    } else if dpi >= 10050 {
        ((dpi - 10050) / 50, 0x22)
    } else {
        (dpi / 10 - 1, 0)
    };
    let high = (raw >> 8) as u8;
    let low = (raw & 0xff) as u8;
    let byte2 = (high << 2) | (high << 6) | dpi_ex | (dpi_ex << 4);
    let mut result = [low, low, byte2, 0];
    result[3] = data_checksum(&result[..3]);
    result
}

fn dpi_options() -> Vec<u32> {
    let mut options = Vec::new();
    let mut dpi = 10;
    while dpi <= 10_000 {
        options.push(dpi);
        dpi += 10;
    }
    let mut dpi = 10_050;
    while dpi <= 30_000 {
        options.push(dpi);
        dpi += 50;
    }
    let mut dpi = 30_100;
    while dpi <= 32_000 {
        options.push(dpi);
        dpi += 100;
    }
    options
}

/// A connected, identified Pulsar-vendor HID interface, ready to accept DPI
/// and polling-rate writes.
pub struct PulsarDevice {
    device: hidapi::HidDevice,
    max_polling_rate_hz: u32,
}

impl PulsarDevice {
    /// Scans every HID interface reporting Pulsar's vendor id and opens the
    /// first one that answers the identification query on report 8 — a mouse
    /// exposes several HID interfaces (boot mouse, consumer control, ...)
    /// sharing the same vendor id, and only one of them is this config
    /// channel, so wrong interfaces are expected to fail here and are
    /// skipped rather than treated as a hard error.
    pub fn open() -> Result<Self> {
        let api = HidApi::new().context("could not initialize the HID subsystem")?;
        let mut last_error: Option<anyhow::Error> = None;
        let candidates = api
            .device_list()
            .filter(|info| info.vendor_id() == VENDOR_ID)
            .map(|info| info.path().to_owned())
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            bail!("no HID interface with Pulsar's vendor id (0x{VENDOR_ID:04x}) was found");
        }
        for path in candidates {
            let device = match api.open_path(&path) {
                Ok(device) => device,
                Err(error) => {
                    last_error = Some(anyhow!(error));
                    continue;
                }
            };
            let candidate = Self {
                device,
                max_polling_rate_hz: 1000,
            };
            match candidate.identify() {
                Ok(max_polling_rate_hz) => {
                    return Ok(Self {
                        max_polling_rate_hz,
                        ..candidate
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("no Pulsar configuration interface answered")))
    }

    /// Confirms this interface is the report-8 config channel and reads back
    /// the connection's maximum polling rate, mirroring `readDeviceInfo` in
    /// the WebHID driver.
    fn identify(&self) -> Result<u32> {
        let mut challenge = [0u8; 8];
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or_default();
        challenge[..4].copy_from_slice(&nonce.to_le_bytes());
        let response = self.query(command::ENCRYPTION_DATA, &challenge)?;
        assert_accepted(&response, "identification")?;
        let device_type = response[11];
        Ok(match device_type {
            0 | 2 => 1000,
            1 => 4000,
            3 | 5 => 8000,
            4 => 2000,
            _ => 1000,
        })
    }

    /// Sets the polling rate, reads it back, and errors if the mouse did not
    /// actually keep the requested value.
    pub fn set_polling_rate(&self, rate_hz: u32) -> Result<u32> {
        if rate_hz > self.max_polling_rate_hz {
            bail!(
                "this connection supports at most {} Hz",
                self.max_polling_rate_hz
            );
        }
        let encoded = encode_polling_rate(rate_hz)?;
        self.with_device_control(|| {
            self.write_checked_byte(flash::REPORT_RATE, encoded)?;
            let readback = self.read_flash(flash::REPORT_RATE, 2)?;
            let confirmed = decode_polling_rate(readback[0]);
            match confirmed {
                Some(value) if value == rate_hz => Ok(value),
                Some(value) => bail!("the mouse kept {value} Hz instead of {rate_hz} Hz"),
                None => bail!("the mouse did not confirm the requested polling rate"),
            }
        })
    }

    /// Sets DPI on the mouse's currently active DPI stage, reads it back, and
    /// errors if the mouse did not actually keep the requested value.
    pub fn set_dpi(&self, dpi: u32) -> Result<u32> {
        if !dpi_options().contains(&dpi) {
            bail!("{dpi} DPI is not supported by this Pulsar sensor");
        }
        self.with_device_control(|| {
            let current_stage = self.read_flash(flash::CURRENT_DPI, 2)?[0].min(7) as u16;
            let address = flash::DPI_VALUES + current_stage * 4;
            self.write_flash(address, &encode_dpi(dpi))?;
            let confirmed = decode_dpi(&self.read_flash(address, 4)?);
            match confirmed {
                Some(value) if value == dpi => Ok(value),
                Some(value) => bail!("the mouse kept {value} DPI instead of {dpi} DPI"),
                None => bail!("the mouse did not confirm the requested DPI"),
            }
        })
    }

    /// Wraps a flash read/write in the receiver's host-control handshake:
    /// entered before the operation, always exited afterward (best-effort —
    /// leaving the receiver stuck in host-control mode would also break its
    /// normal button/movement reporting).
    fn with_device_control<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.set_device_online(true)?;
        let result = operation();
        let _ = self.set_device_online(false);
        result
    }

    fn set_device_online(&self, enabled: bool) -> Result<()> {
        for _ in 0..20 {
            let mut packet = [0u8; PACKET_LENGTH];
            packet[0] = command::DEVICE_ONLINE;
            packet[5] = enabled as u8;
            let response = self.exchange(packet)?;
            let label = if enabled {
                "host-control entry"
            } else {
                "host-control exit"
            };
            assert_accepted(&response, label)?;
            if response[9] != 1 {
                if enabled && response[5] != 1 {
                    bail!("the Pulsar mouse is offline — move it or click a button, then retry");
                }
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        bail!("the Pulsar receiver stayed busy")
    }

    fn read_flash(&self, address: u16, length: usize) -> Result<Vec<u8>> {
        let mut result = vec![0u8; length];
        let mut offset = 0usize;
        while offset < length {
            let count = (length - offset).min(10);
            let mut packet = [0u8; PACKET_LENGTH];
            packet[0] = command::READ_FLASH_DATA;
            let current = address + offset as u16;
            packet[2] = (current >> 8) as u8;
            packet[3] = (current & 0xff) as u8;
            packet[4] = count as u8;
            let response = self.exchange(packet)?;
            assert_accepted(&response, "configuration read")?;
            result[offset..offset + count].copy_from_slice(&response[5..5 + count]);
            offset += count;
        }
        Ok(result)
    }

    fn write_flash(&self, address: u16, data: &[u8]) -> Result<()> {
        let mut offset = 0usize;
        while offset < data.len() {
            let count = (data.len() - offset).min(10);
            let mut packet = [0u8; PACKET_LENGTH];
            packet[0] = command::WRITE_FLASH_DATA;
            let current = address + offset as u16;
            packet[2] = (current >> 8) as u8;
            packet[3] = (current & 0xff) as u8;
            packet[4] = count as u8;
            packet[5..5 + count].copy_from_slice(&data[offset..offset + count]);
            let response = self.exchange(packet)?;
            assert_accepted(&response, "configuration write")?;
            offset += count;
        }
        Ok(())
    }

    fn write_checked_byte(&self, address: u16, value: u8) -> Result<()> {
        self.write_flash(
            address,
            &[value, (0x55u16.wrapping_sub(value as u16) & 0xff) as u8],
        )
    }

    fn query(&self, command: u8, parameters: &[u8]) -> Result<[u8; PACKET_LENGTH]> {
        if parameters.len() > 10 {
            bail!("Pulsar queries support at most 10 parameter bytes");
        }
        let mut packet = [0u8; PACKET_LENGTH];
        packet[0] = command;
        packet[4] = parameters.len() as u8;
        packet[5..5 + parameters.len()].copy_from_slice(parameters);
        self.exchange(packet)
    }

    /// Writes report 8 and waits for the matching input report (same command
    /// byte echoed back), ignoring any other unsolicited reports in between.
    fn exchange(&self, mut packet: [u8; PACKET_LENGTH]) -> Result<[u8; PACKET_LENGTH]> {
        packet[PACKET_LENGTH - 1] = packet_checksum(&packet);
        let command = packet[0];
        let mut frame = [0u8; PACKET_LENGTH + 1];
        frame[0] = CONFIG_REPORT_ID;
        frame[1..].copy_from_slice(&packet);
        self.device.write(&frame).with_context(|| {
            format!("could not write Pulsar report 8 (command 0x{command:02x})")
        })?;

        let deadline = Instant::now() + EXCHANGE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("the Pulsar mouse did not answer command 0x{command:02x}");
            }
            let mut buffer = [0u8; PACKET_LENGTH + 1];
            let read = self
                .device
                .read_timeout(&mut buffer, remaining.as_millis().max(1) as i32)
                .context("could not read a Pulsar HID report")?;
            if read == 0 {
                continue;
            }
            // Some backends prefix the report id, some don't — accept both.
            let body = if read == buffer.len() && buffer[0] == CONFIG_REPORT_ID {
                &buffer[1..read]
            } else {
                &buffer[..read.min(PACKET_LENGTH)]
            };
            if body.first() == Some(&command) {
                let mut response = [0u8; PACKET_LENGTH];
                let copy_len = body.len().min(PACKET_LENGTH);
                response[..copy_len].copy_from_slice(&body[..copy_len]);
                return Ok(response);
            }
        }
    }
}

fn assert_accepted(response: &[u8; PACKET_LENGTH], operation: &str) -> Result<()> {
    if response[1] != 0 {
        bail!(
            "the Pulsar receiver rejected the {operation} (status {})",
            response[1]
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_round_trips_through_encode_and_decode() {
        for dpi in [400, 800, 1600, 3200, 10_000, 10_050, 20_000, 30_100, 32_000] {
            let encoded = encode_dpi(dpi);
            assert_eq!(
                decode_dpi(&encoded),
                Some(dpi),
                "dpi={dpi} encoded={encoded:?}"
            );
        }
    }

    #[test]
    fn polling_rate_round_trips_through_encode_and_decode() {
        for rate in POLLING_RATES {
            let encoded = encode_polling_rate(rate).unwrap();
            assert_eq!(
                decode_polling_rate(encoded),
                Some(rate),
                "rate={rate} encoded={encoded}"
            );
        }
    }

    #[test]
    fn unsupported_polling_rate_is_rejected() {
        assert!(encode_polling_rate(333).is_err());
    }

    #[test]
    fn dpi_options_cover_the_full_range_in_documented_steps() {
        let options = dpi_options();
        assert_eq!(options.first(), Some(&10));
        assert_eq!(options.last(), Some(&32_000));
        assert!(options.contains(&800));
        assert!(options.contains(&10_050));
        assert!(options.contains(&30_100));
    }

    #[test]
    fn packet_checksum_matches_the_hardware_verified_pulsar_formula() {
        // 0x55 - (CONFIG_REPORT_ID + sum(bytes[0..14])) & 0xff, over a
        // deviceOnline(true) packet — mirrors pulsarPacketChecksum in
        // mouse-protocol/src/pulsar/index.ts.
        let mut packet = [0u8; PACKET_LENGTH];
        packet[0] = command::DEVICE_ONLINE;
        packet[5] = 1;
        let checksum = packet_checksum(&packet);
        let mut sum: u32 = CONFIG_REPORT_ID as u32;
        for byte in &packet[..PACKET_LENGTH - 1] {
            sum += *byte as u32;
        }
        assert_eq!(checksum, (0x55u32.wrapping_sub(sum & 0xff) & 0xff) as u8);
    }
}
