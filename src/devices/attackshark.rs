//! Attack Shark X11 / R1 wire protocol (VID 0x1d57).
//!
//! These are pure encoders/decoders with no I/O so they can be unit-tested
//! against the reference driver's own vectors
//! (dressedinblack5/attack-shark-x11-electron). The transport that carries
//! them lives in the parent module.
//!
//! Config travels as HID feature reports on USB interface 2 — the exact
//! channel a browser cannot reach, because Chrome hides those collections and
//! never lets a page write to them. A native HID handle has no such block, so
//! the Bridge can send these where the web app cannot.
//!
//! Safety: only the fully specified, low-risk operations are encoded here
//! (polling rate, and read-only battery/polling readback). DPI, RGB, macros
//! and reset involve large packets with many fixed bytes and a reverse-
//! engineered checksum; sending those from scratch risks corrupting unknown
//! state, so they are deliberately left to the native reference driver until a
//! read-modify-write path is verified on hardware.

/// Attack Shark OEM vendor id shared by the X11 and R1 families.
pub const X11_VID: u16 = 0x1d57;

/// Documented product ids: wired X11, wireless X11 receiver, R1.
pub const X11_WIRED_PID: u16 = 0xfa55;
pub const X11_WIRELESS_PID: u16 = 0xfa60;
pub const R1_PID: u16 = 0xfa61;

/// USB interface that carries the configuration feature reports.
pub const CONTROL_INTERFACE: i32 = 2;

/// Feature report id the polling-rate command is written on.
pub const POLLING_REPORT_ID: u8 = 0x06;
/// Feature report id used to request a state read-back.
pub const READ_REQUEST_REPORT_ID: u8 = 0xa0;
/// Input report id the autonomous battery packet arrives on.
pub const BATTERY_REPORT_ID: u8 = 0x03;

/// Battery input report signature; byte 4 is the percentage.
const BATTERY_SIGNATURE: [u8; 4] = [0x03, 0x55, 0x40, 0x01];

/// Supported polling rates as (hz, wire code). The code is the value written
/// at byte 3; the checksum byte is `0xff - code`.
pub const POLLING_RATES: [(u16, u8); 4] = [(125, 0x08), (250, 0x04), (500, 0x02), (1000, 0x01)];

/// True when a VID/PID pair is an X11-family unit this module understands.
pub fn is_x11(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == X11_VID && matches!(product_id, X11_WIRED_PID | X11_WIRELESS_PID | R1_PID)
}

/// Human model name for a product id.
pub fn model_name(product_id: u16) -> &'static str {
    match product_id {
        X11_WIRED_PID | X11_WIRELESS_PID => "Attack Shark X11",
        R1_PID => "Attack Shark R1",
        _ => "Attack Shark",
    }
}

/// Only the wireless receiver reports battery and connects wirelessly.
pub fn is_wireless(product_id: u16) -> bool {
    product_id == X11_WIRELESS_PID
}

/// Polling rates this family accepts, in ascending order.
pub fn supported_polling_rates() -> Vec<u16> {
    POLLING_RATES.iter().map(|&(hz, _)| hz).collect()
}

/// Encode the X11 polling-rate feature report, or `None` for an unsupported
/// rate. Byte 0 is the report id (hidapi consumes it as the leading byte).
///
/// Layout: `06 09 01 <code> <0xff-code> 00 00 00 00`.
pub fn polling_packet(hz: u16) -> Option<[u8; 9]> {
    let code = POLLING_RATES
        .iter()
        .find_map(|&(rate, code)| (rate == hz).then_some(code))?;
    Some([
        POLLING_REPORT_ID,
        0x09,
        0x01,
        code,
        0xff - code,
        0x00,
        0x00,
        0x00,
        0x00,
    ])
}

/// Encode the read-request that asks the mouse to publish its current polling
/// rate on `POLLING_REPORT_ID`. Byte 0 is the `READ_REQUEST_REPORT_ID`.
pub fn polling_read_request() -> [u8; 8] {
    [
        READ_REQUEST_REPORT_ID,
        0x00,
        0x01,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
    ]
}

/// Decode a polling-rate feature report read back from the mouse. `report`
/// includes the leading report id, so the rate code sits at index 2. Returns
/// the rate in Hz, or `None` if the code is unknown or the buffer is short.
pub fn parse_polling_reply(report: &[u8]) -> Option<u16> {
    let code = *report.get(2)?;
    POLLING_RATES
        .iter()
        .find_map(|&(hz, wire)| (wire == code).then_some(hz))
}

/// Decode a battery input report. `report` includes the leading report id, so
/// the signature occupies bytes 0..4 and the percentage byte 4. Returns the
/// percentage (0..=100) or `None` for a non-battery or out-of-range report.
pub fn parse_battery(report: &[u8]) -> Option<u8> {
    if report.len() < 5 || report[..4] != BATTERY_SIGNATURE {
        return None;
    }
    let percent = report[4];
    (percent <= 100).then_some(percent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn polling_packets_match_reference_vectors() {
        // From attack-shark-x11-electron __tests__/PollingRateBuilder.test.ts.
        assert_eq!(hex(&polling_packet(125).unwrap()), "06090108f700000000");
        assert_eq!(hex(&polling_packet(250).unwrap()), "06090104fb00000000");
        assert_eq!(hex(&polling_packet(500).unwrap()), "06090102fd00000000");
        assert_eq!(hex(&polling_packet(1000).unwrap()), "06090101fe00000000");
    }

    #[test]
    fn polling_packet_rejects_unsupported_rates() {
        assert!(polling_packet(2000).is_none());
        assert!(polling_packet(0).is_none());
    }

    #[test]
    fn polling_reply_round_trips_every_rate() {
        for &(hz, code) in &POLLING_RATES {
            let reply = [POLLING_REPORT_ID, 0x00, code, 0x00];
            assert_eq!(parse_polling_reply(&reply), Some(hz));
        }
        assert_eq!(parse_polling_reply(&[POLLING_REPORT_ID, 0x00, 0x99]), None);
        assert_eq!(parse_polling_reply(&[POLLING_REPORT_ID]), None);
    }

    #[test]
    fn battery_parse_validates_signature_and_range() {
        assert_eq!(parse_battery(&[0x03, 0x55, 0x40, 0x01, 80]), Some(80));
        assert_eq!(parse_battery(&[0x03, 0x55, 0x40, 0x01, 100]), Some(100));
        // Wrong signature, out of range, and short buffers are rejected.
        assert_eq!(parse_battery(&[0x03, 0x55, 0x40, 0x00, 80]), None);
        assert_eq!(parse_battery(&[0x03, 0x55, 0x40, 0x01, 101]), None);
        assert_eq!(parse_battery(&[0x03, 0x55, 0x40, 0x01]), None);
    }

    #[test]
    fn family_recognition_covers_documented_pids() {
        assert!(is_x11(0x1d57, 0xfa55));
        assert!(is_x11(0x1d57, 0xfa60));
        assert!(is_x11(0x1d57, 0xfa61));
        assert!(!is_x11(0x1d57, 0x1234));
        assert!(!is_x11(0x25a7, 0xfa60));
        assert_eq!(model_name(0xfa61), "Attack Shark R1");
        assert!(is_wireless(0xfa60));
        assert!(!is_wireless(0xfa55));
    }
}
