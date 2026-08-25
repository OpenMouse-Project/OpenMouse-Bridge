//! A minimal HID report-descriptor parser, just large enough to rebuild the
//! WebHID `collections` tree.
//!
//! Every `@openmouse/protocol` driver decides whether it owns a device in a
//! static `isSupported(device)` that reads `device.collections` — not only
//! the top-level usage page/usage, but the report ids declared inside it
//! (Pulsar matches "one input and one output report, both id 0x08"; WLMouse
//! walks `children` for a feature report id). A bridge that reports no
//! collections therefore fails every one of those checks, which is why both
//! earlier adapters in this project (`native-hid/src/hid-device-adapter.mjs`
//! and Desktop's `TauriHidDevice`) had to bypass the driver registry and
//! hand-maintain a brand table instead. Parsing the descriptor here lets the
//! web app's own registry auto-detect through Bridge exactly as it does over
//! WebHID, with no second list of devices to keep in sync.
//!
//! Only the items that shape `collections` are interpreted: usage page,
//! usage, report id, report size, report count, push/pop, collection,
//! end collection, and the three main data items. Logical/physical ranges,
//! units, and string/designator indices are skipped — nothing reads them.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportItem {
    pub report_size: u32,
    pub report_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportInfo {
    pub report_id: u8,
    pub items: Vec<ReportItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionInfo {
    pub usage_page: u16,
    pub usage: u16,
    pub input_reports: Vec<ReportInfo>,
    pub output_reports: Vec<ReportInfo>,
    pub feature_reports: Vec<ReportInfo>,
    pub children: Vec<CollectionInfo>,
}

impl CollectionInfo {
    /// Whether a page may never touch this collection.
    ///
    /// These are the usages Chrome withholds from WebHID: the ones the
    /// operating system itself reads for cursor motion and keystrokes, and
    /// authenticators. Matching that list is both a parity and a safety
    /// property — opening one of these natively also freezes the device's own
    /// input on macOS (confirmed on real hardware, see Desktop's
    /// `src-tauri/src/hid.rs`).
    ///
    /// This is not a nicety for mice specifically. A Keychron M6 on Bluetooth
    /// exposes nothing but these collections, so without the check it would be
    /// offered to the page as a device no driver can drive.
    pub fn protected(&self) -> bool {
        match (self.usage_page, self.usage) {
            // Generic Desktop: pointer, mouse, keyboard, keypad.
            (0x01, 0x01 | 0x02 | 0x06 | 0x07) => true,
            // Keyboard/Keypad and FIDO pages, whatever the usage.
            (0x07 | 0xF1D0, _) => true,
            // Consumer Control: media and system keys.
            (0x0C, 0x01) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Globals {
    usage_page: u16,
    report_id: u8,
    report_size: u32,
    report_count: u32,
}

/// Parses a raw report descriptor into its top-level collections.
///
/// Malformed input is truncated rather than rejected: a descriptor that ends
/// mid-item, or that never closes a collection, still yields everything
/// parsed up to that point. A device with a slightly wrong descriptor is
/// common enough in this hardware class that refusing to list it would be
/// worse than describing the part that made sense.
pub fn parse(bytes: &[u8]) -> Vec<CollectionInfo> {
    let mut top: Vec<CollectionInfo> = Vec::new();
    let mut open: Vec<CollectionInfo> = Vec::new();
    let mut globals = Globals::default();
    let mut saved: Vec<Globals> = Vec::new();
    let mut usages: Vec<u32> = Vec::new();
    let mut index = 0usize;

    while index < bytes.len() {
        let prefix = bytes[index];
        index += 1;

        // Long items carry their own size byte and are never used by the
        // devices here; skip the whole item.
        if prefix == 0xFE {
            let size = bytes.get(index).copied().unwrap_or(0) as usize;
            index = index.saturating_add(2).saturating_add(size);
            continue;
        }

        let size = match prefix & 0x03 {
            3 => 4,
            other => other as usize,
        };
        let Some(data) = bytes.get(index..index + size) else {
            break;
        };
        index += size;
        let value = data
            .iter()
            .enumerate()
            .fold(0u32, |accumulator, (offset, byte)| {
                accumulator | (u32::from(*byte) << (8 * offset))
            });

        match prefix & 0xFC {
            // Main: Collection
            0xA0 => {
                let (usage_page, usage) =
                    resolve_usage(globals.usage_page, usages.first().copied());
                open.push(CollectionInfo {
                    usage_page,
                    usage,
                    ..CollectionInfo::default()
                });
                usages.clear();
            }
            // Main: End Collection
            0xC0 => {
                close(&mut open, &mut top);
                usages.clear();
            }
            // Main: Input / Output / Feature
            0x80 | 0x90 | 0xB0 => {
                if let Some(current) = open.last_mut() {
                    let reports = match prefix & 0xFC {
                        0x80 => &mut current.input_reports,
                        0x90 => &mut current.output_reports,
                        _ => &mut current.feature_reports,
                    };
                    let item = ReportItem {
                        report_size: globals.report_size,
                        report_count: globals.report_count,
                    };
                    match reports
                        .iter_mut()
                        .find(|report| report.report_id == globals.report_id)
                    {
                        Some(report) => report.items.push(item),
                        None => reports.push(ReportInfo {
                            report_id: globals.report_id,
                            items: vec![item],
                        }),
                    }
                }
                usages.clear();
            }
            0x04 => globals.usage_page = value as u16,
            0x84 => globals.report_id = value as u8,
            0x74 => globals.report_size = value,
            0x94 => globals.report_count = value,
            0xA4 => saved.push(globals),
            0xB4 => {
                if let Some(previous) = saved.pop() {
                    globals = previous;
                }
            }
            // Local: Usage. A four-byte usage carries its own page in the
            // high half and does not disturb the global page.
            0x08 => usages.push(if size == 4 { value } else { value & 0xFFFF }),
            _ => {}
        }
    }

    // An unterminated collection still describes a real device.
    while !open.is_empty() {
        close(&mut open, &mut top);
    }
    top
}

fn close(open: &mut Vec<CollectionInfo>, top: &mut Vec<CollectionInfo>) {
    let Some(finished) = open.pop() else { return };
    match open.last_mut() {
        Some(parent) => parent.children.push(finished),
        None => top.push(finished),
    }
}

fn resolve_usage(global_page: u16, usage: Option<u32>) -> (u16, u16) {
    match usage {
        Some(value) if value > 0xFFFF => ((value >> 16) as u16, value as u16),
        Some(value) => (global_page, value as u16),
        None => (global_page, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape every vendor control interface in this project has, and the
    /// exact thing `PulsarHidClient.isSupported()` looks for: one input and
    /// one output report sharing a report id, on a vendor usage page.
    #[test]
    fn parses_a_vendor_control_collection() {
        let descriptor = [
            0x06, 0x00, 0xFF, // Usage Page (0xFF00)
            0x09, 0x01, //       Usage (0x01)
            0xA1, 0x01, //       Collection (Application)
            0x85, 0x08, //         Report ID (8)
            0x09, 0x02, //         Usage (0x02)
            0x75, 0x08, //         Report Size (8)
            0x95, 0x3F, //         Report Count (63)
            0x81, 0x00, //         Input
            0x09, 0x03, //         Usage (0x03)
            0x91, 0x00, //         Output
            0xC0, //             End Collection
        ];

        let collections = parse(&descriptor);

        assert_eq!(collections.len(), 1);
        let collection = &collections[0];
        assert_eq!(collection.usage_page, 0xFF00);
        assert_eq!(collection.usage, 0x01);
        assert_eq!(
            collection.input_reports,
            vec![ReportInfo {
                report_id: 8,
                items: vec![ReportItem {
                    report_size: 8,
                    report_count: 63
                }],
            }]
        );
        assert_eq!(collection.output_reports.len(), 1);
        assert_eq!(collection.output_reports[0].report_id, 8);
        assert!(collection.feature_reports.is_empty());
        assert!(!collection.protected());
    }

    #[test]
    fn protects_exactly_the_usages_a_browser_withholds() {
        let collection = |usage_page: u16, usage: u16| CollectionInfo {
            usage_page,
            usage,
            ..CollectionInfo::default()
        };

        for (page, usage) in [
            (0x01, 0x01),
            (0x01, 0x02),
            (0x01, 0x06),
            (0x01, 0x07),
            (0x07, 0x00),
            (0x0C, 0x01),
            (0xF1D0, 0x01),
        ] {
            assert!(
                collection(page, usage).protected(),
                "0x{page:04x}:0x{usage:02x} must be protected"
            );
        }
        for (page, usage) in [
            (0x01, 0x04),
            (0x0C, 0x02),
            (0xFF00, 0x01),
            (0xFFC1, 0x01),
            (0xFF60, 0x61),
        ] {
            assert!(
                !collection(page, usage).protected(),
                "0x{page:04x}:0x{usage:02x} must stay reachable"
            );
        }
    }

    /// A boot mouse: nested physical collection, no report id, and the
    /// protected usage the bridge must never open.
    #[test]
    fn nests_children_and_flags_the_protected_mouse_collection() {
        let descriptor = [
            0x05, 0x01, // Usage Page (Generic Desktop)
            0x09, 0x02, // Usage (Mouse)
            0xA1, 0x01, // Collection (Application)
            0x09, 0x01, //   Usage (Pointer)
            0xA1, 0x00, //   Collection (Physical)
            0x75, 0x08, //     Report Size (8)
            0x95, 0x03, //     Report Count (3)
            0x81, 0x06, //     Input
            0xC0, //         End Collection
            0xC0, //       End Collection
        ];

        let collections = parse(&descriptor);

        assert_eq!(collections.len(), 1);
        assert!(collections[0].protected());
        assert!(collections[0].input_reports.is_empty());
        let child = &collections[0].children[0];
        assert_eq!((child.usage_page, child.usage), (0x01, 0x01));
        assert_eq!(child.input_reports[0].report_id, 0);
        // Pointer is protected too: a Keychron M6 on Bluetooth exposes only
        // collections like these, and none of them may reach a page.
        assert!(child.protected());
    }

    /// Feature reports on a child collection, which is where WLMouse keeps
    /// its config channel, plus a global push/pop around them.
    #[test]
    fn parses_feature_reports_under_a_push_pop_pair() {
        let descriptor = [
            0x06, 0xC0, 0xFF, // Usage Page (0xFFC0)
            0x09, 0x01, //       Usage (0x01)
            0xA1, 0x01, //       Collection (Application)
            0x09, 0x02, //         Usage (0x02)
            0xA1, 0x02, //         Collection (Logical)
            0xA4, //                 Push
            0x85, 0x05, //           Report ID (5)
            0x75, 0x08, //           Report Size (8)
            0x95, 0x20, //           Report Count (32)
            0xB1, 0x02, //           Feature
            0xB4, //                 Pop
            0x85, 0x06, //           Report ID (6)
            0xB1, 0x02, //           Feature
            0xC0, //               End Collection
            0xC0, //             End Collection
        ];

        let collections = parse(&descriptor);
        let child = &collections[0].children[0];

        assert_eq!(child.feature_reports.len(), 2);
        assert_eq!(child.feature_reports[0].report_id, 5);
        // Pop restored the report id/size/count that were live before Push,
        // so the second feature report is a fresh id with zeroed sizing.
        assert_eq!(child.feature_reports[1].report_id, 6);
        assert_eq!(child.feature_reports[1].items[0].report_size, 0);
    }

    /// A descriptor that ends mid-item still describes what came before it.
    #[test]
    fn truncated_input_is_kept_not_discarded() {
        let descriptor = [
            0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x08, 0x81, 0x00, 0x06, 0x00,
        ];

        let collections = parse(&descriptor);

        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].input_reports[0].report_id, 8);
    }
}
