//! Native battery reader for mice on the CompX page-command framing that the
//! Lamzu driver in `mouse-protocol` speaks (the Attack Shark R5 Ultra is one).
//!
//! The node-hid helper cannot read these: its feature-report request to the
//! vendor interface times out on macOS, while the same request through
//! hidapi gets an answer. This sends the one request the control panel sends
//! for the battery and nothing else.
//!
//! Request: a 64-byte feature report (id 0) with the target at byte 2, the
//! payload length at byte 3, the page at byte 4 and the command at byte 5.
//! Reply: status at byte 0 (`0xa1` ok, `0xa0`/`0xa3` pending or busy, `0xa2`
//! unsupported), the echoed page and command at bytes 4 and 5, the payload from
//! byte 6: a charging flag, then the percent.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::native_hid::{NativeBattery, NativeStatus};

const VENDOR_ID: u16 = 0x373e;
/// Products confirmed on hardware: the Attack Shark R5 Ultra, on its cable and
/// on its 8K receiver. Other Lamzu-family mice share the framing but have not
/// been tried here, so nothing is sent to them.
const PRODUCTS: &[(u16, &str)] = &[
    (0x0046, "Attack Shark R5 Ultra"),
    (0x0047, "Attack Shark R5 Ultra"),
];

const REPORT_ID: u8 = 0;
const PACKET_LEN: usize = 64;
const HEADER_LEN: usize = 6;
const TARGET_MOUSE: u8 = 0x02;
const PAGE_DEVICE: u8 = 0x00;
const COMMAND_BATTERY: u8 = 0x83;
const PAYLOAD_LEN: u8 = 0x02;

const STATUS_PENDING: u8 = 0xa0;
const STATUS_OK: u8 = 0xa1;
const STATUS_UNSUPPORTED: u8 = 0xa2;
const STATUS_BUSY: u8 = 0xa3;

const ATTEMPTS: usize = 12;
const QUICK_ATTEMPTS: usize = 3;
const QUICK_DELAY: Duration = Duration::from_millis(30);
/// Later attempts wait longer, giving a sleeping mouse time to wake.
const WAKE_DELAY: Duration = Duration::from_millis(300);

/// Brands whose discovery label can belong to one of these mice: the vendor id
/// is shared with Delux, and the same hardware is sold as Attack Shark.
pub(super) fn handles(brand: &str) -> bool {
    ["delux", "attack shark", "lamzu", "crdrako"]
        .iter()
        .any(|name| brand.eq_ignore_ascii_case(name))
}

/// One feature-report exchange with a device, so the protocol logic runs
/// against a fake in tests.
trait FeatureLink {
    fn send(&mut self, frame: &[u8]) -> Result<()>;
    /// The reply with the report id byte removed.
    fn receive(&mut self) -> Result<Vec<u8>>;
}

struct HidLink<'a> {
    device: &'a hidapi::HidDevice,
    buffer_len: usize,
}

impl FeatureLink for HidLink<'_> {
    fn send(&mut self, frame: &[u8]) -> Result<()> {
        self.device
            .send_feature_report(frame)
            .context("could not send the battery request")
    }

    fn receive(&mut self) -> Result<Vec<u8>> {
        let mut data = vec![0; self.buffer_len];
        data[0] = REPORT_ID;
        let size = self
            .device
            .get_feature_report(&mut data)
            .context("could not read the battery reply")?;
        Ok(crate::hid::report_payload(&data, size))
    }
}

fn battery_request() -> Vec<u8> {
    let mut frame = vec![0; 1 + PACKET_LEN];
    frame[0] = REPORT_ID;
    // Bytes of the 64-byte packet, after the report id byte.
    frame[1 + 2] = TARGET_MOUSE;
    frame[1 + 3] = PAYLOAD_LEN;
    frame[1 + 4] = PAGE_DEVICE;
    frame[1 + 5] = COMMAND_BATTERY;
    frame
}

/// Asks the mouse for its battery. `Ok(None)` means it answered but reports no
/// battery (a percent above 100).
fn read_battery(
    link: &mut dyn FeatureLink,
    pause: &mut dyn FnMut(Duration),
) -> Result<Option<(u8, bool)>> {
    link.send(&battery_request())?;
    for attempt in 0..ATTEMPTS {
        let reply = link.receive()?;
        if reply.first() == Some(&STATUS_UNSUPPORTED) {
            bail!("the mouse does not support the battery request");
        }
        let answered = reply.len() >= HEADER_LEN
            && reply[0] == STATUS_OK
            && reply[4] == PAGE_DEVICE
            && reply[5] == COMMAND_BATTERY;
        if answered {
            let length = usize::from(reply[3]).min(reply.len() - HEADER_LEN);
            let payload = &reply[HEADER_LEN..HEADER_LEN + length];
            let [charging, percent, ..] = payload else {
                bail!("the battery reply was too short");
            };
            return Ok((*percent <= 100).then_some((*percent, *charging == 1)));
        }
        let waiting = matches!(
            reply.first(),
            Some(&STATUS_PENDING | &STATUS_BUSY | &STATUS_OK)
        );
        if !waiting {
            bail!("the mouse answered with an unexpected status");
        }
        pause(if attempt < QUICK_ATTEMPTS {
            QUICK_DELAY
        } else {
            WAKE_DELAY
        });
    }
    Err(anyhow!(
        "the mouse did not answer; it may be asleep or out of range"
    ))
}

/// The battery of a connected R5 Ultra, or `None` when there is none (no
/// supported mouse is plugged in, or it reports no battery).
pub(super) fn read_status() -> Result<Option<NativeStatus>> {
    let products: Vec<u16> = PRODUCTS.iter().map(|(id, _)| *id).collect();
    let found = crate::hid::feature_interfaces(VENDOR_ID, &products, REPORT_ID)
        .map_err(|error| anyhow!(error))?;
    let mut last_error = None;
    for interface in &found.interfaces {
        let mut link = HidLink {
            device: &interface.device,
            buffer_len: interface.buffer_len,
        };
        match read_battery(&mut link, &mut std::thread::sleep) {
            Ok(Some((percent, charging))) => {
                let name = PRODUCTS
                    .iter()
                    .find(|(id, _)| *id == interface.product_id)
                    .map(|(_, name)| (*name).to_owned());
                return Ok(Some(NativeStatus {
                    battery: NativeBattery {
                        percent,
                        charging,
                        reported_name: name,
                    },
                    dpi: None,
                    polling_rate_hz: None,
                    switch_mode: None,
                }));
            }
            Ok(None) => return Ok(None),
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeLink {
        sent: Vec<Vec<u8>>,
        replies: VecDeque<Vec<u8>>,
    }

    impl FakeLink {
        fn new(replies: Vec<Vec<u8>>) -> Self {
            Self {
                sent: Vec::new(),
                replies: replies.into(),
            }
        }
    }

    impl FeatureLink for FakeLink {
        fn send(&mut self, frame: &[u8]) -> Result<()> {
            self.sent.push(frame.to_vec());
            Ok(())
        }

        fn receive(&mut self) -> Result<Vec<u8>> {
            // Once the script runs out the mouse keeps saying "pending".
            Ok(self
                .replies
                .pop_front()
                .unwrap_or_else(|| reply(STATUS_PENDING, 0, 0, &[])))
        }
    }

    /// A 64-byte reply: status, echoed page and command, then the payload.
    fn reply(status: u8, page: u8, command: u8, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0; PACKET_LEN];
        packet[0] = status;
        packet[3] = payload.len() as u8;
        packet[4] = page;
        packet[5] = command;
        packet[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
        packet
    }

    fn battery_reply(charging: u8, percent: u8) -> Vec<u8> {
        reply(
            STATUS_OK,
            PAGE_DEVICE,
            COMMAND_BATTERY,
            &[charging, percent],
        )
    }

    fn run(replies: Vec<Vec<u8>>) -> (Result<Option<(u8, bool)>>, FakeLink, Vec<Duration>) {
        let mut link = FakeLink::new(replies);
        let mut pauses = Vec::new();
        let result = read_battery(&mut link, &mut |duration| pauses.push(duration));
        (result, link, pauses)
    }

    #[test]
    fn the_request_is_the_one_the_control_panel_sends() {
        let (_, link, _) = run(vec![battery_reply(0, 66)]);
        assert_eq!(link.sent.len(), 1, "one request, never repeated");
        let frame = &link.sent[0];
        assert_eq!(frame.len(), 65, "report id plus 64 bytes");
        assert_eq!(frame[0], 0, "report id 0");
        // target 0x02, length 2, page 0, command 0x83 after the status and
        // reserved bytes.
        assert_eq!(&frame[1..7], &[0x00, 0x00, 0x02, 0x02, 0x00, 0x83]);
        assert!(frame[7..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn a_reply_gives_the_percent_and_the_charging_flag() {
        assert_eq!(
            run(vec![battery_reply(0, 66)]).0.unwrap(),
            Some((66, false))
        );
        assert_eq!(
            run(vec![battery_reply(1, 100)]).0.unwrap(),
            Some((100, true))
        );
    }

    #[test]
    fn pending_and_busy_replies_are_waited_out() {
        let (result, _, pauses) = run(vec![
            reply(STATUS_PENDING, 0, 0, &[]),
            reply(STATUS_BUSY, 0, 0, &[]),
            battery_reply(0, 42),
        ]);
        assert_eq!(result.unwrap(), Some((42, false)));
        assert_eq!(pauses, vec![QUICK_DELAY, QUICK_DELAY]);
    }

    #[test]
    fn a_reply_to_another_command_is_not_taken_for_the_battery() {
        // An ok answer for the firmware command (0x81) must be skipped.
        let (result, _, _) = run(vec![
            reply(STATUS_OK, PAGE_DEVICE, 0x81, &[1, 2, 3]),
            battery_reply(0, 77),
        ]);
        assert_eq!(result.unwrap(), Some((77, false)));
    }

    #[test]
    fn a_silent_mouse_is_given_time_to_wake_then_reported() {
        let (result, link, pauses) = run(Vec::new());
        assert!(result.unwrap_err().to_string().contains("asleep"));
        assert_eq!(link.sent.len(), 1, "no second request while waiting");
        assert_eq!(pauses.len(), ATTEMPTS);
        assert_eq!(pauses[..QUICK_ATTEMPTS], [QUICK_DELAY; QUICK_ATTEMPTS]);
        assert!(
            pauses[QUICK_ATTEMPTS..]
                .iter()
                .all(|pause| *pause == WAKE_DELAY)
        );
    }

    #[test]
    fn unsupported_and_unexpected_statuses_fail_at_once() {
        let (result, _, pauses) = run(vec![reply(STATUS_UNSUPPORTED, 0, 0, &[])]);
        assert!(result.unwrap_err().to_string().contains("does not support"));
        assert!(pauses.is_empty());
        let (result, _, _) = run(vec![reply(0x7f, 0, 0, &[])]);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unexpected status")
        );
    }

    #[test]
    fn a_percent_above_100_means_the_mouse_has_no_battery() {
        assert_eq!(run(vec![battery_reply(0, 0xff)]).0.unwrap(), None);
    }

    #[test]
    fn a_short_payload_is_an_error_not_a_guess() {
        let short = reply(STATUS_OK, PAGE_DEVICE, COMMAND_BATTERY, &[1]);
        assert!(run(vec![short]).0.is_err());
        // A claimed length longer than the packet cannot read past it.
        let mut long = battery_reply(0, 50);
        long[3] = 250;
        assert!(run(vec![long]).0.is_ok());
    }

    #[test]
    fn only_the_brands_that_can_label_these_mice_are_handled() {
        for brand in ["Delux", "attack shark", "Lamzu", "CRDrako"] {
            assert!(handles(brand), "{brand}");
        }
        for brand in ["Logitech", "Razer", "Pulsar", "ATK"] {
            assert!(!handles(brand), "{brand}");
        }
    }
}
