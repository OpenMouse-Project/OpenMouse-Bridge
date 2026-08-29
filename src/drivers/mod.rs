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

use anyhow::Result;

use crate::config::ApplicationProfile;

mod native_hid;
pub mod pulsar;

/// Pushes a profile's DPI/polling rate to the mouse over native HID, if
/// Bridge has a driver for that device.
///
/// Returns `Ok(true)` when a driver claimed the device and applied the
/// settings, `Ok(false)` when no driver recognizes this device (most
/// devices are still driven by the OpenMouse web app over WebHID instead),
/// and `Err` when a driver claimed the device but applying failed (e.g. the
/// mouse is asleep or disconnected).
///
/// Brands with a dependency-free native Rust driver (currently just Pulsar)
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
    match brand.as_str() {
        pulsar::BRAND => {
            pulsar::apply(profile)?;
            Ok(true)
        }
        _ => native_hid::apply(profile),
    }
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
            },
            device: ProfileDevice {
                id: device_id.into(),
                name: "Test Mouse".into(),
            },
            settings: ProfileSettings {
                dpi: Some(800),
                polling_rate_hz: Some(1000),
            },
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
}
