//! Registry of native HID drivers Bridge can push a saved profile's DPI and
//! polling rate to directly, without a browser tab open holding a WebHID
//! connection.
//!
//! Each driver is a Rust port of the matching hardware-verified WebHID
//! driver in the sibling `mouse-protocol` repo (`@openmouse/protocol`),
//! scoped to DPI/polling-rate only — the only settings Bridge profiles
//! store. Dispatch matches `ApplicationProfile.device.id`'s `"<brand>:"`
//! prefix, the same convention the OpenMouse web app writes when it saves a
//! profile (see `openmouse/src/app/InterfaceSettings.tsx`); brand strings
//! come from `mouse-protocol`'s `MouseStatus.brand` union
//! (`src/drivers/mouse-types.ts`). First match wins, same as
//! `mouse-protocol`'s own `src/drivers/registry.ts`.

use std::sync::{Mutex, MutexGuard};

use anyhow::Result;

use crate::config::ApplicationProfile;

pub(crate) mod automation;
pub mod delux;
mod native_hid;
pub mod pulsar;

pub use native_hid::{NativeBattery, NativeStatus, spawn_button_listener};

/// A mouse Bridge discovered by itself, by scanning the HID bus for vendor
/// ids its drivers recognize — no saved profile required.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedMouse {
    /// Stable profile-style id, `"<Brand>:<serial or vid:pid>"` — the same
    /// `"<brand>:"` prefix convention the OpenMouse web app writes.
    pub device_id: String,
    /// Human-readable product name from the HID descriptor.
    pub name: String,
}

/// Brand keys and vendor ids, mirroring the helper's `native-hid/src/brands.mjs`
/// (the same drivers the web panel drives, and the same normalization
/// `apply.mjs` gives `device.id` prefixes: trim + lowercase). When several
/// brands share a vendor id, the entry names the brand whose driver class
/// list covers the most sibling hardware — Delux's list includes the Attack
/// Shark and Lamzu clients — or `mouse-protocol`'s registry.ts first match;
/// the loser's devices merely fail their silent battery probe later.
const KNOWN_BRANDS: &[(&str, &[u16])] = &[
    ("Delux", &[0x1d57, 0x248a, 0x373e]),
    ("Logitech", &[0x046d]),
    ("Razer", &[0x1532]),
    ("Zaunkoenig", &[0x0483]),
    ("Finalmouse", &[0x361d]),
    ("Endgame Gear", &[0x3367]),
    ("Teevolution", &[0x3554]),
    ("WLMouse", &[0x36a7]),
    ("moddoMOUSE", &[0x2fe3]),
    ("Ninjutso", &[0x093a, 0x1915]),
    ("ATK", &[0x373b]),
    ("Attack Shark", &[0x25a7]),
    ("Keychron", &[0x3434]),
    ("Fantech", &[0x3151]),
    ("Wooting", &[0x31e3]),
    ("Microsoft", &[0x045e]),
    ("WALLHACK", &[0x3879, 0x1caa]),
];

/// Logitech is special: every Logitech-branded device (keyboards, headsets)
/// shares vendor id 0x046d and the HID++ transport (`mouse-protocol`'s
/// `drivers/logitech/hidpp.ts` has the same limitation — "whether it is a
/// *mouse* cannot be told from descriptors"), so discovery keeps the HID++
/// collections — short/long reports on usage page 0xFF00, Bluetooth on
/// 0xFF43 — and a keyboard merely fails its battery probe silently later.
const LOGITECH_VENDOR_ID: u16 = 0x046d;
const HIDPP_USAGE_PAGE: u16 = 0xff00;
const HIDPP_BLUETOOTH_USAGE_PAGE: u16 = 0xff43;

/// The brand a discovered HID interface belongs to, or `None` when the
/// interface should be skipped (other vendors' hardware, Logitech non-HID++
/// interfaces, Noir's undriven receiver PID).
fn discovered_brand(info: &hidapi::DeviceInfo) -> Option<&'static str> {
    let vendor_id = info.vendor_id();
    if vendor_id == pulsar::VENDOR_ID {
        return Some("Pulsar");
    }
    if vendor_id == LOGITECH_VENDOR_ID {
        let usage_page = info.usage_page();
        let hidpp_transport = (usage_page == HIDPP_USAGE_PAGE
            && matches!(info.usage(), 0x01 | 0x02))
            || usage_page == HIDPP_BLUETOOTH_USAGE_PAGE;
        return hidpp_transport.then_some("Logitech");
    }
    if vendor_id == 0x3554 {
        // Teevolution and VGN share this vendor page; their product ids are
        // disjoint (pulsar-hid.ts's CLAIMED_VGN_PRODUCT_IDS). Noir Gear's
        // 0xf500 rides it too but has no native driver.
        return match info.product_id() {
            0xfb56 | 0xfb57 => Some("VGN"),
            0xf500 => None,
            _ => Some("Teevolution"),
        };
    }
    KNOWN_BRANDS
        .iter()
        .find(|(_, ids)| ids.contains(&vendor_id))
        .map(|(brand, _)| *brand)
}

/// Every HID interface on the bus whose vendor id belongs to a brand with a
/// Bridge driver, deduplicated to one entry per physical mouse (several
/// interfaces share the same vendor/product/serial).
///
/// Deliberately does not open any device — enumeration only — so it is safe
/// to call while a web panel session or another driver operation runs.
pub fn detect_mice() -> Vec<DetectedMouse> {
    let api = match hidapi::HidApi::new() {
        Ok(api) => api,
        Err(error) => {
            tracing::warn!(%error, "could not enumerate HID devices");
            return Vec::new();
        }
    };
    let mut mice: Vec<DetectedMouse> = Vec::new();
    let mut seen: Vec<(u16, u16, String)> = Vec::new();
    for info in api.device_list() {
        let vendor_id = info.vendor_id();
        let Some(brand) = discovered_brand(info) else {
            continue;
        };
        // One physical mouse exposes several HID interfaces (boot mouse,
        // consumer control, vendor config, ...). Dedup on the identity
        // triple so the same mouse is not listed three times.
        let tail = identity_tail(&(
            vendor_id,
            info.product_id(),
            info.serial_number().unwrap_or_default(),
        ));
        let identity = (vendor_id, info.product_id(), tail);
        if seen.contains(&identity) {
            continue;
        }
        seen.push(identity.clone());
        mice.push(DetectedMouse {
            device_id: format!("{brand}:{}", identity.2),
            name: product_name(info.product_string().unwrap_or_default(), brand),
        });
    }
    mice
}

/// The stable per-mouse tail of a discovered device id: the serial number
/// when the device reports one, else `vid:pid`.
fn identity_tail(identity: &(u16, u16, &str)) -> String {
    if identity.2.is_empty() {
        format!("{:04x}:{:04x}", identity.0, identity.1)
    } else {
        identity.2.to_owned()
    }
}

/// The HID product string, or the brand name when the device reports none.
fn product_name(product_string: &str, brand: &str) -> String {
    let trimmed = product_string.trim();
    if trimmed.is_empty() {
        format!("{brand} Mouse")
    } else {
        trimmed.to_owned()
    }
}

/// Serializes Bridge's own device access. A profile switch and a battery read
/// can start at the same moment (both run at startup), and on macOS the second
/// open of a receiver fails with "exclusive access"; elsewhere the two would
/// interleave requests on the same device.
static DEVICE_ACCESS: Mutex<automation::ManualIntent> = Mutex::new(automation::ManualIntent::new());

pub(crate) fn device_access() -> MutexGuard<'static, automation::ManualIntent> {
    DEVICE_ACCESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn manual_report_access(
    vendor_id: u16,
) -> Result<MutexGuard<'static, automation::ManualIntent>> {
    let mut access = device_access();
    if vendor_id == 0x0483 {
        access.invalidate("Zaunkoenig", true, true)?;
    }
    Ok(access)
}

fn ensure_native_ownership() -> Result<()> {
    if crate::hid::client_session_active() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "native HID is owned by an active WebSocket session",
        )
        .into());
    }
    Ok(())
}

/// Pushes a profile's DPI/polling rate to the mouse over native HID, if
/// Bridge has a driver for that device.
///
/// Returns `Ok(true)` when a driver claimed the device and applied the
/// settings, `Ok(false)` when no driver recognizes this device (most
/// devices are still driven by the OpenMouse web app over WebHID instead),
/// and `Err` when a driver claimed the device but applying failed (e.g. the
/// mouse is asleep or disconnected).
///
/// Brands with a dependency-free native Rust driver (currently Pulsar and Delux)
/// are handled directly; everything else falls back to the bundled Node.js
/// helper, which reuses `@openmouse/protocol`'s own hardware-verified
/// driver classes rather than Bridge reimplementing each vendor's protocol
/// (see `native_hid` and `native-hid/README.md`).
pub fn apply_profile(profile: &ApplicationProfile) -> Result<bool> {
    let brand = profile
        .device
        .id
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut access = device_access();
    ensure_native_ownership()?;
    access.invalidate(
        &brand,
        profile.settings.dpi.is_some(),
        profile.settings.polling_rate_hz.is_some(),
    )?;
    match brand.as_str() {
        pulsar::BRAND => {
            pulsar::apply(profile)?;
            Ok(true)
        }
        delux::BRAND => {
            if delux::apply(profile)? {
                Ok(true)
            } else {
                native_hid::apply(profile)
            }
        }
        _ => native_hid::apply(profile),
    }
}

pub(crate) fn reconcile_automatic(
    automation: &mut automation::NativeAutomation,
    desired: Option<&ApplicationProfile>,
) -> Result<()> {
    let access = device_access();
    automation.observe_manual_intent(&access);
    ensure_native_ownership()?;
    automation.reconcile(
        desired,
        &mut automation::HelperIo(|request: &serde_json::Value| native_hid::run_helper(request)),
    )
}

/// Reads the onboard-memory state of the mouse `device_id` names
/// (`"<brand>:<name>"`) over native HID — battery, and the current DPI /
/// polling rate / switch mode wherever the driver can read them.
/// `Ok(None)` means Bridge has no reader for the brand or the mouse reports
/// no battery; `Err` means a driver exists but the mouse did not answer
/// (e.g. asleep or disconnected).
pub fn read_status(device_id: &str) -> Result<Option<NativeStatus>> {
    let brand = device_id.split(':').next().unwrap_or_default();
    let _access = device_access();
    ensure_native_ownership()?;
    if brand.eq_ignore_ascii_case(pulsar::BRAND) {
        return pulsar::read_status();
    }
    native_hid::read_status(brand)
}

/// local Bridge. Native-only devices use this path when WebHID cannot access
/// their protected configuration interface.
pub fn apply_settings(brand: &str, dpi: Option<u32>, polling_rate_hz: Option<u32>) -> Result<bool> {
    let mut access = device_access();
    ensure_native_ownership()?;
    access.invalidate(brand, dpi.is_some(), polling_rate_hz.is_some())?;
    native_hid::apply_settings(brand, dpi, polling_rate_hz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProfileApplication, ProfileDevice, ProfileSettings};

    fn profile(device_id: &str) -> ApplicationProfile {
        ApplicationProfile {
            application: ProfileApplication {
                name: "Test Game".into(),
                executable: "test.exe".into(),
                path: "/games/test".into(),
                local_id: None,
                activation: None,
                bundle_path: None,
                bundle_identifier: None,
            },
            device: ProfileDevice {
                id: device_id.into(),
                name: "Test Mouse".into(),
            },
            settings: ProfileSettings {
                dpi: Some(800),
                polling_rate_hz: Some(1000),
                snapshot: None,
            },
            enabled: true,
        }
    }

    static TEST_NATIVE: Mutex<()> = Mutex::new(());

    struct FakeNative {
        dpi: u32,
        hz: u32,
        fail_manual: bool,
        calls: Vec<serde_json::Value>,
    }

    fn fake_native() -> (std::sync::Arc<Mutex<FakeNative>>, native_hid::TestHelper) {
        let values = std::sync::Arc::new(Mutex::new(FakeNative {
            dpi: 800,
            hz: 1000,
            fail_manual: false,
            calls: Vec::new(),
        }));
        let device = values.clone();
        (
            values,
            Box::new(move |request| {
                let mut device = device.lock().unwrap();
                device.calls.push(request.clone());
                let identity = serde_json::json!({"vendorId":1155,"productId":42082,"path":"fake-a","serialNumber":"serial-a","interface":2});
                if request["action"] == "automatic-read" {
                    return Ok(Some(serde_json::to_vec(
                        &serde_json::json!({"identity":identity,"dpi":device.dpi,"pollingRateHz":device.hz}),
                    )?));
                }
                if request["action"] == "automatic-apply" {
                    assert_eq!(request["identity"], identity);
                }
                if let Some(dpi) = request["dpi"].as_u64() {
                    device.dpi = dpi as u32;
                }
                if request["action"].is_null() && device.fail_manual {
                    anyhow::bail!("manual write failed after possible partial change");
                }
                if let Some(hz) = request["pollingRateHz"].as_u64() {
                    device.hz = hz as u32;
                }
                Ok(Some(serde_json::to_vec(
                    &serde_json::json!({"identity":identity}),
                )?))
            }),
        )
    }

    #[test]
    fn manual_settings_takeover_preserves_3200_on_automatic_exit() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = None;
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            assert_eq!(values.lock().unwrap().dpi, 1600);
            assert!(apply_settings("Zaunkoenig", Some(3200), None).unwrap());
            reconcile_automatic(&mut automation, None).unwrap();
            assert_eq!(values.lock().unwrap().dpi, 3200);
        });
    }

    #[test]
    fn manual_profile_and_partial_failure_preserve_requested_fields_and_restore_untouched_fields() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        for fail in [false, true] {
            let (values, helper) = fake_native();
            native_hid::with_test_helper(helper, || {
                let mut automation = automation::NativeAutomation::default();
                let mut game = profile("Zaunkoenig:M3K");
                game.settings.dpi = Some(1600);
                game.settings.polling_rate_hz = Some(2000);
                reconcile_automatic(&mut automation, Some(&game)).unwrap();
                values.lock().unwrap().fail_manual = fail;
                let mut manual = game.clone();
                manual.settings.dpi = Some(3200);
                manual.settings.polling_rate_hz = None;
                assert_eq!(apply_profile(&manual).is_err(), fail);
                let error = reconcile_automatic(&mut automation, Some(&game)).unwrap_err();
                assert!(error.to_string().contains("manual intent"));
                assert_eq!(values.lock().unwrap().dpi, 3200);
                reconcile_automatic(&mut automation, None).unwrap();
                let device = values.lock().unwrap();
                assert_eq!((device.dpi, device.hz), (3200, 1000));
                assert!(device.calls.last().unwrap()["dpi"].is_null());
            });
        }
    }

    #[test]
    fn new_automatic_acquisition_captures_manual_baseline_without_losing_other_original_field() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = Some(2000);
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            apply_settings("zaunkoenig", Some(3200), None).unwrap();
            let mut next = game.clone();
            next.application.name = "Next".into();
            next.settings.dpi = Some(6400);
            reconcile_automatic(&mut automation, Some(&next)).unwrap();
            assert_eq!(values.lock().unwrap().dpi, 6400);
            reconcile_automatic(&mut automation, None).unwrap();
            let device = values.lock().unwrap();
            assert_eq!((device.dpi, device.hz), (3200, 1000));
        });
    }

    #[test]
    fn browser_report_takeover_invalidates_unknown_fields_before_native_restore() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            {
                let _access = manual_report_access(0x0483).unwrap();
                values.lock().unwrap().dpi = 3200;
            }
            reconcile_automatic(&mut automation, None).unwrap();
            assert_eq!(values.lock().unwrap().dpi, 3200);
        });
    }

    #[test]
    fn helper_accepted_brand_whitespace_cannot_bypass_manual_takeover() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = None;
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            let brand = " \u{feff}ZaUnKoEnIg \t";
            apply_settings(brand, Some(3200), None).unwrap();
            assert_eq!(values.lock().unwrap().calls.last().unwrap()["brand"], brand);
            reconcile_automatic(&mut automation, None).unwrap();
            assert_eq!(values.lock().unwrap().dpi, 3200);
        });
    }

    #[test]
    fn polling_manual_intent_preserves_manual_rate_and_restores_only_dpi() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = Some(2000);
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            apply_settings("Zaunkoenig", None, Some(4000)).unwrap();
            reconcile_automatic(&mut automation, None).unwrap();
            let device = values.lock().unwrap();
            assert_eq!((device.dpi, device.hz), (800, 4000));
            assert!(device.calls.last().unwrap()["pollingRateHz"].is_null());
        });
    }

    #[test]
    fn partially_failed_manual_request_relinquishes_every_requested_field() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = Some(2000);
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            values.lock().unwrap().fail_manual = true;
            assert!(apply_settings("Zaunkoenig", Some(3200), Some(4000)).is_err());
            reconcile_automatic(&mut automation, None).unwrap();
            {
                let device = values.lock().unwrap();
                assert_eq!((device.dpi, device.hz), (3200, 2000));
                assert_eq!(device.calls.len(), 3);
            }
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            reconcile_automatic(&mut automation, None).unwrap();
            let device = values.lock().unwrap();
            assert_eq!((device.dpi, device.hz), (3200, 2000));
        });
    }

    #[test]
    fn queued_automatic_exit_waits_for_manual_intent_and_partial_completion() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        for fail in [false, true] {
            let (values, helper) = fake_native();
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = None;
            native_hid::with_test_helper(helper, || {
                reconcile_automatic(&mut automation, Some(&game))
            })
            .unwrap();
            values.lock().unwrap().fail_manual = fail;
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let manual_values = values.clone();
            let manual = std::thread::spawn(move || {
                native_hid::with_test_helper(
                    Box::new(move |request| {
                        manual_values.lock().unwrap().dpi = request["dpi"].as_u64().unwrap() as u32;
                        started_tx.send(()).unwrap();
                        release_rx
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                        if fail {
                            anyhow::bail!("manual partial failure");
                        }
                        Ok(Some(Vec::new()))
                    }),
                    || apply_settings("Zaunkoenig", Some(3200), None),
                )
            });
            started_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(DEVICE_ACCESS.try_lock().is_err());
            let (queued_tx, queued_rx) = std::sync::mpsc::channel();
            let automatic = std::thread::spawn(move || {
                native_hid::with_test_helper(
                    Box::new(|_| panic!("manual takeover leaves no automatic write to restore")),
                    || {
                        queued_tx.send(()).unwrap();
                        reconcile_automatic(&mut automation, None)
                    },
                )
            });
            queued_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            release_tx.send(()).unwrap();
            assert_eq!(manual.join().unwrap().is_err(), fail);
            automatic.join().unwrap().unwrap();
            assert_eq!(values.lock().unwrap().dpi, 3200);
        }
    }

    /// Exercises the real Rust -> Node.js round trip, so it needs Node.js on
    /// PATH and the native-hid/ checkout present next to Cargo.toml — not
    /// guaranteed in every CI environment, so it's opt-in rather than part
    /// of the default `cargo test` run. Run explicitly with
    /// `cargo test -- --ignored native_hid`.
    #[test]
    #[ignore = "requires Node.js on PATH and the native-hid/ checkout"]
    fn native_hid_helper_reports_no_driver_for_an_unknown_brand() {
        let outcome = apply_profile(&profile("NotARealBrand:Some Mouse"));
        assert!(!outcome.unwrap());
    }

    #[test]
    #[ignore = "requires Node.js on PATH and the native-hid/ checkout"]
    fn native_hid_helper_errors_when_no_matching_device_is_connected() {
        let outcome = apply_profile(&profile("Razer:Viper V3 Pro"));
        assert!(
            outcome.is_err(),
            "expected an error with no Razer mouse connected, got {outcome:?}"
        );
    }

    /// Discovery must see every vendor id the helper can drive, and every
    /// brand discovery writes into `device.id` must normalize to a helper
    /// brand key — otherwise a mouse is discovered but its battery or
    /// settings can never be read. Parses `native-hid/src/brands.mjs` (the
    /// helper's own table) so the two cannot drift apart silently.
    #[test]
    fn discovery_table_matches_the_native_hid_helper_brand_table() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/native-hid/src/brands.mjs"
        ))
        .expect("the native-hid checkout ships next to Cargo.toml");
        let mut after_table = false;
        let mut helper_keys = Vec::new();
        let mut helper_vendor_ids = Vec::new();
        for line in source.lines() {
            if line.starts_with("export const BRAND_DRIVERS") {
                after_table = true;
                continue;
            }
            if !after_table {
                continue;
            }
            if let Some(start) = line.find("vendorIds: [") {
                let rest = &line[start + "vendorIds: [".len()..];
                let end = rest.find(']').expect("vendorIds lists close on their line");
                for value in rest[..end].split(',') {
                    let value = value.trim();
                    if value.is_empty() {
                        continue;
                    }
                    helper_vendor_ids.push(
                        u16::from_str_radix(value.trim_start_matches("0x"), 16)
                            .expect("brands.mjs vendor ids are hex"),
                    );
                }
            } else if let Some(key) = line.strip_suffix(": {") {
                helper_keys.push(key.trim().trim_matches('"').to_owned());
            }
        }
        assert!(
            !helper_vendor_ids.is_empty(),
            "parsed no vendor ids — did brands.mjs's format change?"
        );
        let discoverable = |id: u16| {
            // The special-cased vendor ids in `discovered_brand`: the shared
            // Teevolution/VGN page, Logitech's HID++ transport, and Pulsar's
            // own Rust-driver vendor id.
            [pulsar::VENDOR_ID, 0x3554, LOGITECH_VENDOR_ID].contains(&id)
                || delux::SUPPORTED_VENDOR_IDS.contains(&id)
                || KNOWN_BRANDS.iter().any(|(_, ids)| ids.contains(&id))
        };
        for id in helper_vendor_ids {
            assert!(
                discoverable(id),
                "vendor id 0x{id:04x} is in brands.mjs but discovery cannot see it"
            );
        }
        for (brand, _) in KNOWN_BRANDS {
            let key = brand.trim().to_lowercase();
            assert!(
                helper_keys.contains(&key),
                "discovery brand {brand:?} has no {key:?} key in brands.mjs"
            );
        }
    }

    #[test]
    #[ignore = "prints what discovery sees on live hardware; needs a supported mouse connected"]
    fn discovery_lists_the_connected_mice() {
        let mice = detect_mice();
        for mouse in &mice {
            println!("{mouse:?}");
        }
        assert!(!mice.is_empty(), "discovery found no mice");
    }
}
