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

/// USB interface that carries the configuration channel.
pub const CONTROL_INTERFACE: u8 = 2;

/// The config channel is reached with raw USB control transfers, exactly like
/// the reference driver (dressedinblack5/attack-shark-x11-electron): a HID
/// class SET_REPORT to interface 2. Windows' HID stack refuses these because
/// the descriptor declares no feature reports, so this must go through a raw
/// USB handle (WinUSB / Zadig on Windows, hidraw/libusb elsewhere).
///
/// bmRequestType 0x21 = Host->Device | Class | Interface.
pub const SET_REPORT_REQUEST: u8 = 0x09;
/// wValue high byte: HID report type 0x03 = Feature.
pub const FEATURE_REPORT_TYPE: u16 = 0x03;
/// Feature report id the polling-rate command is written on.
pub const POLLING_REPORT_ID: u8 = 0x06;
/// wValue for the polling SET_REPORT: (Feature << 8) | report id = 0x0306.
pub const POLLING_WVALUE: u16 = (FEATURE_REPORT_TYPE << 8) | POLLING_REPORT_ID as u16;
/// Feature report id the DPI/stage command is written on.
pub const DPI_REPORT_ID: u8 = 0x04;
/// wValue for the DPI SET_REPORT: (Feature << 8) | report id = 0x0304.
pub const DPI_WVALUE: u16 = (FEATURE_REPORT_TYPE << 8) | DPI_REPORT_ID as u16;
/// Interrupt IN endpoint that streams battery packets on the wireless receiver.
pub const BATTERY_ENDPOINT: u8 = 0x83;
/// Input report id the autonomous battery packet arrives on.
pub const BATTERY_REPORT_ID: u8 = 0x03;

/// The X11 has six DPI stages; DPI runs 50–22000 in 50-step increments.
pub const DPI_STAGE_COUNT: usize = 6;
pub const DPI_MIN: u16 = 50;
pub const DPI_MAX: u16 = 22_000;
pub const DPI_STEP: u16 = 50;

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

/// The fixed 56-byte DPI template. Bytes the firmware expects verbatim are
/// pre-set here (matching the reference driver's DpiBuilder); the dynamic
/// fields — angle snap, rippler, the six stage bytes, the stage masks, the
/// current stage and the checksum — are written by `dpi_packet`. Byte 0 is the
/// report id.
#[rustfmt::skip]
const DPI_TEMPLATE: [u8; 56] = [
    0x04, 0x38, 0x01, 0x00, 0x01, 0x3f, 0x20, 0x20,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00,
    0x02,
    0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x00, 0x00, 0xff, 0xff, 0xff,
    0x00, 0xff, 0xff, 0x40, 0x00, 0xff, 0xff, 0xff,
    0x02,
    0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

/// Encode a DPI value to its sensor byte: the smallest supported step >= dpi.
/// `None` when dpi exceeds the sensor maximum.
pub fn encode_dpi(dpi: u16) -> Option<u8> {
    DPI_STEP_MAP
        .iter()
        .find(|&&(step, _)| step >= dpi)
        .map(|&(_, code)| code)
}

/// Build the 56-byte DPI feature report for the six stage values and the active
/// stage (1-based). `None` if a stage DPI is unsupported or the active stage is
/// out of range. Byte 0 is the report id.
///
/// Mirrors the reference DpiBuilder.build(): stage mask bit set per stage above
/// 12000 DPI; high-stage flag set when a stage lands in an upper register page
/// ([10100,12000] or [20100,22000]); checksum = sum(bytes 3..=49) & 0xffff,
/// stored big-endian at bytes 50-51.
pub fn dpi_packet(
    stages: [u16; DPI_STAGE_COUNT],
    active_stage: u8,
    angle_snap: bool,
    rippler: bool,
) -> Option<[u8; 56]> {
    if !(1..=DPI_STAGE_COUNT as u8).contains(&active_stage) {
        return None;
    }
    let mut buf = DPI_TEMPLATE;
    buf[3] = angle_snap as u8;
    buf[4] = rippler as u8;
    buf[24] = active_stage;

    let mut mask = 0u8;
    for (i, &dpi) in stages.iter().enumerate() {
        buf[8 + i] = encode_dpi(dpi)?;
        if dpi > 12_000 {
            mask |= 1 << i;
        }
        let high = (10_100..=12_000).contains(&dpi) || (20_100..=22_000).contains(&dpi);
        buf[16 + i] = high as u8;
    }
    buf[6] = mask;
    buf[7] = mask;

    let checksum = buf[3..=49].iter().map(|&b| b as u32).sum::<u32>() & 0xffff;
    buf[50] = (checksum >> 8) as u8;
    buf[51] = (checksum & 0xff) as u8;
    Some(buf)
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

/// Prefix of the DPI-button notification. When the physical DPI button cycles
/// the active stage, the mouse pushes `03 55 10 <stage> 00` on the same
/// interrupt endpoint as battery; byte 3 is the new active stage (1-based).
/// (Battery uses `03 55 40 01 …`; byte 2 — 0x10 vs 0x40 — tells them apart.)
const DPI_NOTIFY_PREFIX: [u8; 3] = [0x03, 0x55, 0x10];

/// Decode a DPI-button notification, returning the new active stage (1..=6), or
/// `None` for any other packet.
pub fn parse_dpi_stage(report: &[u8]) -> Option<u8> {
    if report.len() < 4 || report[..3] != DPI_NOTIFY_PREFIX {
        return None;
    }
    let stage = report[3];
    (1..=DPI_STAGE_COUNT as u8)
        .contains(&stage)
        .then_some(stage)
}

/// DPI value → sensor byte, ascending by DPI. From the reference driver's
/// dpi-map.ts. Codes above 10000 intentionally repeat lower codes; the
/// stage mask and high-stage flags select the sensor's register page.
#[rustfmt::skip]
static DPI_STEP_MAP: &[(u16, u8)] = &[
    (50, 0x01),
    (100, 0x02),
    (150, 0x03),
    (200, 0x04),
    (250, 0x05),
    (300, 0x06),
    (350, 0x08),
    (400, 0x09),
    (450, 0x0a),
    (500, 0x0b),
    (550, 0x0c),
    (600, 0x0e),
    (650, 0x0f),
    (700, 0x10),
    (750, 0x11),
    (800, 0x12),
    (850, 0x13),
    (900, 0x15),
    (950, 0x16),
    (1000, 0x17),
    (1050, 0x18),
    (1100, 0x19),
    (1150, 0x1b),
    (1200, 0x1c),
    (1250, 0x1d),
    (1300, 0x1e),
    (1350, 0x1f),
    (1400, 0x20),
    (1450, 0x22),
    (1500, 0x23),
    (1550, 0x24),
    (1600, 0x25),
    (1650, 0x26),
    (1700, 0x27),
    (1750, 0x29),
    (1800, 0x2a),
    (1850, 0x2b),
    (1900, 0x2c),
    (1950, 0x2d),
    (2000, 0x2f),
    (2050, 0x30),
    (2100, 0x31),
    (2150, 0x32),
    (2200, 0x33),
    (2250, 0x34),
    (2300, 0x36),
    (2350, 0x37),
    (2400, 0x38),
    (2450, 0x39),
    (2500, 0x3a),
    (2550, 0x3b),
    (2600, 0x3d),
    (2650, 0x3e),
    (2700, 0x3f),
    (2750, 0x40),
    (2800, 0x41),
    (2850, 0x43),
    (2900, 0x44),
    (2950, 0x45),
    (3000, 0x46),
    (3050, 0x47),
    (3100, 0x48),
    (3150, 0x4a),
    (3200, 0x4b),
    (3250, 0x4c),
    (3300, 0x4d),
    (3350, 0x4e),
    (3400, 0x4f),
    (3450, 0x51),
    (3500, 0x52),
    (3550, 0x53),
    (3600, 0x54),
    (3650, 0x55),
    (3700, 0x57),
    (3750, 0x58),
    (3800, 0x59),
    (3850, 0x5a),
    (3900, 0x5b),
    (3950, 0x5c),
    (4000, 0x5e),
    (4050, 0x5f),
    (4100, 0x60),
    (4150, 0x61),
    (4200, 0x62),
    (4250, 0x63),
    (4300, 0x65),
    (4350, 0x66),
    (4400, 0x67),
    (4450, 0x68),
    (4500, 0x69),
    (4550, 0x6b),
    (4600, 0x6c),
    (4650, 0x6d),
    (4700, 0x6e),
    (4750, 0x6f),
    (4800, 0x70),
    (4850, 0x72),
    (4900, 0x73),
    (4950, 0x74),
    (5000, 0x75),
    (5050, 0x76),
    (5100, 0x77),
    (5150, 0x79),
    (5200, 0x7a),
    (5250, 0x7b),
    (5300, 0x7c),
    (5350, 0x7d),
    (5400, 0x7f),
    (5450, 0x80),
    (5500, 0x81),
    (5550, 0x82),
    (5600, 0x83),
    (5650, 0x84),
    (5700, 0x86),
    (5750, 0x87),
    (5800, 0x88),
    (5850, 0x89),
    (5900, 0x8a),
    (5950, 0x8b),
    (6000, 0x8d),
    (6050, 0x8e),
    (6100, 0x8f),
    (6150, 0x90),
    (6200, 0x91),
    (6250, 0x93),
    (6300, 0x94),
    (6350, 0x95),
    (6400, 0x96),
    (6450, 0x97),
    (6500, 0x98),
    (6550, 0x9a),
    (6600, 0x9b),
    (6650, 0x9c),
    (6700, 0x9d),
    (6750, 0x9e),
    (6800, 0x9f),
    (6850, 0xa1),
    (6900, 0xa2),
    (6950, 0xa3),
    (7000, 0xa4),
    (7050, 0xa5),
    (7100, 0xa7),
    (7150, 0xa8),
    (7200, 0xa9),
    (7250, 0xaa),
    (7300, 0xab),
    (7350, 0xac),
    (7400, 0xae),
    (7450, 0xaf),
    (7500, 0xb0),
    (7550, 0xb1),
    (7600, 0xb2),
    (7650, 0xb3),
    (7700, 0xb5),
    (7750, 0xb6),
    (7800, 0xb7),
    (7850, 0xb8),
    (7900, 0xb9),
    (7950, 0xbb),
    (8000, 0xbc),
    (8050, 0xbd),
    (8100, 0xbe),
    (8150, 0xbf),
    (8200, 0xc0),
    (8250, 0xc2),
    (8300, 0xc3),
    (8350, 0xc4),
    (8400, 0xc5),
    (8450, 0xc6),
    (8500, 0xc7),
    (8550, 0xc9),
    (8600, 0xca),
    (8650, 0xcb),
    (8700, 0xcc),
    (8750, 0xcd),
    (8800, 0xcf),
    (8850, 0xd0),
    (8900, 0xd1),
    (8950, 0xd2),
    (9000, 0xd3),
    (9050, 0xd4),
    (9100, 0xd6),
    (9150, 0xd7),
    (9200, 0xd8),
    (9250, 0xd9),
    (9300, 0xda),
    (9350, 0xdb),
    (9400, 0xdd),
    (9450, 0xde),
    (9500, 0xdf),
    (9550, 0xe0),
    (9600, 0xe1),
    (9650, 0xe3),
    (9700, 0xe4),
    (9750, 0xe5),
    (9800, 0xe6),
    (9850, 0xe7),
    (9900, 0xe8),
    (9950, 0xea),
    (10000, 0xeb),
    (10100, 0x76),
    (10200, 0x77),
    (10300, 0x79),
    (10400, 0x7a),
    (10500, 0x7b),
    (10600, 0x7c),
    (10700, 0x7d),
    (10800, 0x7f),
    (10900, 0x80),
    (11000, 0x81),
    (11100, 0x82),
    (11200, 0x83),
    (11300, 0x84),
    (11400, 0x86),
    (11500, 0x87),
    (11600, 0x88),
    (11700, 0x89),
    (11800, 0x8a),
    (11900, 0x8b),
    (12000, 0x8d),
    (12100, 0x8e),
    (12200, 0x8f),
    (12300, 0x90),
    (12400, 0x91),
    (12500, 0x93),
    (12600, 0x94),
    (12700, 0x95),
    (12800, 0x96),
    (12900, 0x97),
    (13000, 0x98),
    (13100, 0x9a),
    (13200, 0x9b),
    (13300, 0x9c),
    (13400, 0x9d),
    (13500, 0x9e),
    (13600, 0x9f),
    (13700, 0xa1),
    (13800, 0xa2),
    (13900, 0xa3),
    (14000, 0xa4),
    (14100, 0xa5),
    (14200, 0xa7),
    (14300, 0xa8),
    (14400, 0xa9),
    (14500, 0xaa),
    (14600, 0xab),
    (14700, 0xac),
    (14800, 0xae),
    (14900, 0xaf),
    (15000, 0xb0),
    (15100, 0xb1),
    (15200, 0xb2),
    (15300, 0xb3),
    (15400, 0xb5),
    (15500, 0xb6),
    (15600, 0xb7),
    (15700, 0xb8),
    (15800, 0xb9),
    (15900, 0xbb),
    (16000, 0xbc),
    (16100, 0xbd),
    (16200, 0xbe),
    (16300, 0xbf),
    (16400, 0xc0),
    (16500, 0xc2),
    (16600, 0xc3),
    (16700, 0xc4),
    (16800, 0xc5),
    (16900, 0xc6),
    (17000, 0xc7),
    (17100, 0xc9),
    (17200, 0xca),
    (17300, 0xcb),
    (17400, 0xcc),
    (17500, 0xcd),
    (17600, 0xcf),
    (17700, 0xd0),
    (17800, 0xd1),
    (17900, 0xd2),
    (18000, 0xd3),
    (18100, 0xd4),
    (18200, 0xd6),
    (18300, 0xd7),
    (18400, 0xd8),
    (18500, 0xd9),
    (18600, 0xda),
    (18700, 0xdb),
    (18800, 0xdd),
    (18900, 0xde),
    (19000, 0xdf),
    (19100, 0xe0),
    (19200, 0xe1),
    (19300, 0xe3),
    (19400, 0xe4),
    (19500, 0xe5),
    (19600, 0xe6),
    (19700, 0xe7),
    (19800, 0xe8),
    (19900, 0xea),
    (20000, 0xeb),
    (20100, 0x76),
    (20200, 0x77),
    (20300, 0x79),
    (20400, 0x7a),
    (20500, 0x7b),
    (20600, 0x7c),
    (20700, 0x7d),
    (20800, 0x7f),
    (20900, 0x80),
    (21000, 0x81),
    (21100, 0x82),
    (21200, 0x83),
    (21300, 0x84),
    (21400, 0x86),
    (21500, 0x87),
    (21600, 0x88),
    (21700, 0x89),
    (21800, 0x8a),
    (21900, 0x8b),
    (22000, 0x8d),
];

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
    fn polling_control_transfer_parameters_match_reference() {
        // Reference PollingRateBuilder: bRequest 0x09, wValue 0x0306, wIndex 2.
        assert_eq!(SET_REPORT_REQUEST, 0x09);
        assert_eq!(POLLING_WVALUE, 0x0306);
        assert_eq!(CONTROL_INTERFACE, 2);
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
    fn dpi_encoding_matches_reference_map() {
        assert_eq!(encode_dpi(800), Some(0x12));
        assert_eq!(encode_dpi(1600), Some(0x25));
        assert_eq!(encode_dpi(5000), Some(0x75));
        assert_eq!(encode_dpi(22000), Some(0x8d));
        // Rounds up to the next supported step, clamps below the minimum.
        assert_eq!(encode_dpi(30), Some(0x01)); // -> 50
        assert_eq!(encode_dpi(801), Some(0x13)); // -> 850
        assert_eq!(encode_dpi(22001), None);
    }

    #[test]
    fn dpi_packet_matches_reference_default_vector() {
        // Reference DpiBuilder default (stages 800/1600/2400/3200/5000/22000,
        // active stage 2, angle-snap off, rippler on) after build(): identical
        // to the DpiBuilder.test.ts golden buffer, but with the real computed
        // checksum (0x0f74) in place of the pre-build placeholder (0x0f68).
        let packet = dpi_packet([800, 1600, 2400, 3200, 5000, 22000], 2, false, true).unwrap();
        assert_eq!(
            hex(&packet),
            "04380100013f20201225384b758d0000000000000001000002ff000000ff000000\
             ffffff0000ffffff00ffff4000ffffff020f7400000000"
        );
    }

    #[test]
    fn dpi_packet_sets_masks_flags_and_rejects_bad_input() {
        // A stage above 12000 sets its stage-mask bit; a stage in the upper
        // register window sets its high-stage flag.
        let packet = dpi_packet([1600, 1600, 1600, 1600, 1600, 16000], 1, false, true).unwrap();
        assert_eq!(packet[6], 0x20); // stage-mask bit 5 for the >12000 stage
        assert_eq!(packet[7], 0x20);
        assert_eq!(packet[16 + 5], 0x00); // 16000 is not in an upper-page window
        let paged = dpi_packet([11000, 1600, 1600, 1600, 1600, 1600], 1, false, true).unwrap();
        assert_eq!(paged[16], 0x01); // 11000 is in [10100,12000]
        assert_eq!(paged[6], 0x00); // but not >12000, so no stage-mask bit

        assert!(dpi_packet([800, 800, 800, 800, 800, 800], 0, false, true).is_none());
        assert!(dpi_packet([800, 800, 800, 800, 800, 800], 7, false, true).is_none());
        assert!(dpi_packet([800, 800, 800, 800, 800, 30000], 1, false, true).is_none());
    }

    #[test]
    fn dpi_control_transfer_wvalue_matches_reference() {
        assert_eq!(DPI_WVALUE, 0x0304);
    }

    #[test]
    fn dpi_button_notification_reports_active_stage() {
        // Captured from hardware: cycling the DPI button pushes 03 55 10 <stage> 00.
        assert_eq!(parse_dpi_stage(&[0x03, 0x55, 0x10, 0x01, 0x00]), Some(1));
        assert_eq!(parse_dpi_stage(&[0x03, 0x55, 0x10, 0x06, 0x00]), Some(6));
        // Battery packets and out-of-range stages are not DPI notifications.
        assert_eq!(parse_dpi_stage(&[0x03, 0x55, 0x40, 0x01, 0x64]), None);
        assert_eq!(parse_dpi_stage(&[0x03, 0x55, 0x10, 0x07, 0x00]), None);
        assert_eq!(parse_dpi_stage(&[0x03, 0x55, 0x10]), None);
        // ...and battery decoding still ignores a DPI packet.
        assert_eq!(parse_battery(&[0x03, 0x55, 0x10, 0x03, 0x00]), None);
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
