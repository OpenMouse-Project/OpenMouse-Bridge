//! Native HID driver for Delux mice (including Delux M800 Mini, M800 Pro,
//! and related OEM variants), allowing Bridge to push DPI and polling-rate
//! profile changes directly.
//!
//! Delux mice across hardware revisions commonly use one of three MCU / OEM
//! platforms:
//! 1. 0x1D57 (X11 / BK3633 OEM platform, used by M800 Mini wireless 0xFA60,
//!    wired 0xFA55, and R1 0xFA61):
//!    - Polling rate: Feature report 0x06 (9 bytes, Bit7 checksum)
//!    - DPI: Feature report 0x04 (56 bytes wireless / 52 bytes wired)
//! 2. 0x248A (Delux proprietary VID, used by M800 Pro / M800 Mini 3395):
//!    - Feature report 0x0C (33 bytes)
//! 3. 0x373E (CompX OEM platform):
//!    - Feature report 0x00 (64 bytes)

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hidapi::HidApi;

use crate::config::ApplicationProfile;

/// Brand prefix registered in Bridge matching the `"delux:"` prefix written
/// into `device.id`.
pub const BRAND: &str = "delux";

/// Known Vendor IDs used by Delux mice and receivers.
pub const VENDOR_ID_1D57: u16 = 0x1d57;
pub const VENDOR_ID_248A: u16 = 0x248a;
pub const VENDOR_ID_373E: u16 = 0x373e;

pub const SUPPORTED_VENDOR_IDS: &[u16] = &[VENDOR_ID_1D57, VENDOR_ID_248A, VENDOR_ID_373E];

// ── 0x1D57 (X11 / M800 Mini OEM) protocol constants ──────────────────────────

const POLLING_REPORT_ID_1D57: u8 = 0x06;
const DPI_REPORT_ID_1D57: u8 = 0x04;
const DPI_STAGE_COUNT_1D57: usize = 6;
const DEFAULT_STAGES_1D57: [u32; 6] = [800, 1600, 2400, 3200, 5000, 22000];

// ── 0x248A protocol constants ────────────────────────────────────────────────

const REPORT_ID_248A: u8 = 0x0c;

/// Attempts to apply profile settings directly to a connected Delux mouse.
/// Returns `Ok(true)` if a recognized Delux device was claimed and updated,
/// or `Ok(false)` if no matching native device could be opened (allowing
/// fallback to the Node.js native-hid helper).
pub fn apply(profile: &ApplicationProfile) -> Result<bool> {
    let api = match HidApi::new() {
        Ok(api) => api,
        Err(_) => return Ok(false),
    };

    // Try finding and driving 0x1D57 first (the primary M800 Mini platform)
    if let Ok(device) = Delux1d57Device::open(&api) {
        if let Some(dpi) = profile.settings.dpi {
            device.set_dpi(dpi)?;
            std::thread::sleep(Duration::from_millis(150));
        }
        if let Some(rate) = profile.settings.polling_rate_hz {
            device.set_polling_rate(rate)?;
        }
        return Ok(true);
    }

    // Next try 0x248A (M800 Pro / Mini variant)
    if let Ok(device) = Delux248aDevice::open(&api) {
        if let Some(dpi) = profile.settings.dpi {
            device.set_dpi(dpi)?;
            std::thread::sleep(Duration::from_millis(150));
        }
        if let Some(rate) = profile.settings.polling_rate_hz {
            device.set_polling_rate(rate)?;
        }
        return Ok(true);
    }

    Ok(false)
}

// ── 0x1D57 Device Driver ─────────────────────────────────────────────────────

pub struct Delux1d57Device {
    device: hidapi::HidDevice,
    is_wired: bool,
}

impl Delux1d57Device {
    pub fn open(api: &HidApi) -> Result<Self> {
        let mut last_error: Option<anyhow::Error> = None;
        let candidates = api
            .device_list()
            .filter(|info| info.vendor_id() == VENDOR_ID_1D57)
            .collect::<Vec<_>>();

        if candidates.is_empty() {
            bail!("no 0x1d57 Delux device found");
        }

        for info in candidates {
            let device = match api.open_path(info.path()) {
                Ok(dev) => dev,
                Err(err) => {
                    last_error = Some(anyhow!(err));
                    continue;
                }
            };

            // Test if this interface accepts polling-rate feature report 0x06
            let mut test_buf = [0u8; 9];
            test_buf[0] = POLLING_REPORT_ID_1D57;
            match device.get_feature_report(&mut test_buf) {
                Ok(_) => {
                    let is_wired = info.product_id() == 0xfa55;
                    return Ok(Self { device, is_wired });
                }
                Err(err) => {
                    last_error = Some(anyhow!(err));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("no responsive 0x1d57 interface found")))
    }

    pub fn set_polling_rate(&self, rate_hz: u32) -> Result<()> {
        let rate_byte = match rate_hz {
            125 => 0x08,
            250 => 0x04,
            500 => 0x02,
            1000 => 0x01,
            _ => bail!("{rate_hz} Hz is not supported for Delux M800 Mini (supports 125, 250, 500, 1000 Hz)"),
        };

        let checksum = (0xff - rate_byte) & 0xff;
        let payload = [
            POLLING_REPORT_ID_1D57,
            0x09,
            0x01,
            rate_byte,
            checksum,
            0x00,
            0x00,
            0x00,
            0x00,
        ];

        self.device
            .send_feature_report(&payload)
            .context("failed to write Delux polling rate report 0x06")?;
        Ok(())
    }

    pub fn set_dpi(&self, dpi: u32) -> Result<()> {
        let report = build_1d57_dpi_report(dpi, self.is_wired);
        self.device
            .send_feature_report(&report)
            .context("failed to write Delux DPI feature report 0x04")?;
        Ok(())
    }
}

// ── 0x248A Device Driver ─────────────────────────────────────────────────────

pub struct Delux248aDevice {
    device: hidapi::HidDevice,
}

impl Delux248aDevice {
    pub fn open(api: &HidApi) -> Result<Self> {
        let mut last_error: Option<anyhow::Error> = None;
        let candidates = api
            .device_list()
            .filter(|info| info.vendor_id() == VENDOR_ID_248A)
            .collect::<Vec<_>>();

        if candidates.is_empty() {
            bail!("no 0x248a Delux device found");
        }

        for info in candidates {
            let device = match api.open_path(info.path()) {
                Ok(dev) => dev,
                Err(err) => {
                    last_error = Some(anyhow!(err));
                    continue;
                }
            };

            // Test feature report 0x0C
            let mut test_buf = [0u8; 33];
            test_buf[0] = REPORT_ID_248A;
            match device.get_feature_report(&mut test_buf) {
                Ok(_) => return Ok(Self { device }),
                Err(err) => last_error = Some(anyhow!(err)),
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("no responsive 0x248a interface found")))
    }

    pub fn set_polling_rate(&self, rate_hz: u32) -> Result<()> {
        let rate_byte = match rate_hz {
            125 => 0x08,
            250 => 0x04,
            500 => 0x02,
            1000 => 0x01,
            _ => bail!("{rate_hz} Hz is not supported for Delux M800 Pro (supports 125, 250, 500, 1000 Hz)"),
        };

        let mut payload = [0u8; 33];
        payload[0] = REPORT_ID_248A;
        payload[1] = 0x01;
        payload[2] = 0x07;
        payload[4] = 0x02; // Packet ID + 1
        payload[5] = 0x01;
        payload[6] = 0x01;
        payload[7] = rate_byte;

        self.device
            .send_feature_report(&payload)
            .context("failed to write 0x248A polling rate report")?;
        Ok(())
    }

    pub fn set_dpi(&self, dpi: u32) -> Result<()> {
        let clamped_dpi = dpi.clamp(50, 26000);
        let dpi_val = (clamped_dpi / 50).saturating_sub(1) as u16;

        let mut payload = [0u8; 33];
        payload[0] = REPORT_ID_248A;
        payload[1] = 0x01;
        payload[2] = 0x05;
        payload[4] = 0x02; // Packet ID + 1
        payload[5] = 0x01;
        payload[6] = 0x12;
        payload[7] = 0x11; // 1 enabled stage
        payload[8] = 0x01; // active stage mask (stage 1)
        payload[9] = (dpi_val & 0xff) as u8;
        payload[10] = ((dpi_val >> 8) & 0xff) as u8;

        self.device
            .send_feature_report(&payload)
            .context("failed to write 0x248A DPI report")?;
        Ok(())
    }
}

// ── 0x1D57 DPI Report Builder Helper ─────────────────────────────────────────

fn encode_1d57_dpi_byte(dpi: u32) -> u8 {
    // Standard PAW3395/3370 DPI step register map for X11/M800 platform
    // 50 -> 0x01, 800 -> 0x12, 1600 -> 0x25, 3200 -> 0x4B, 5000 -> 0x75, ...
    let clamped = dpi.clamp(50, 22000);
    if clamped <= 10000 {
        // Linear approximate mapping matching X11_DPI_STEP_MAP
        match clamped {
            50..=300 => (clamped / 50) as u8,
            301..=750 => (clamped / 50 + 1) as u8,
            751..=1100 => (clamped / 50 + 2) as u8,
            1101..=1400 => (clamped / 50 + 3) as u8,
            1401..=1700 => (clamped / 50 + 4) as u8,
            1701..=2000 => (clamped / 50 + 5) as u8,
            2001..=2250 => (clamped / 50 + 6) as u8,
            2251..=2550 => (clamped / 50 + 7) as u8,
            2551..=2850 => (clamped / 50 + 8) as u8,
            2851..=3150 => (clamped / 50 + 9) as u8,
            3151..=3400 => (clamped / 50 + 10) as u8,
            3401..=3700 => (clamped / 50 + 11) as u8,
            3701..=4000 => (clamped / 50 + 12) as u8,
            4001..=4300 => (clamped / 50 + 13) as u8,
            4301..=4550 => (clamped / 50 + 14) as u8,
            4551..=4850 => (clamped / 50 + 15) as u8,
            4851..=5150 => (clamped / 50 + 16) as u8,
            _ => ((clamped / 50) as u8).saturating_add(17),
        }
    } else {
        // High register page for > 10,000 DPI
        0x76
    }
}

fn build_1d57_dpi_report(active_dpi: u32, is_wired: bool) -> Vec<u8> {
    let mut buffer = vec![0u8; if is_wired { 52 } else { 56 }];
    buffer[0] = DPI_REPORT_ID_1D57;
    buffer[1] = 0x38;
    buffer[2] = 0x01;
    buffer[3] = 0x00; // Angle snap off
    buffer[4] = 0x01; // Ripple control on
    buffer[5] = 0x3f;

    let mut stages = DEFAULT_STAGES_1D57;
    stages[0] = active_dpi;

    let mut mask = 0u8;
    for (i, &stage_dpi) in stages.iter().enumerate().take(DPI_STAGE_COUNT_1D57) {
        buffer[8 + i] = encode_1d57_dpi_byte(stage_dpi);
        if stage_dpi > 12000 {
            mask |= 1 << i;
        }
        let high = (10100..=12000).contains(&stage_dpi) || (20100..=22000).contains(&stage_dpi);
        buffer[16 + i] = if high { 1 } else { 0 };
    }
    buffer[6] = mask;
    buffer[7] = mask;
    buffer[24] = 1; // Active stage: 1

    buffer[25] = 0xff;
    buffer[29] = 0xff;
    buffer[33] = 0xff;
    buffer[34] = 0xff;
    buffer[35] = 0xff;
    buffer[38] = 0xff;
    buffer[39] = 0xff;
    buffer[40] = 0xff;
    buffer[42] = 0xff;
    buffer[43] = 0xff;
    buffer[44] = 0x40;
    buffer[46] = 0xff;
    buffer[47] = 0xff;
    buffer[48] = 0xff;
    buffer[49] = 0x02;

    let mut sum: u16 = 0;
    for i in 3..=49 {
        sum = sum.wrapping_add(buffer[i] as u16);
    }
    buffer[50] = ((sum >> 8) & 0xff) as u8;
    buffer[51] = (sum & 0xff) as u8;

    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delux_brand_constant() {
        assert_eq!(BRAND, "delux");
    }

    #[test]
    fn test_1d57_dpi_report_structure() {
        let report = build_1d57_dpi_report(800, false);
        assert_eq!(report.len(), 56);
        assert_eq!(report[0], DPI_REPORT_ID_1D57);
        assert_eq!(report[1], 0x38);
        assert_eq!(report[2], 0x01);
        assert_eq!(report[24], 1); // active stage
    }

    #[test]
    fn test_1d57_dpi_report_wired_length() {
        let report = build_1d57_dpi_report(1600, true);
        assert_eq!(report.len(), 52);
    }
}
