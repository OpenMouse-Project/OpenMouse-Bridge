//! Which devices on the bus are mice Bridge can drive, and which brand each
//! is, decided by `@openmouse/protocol`'s own driver registry — the same
//! `supports()` checks the control panel runs in the browser — rather than a
//! vendor-id table of Bridge's own.
//!
//! Bridge describes the bus (`hid::describe_devices`: grouped devices with
//! collections parsed from their report descriptors, as WebHID would give the
//! panel) and asks the Node helper once which registry driver claims each.
//! Only devices matching the registry's `SUPPORTED_HID_FILTERS` (the filters
//! the panel requests devices with) are ever opened, so a motherboard RGB
//! controller or a touchpad sharing a mouse maker's chip vendor id is never
//! touched.
//!
//! When the helper cannot be reached (Node.js missing), every function here
//! returns `None` and the callers fall back to the vendor-id table in
//! `drivers/mod.rs`.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::hid::{DescribedDevice, HidFilter};

use super::native_hid;

/// How long a failed helper call is remembered before it is tried again, so
/// a missing Node.js does not cost a process spawn on every 5 s scan.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// The registry's device filters, fetched once.
static FILTERS: Mutex<Option<Cached<Vec<HidFilter>>>> = Mutex::new(None);

/// The brand of each described device, for the set of devices last seen.
static IDENTIFIED: Mutex<Option<Identified>> = Mutex::new(None);

enum Cached<T> {
    Ready(T),
    Failed(Instant),
}

struct Identified {
    /// The device keys and whether each was fully described, sorted: when this
    /// changes the devices are identified again.
    signature: Vec<(String, bool)>,
    outcome: Cached<HashMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FiltersReply {
    filters: Vec<HidFilter>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdentifyReply {
    devices: Vec<IdentifiedDevice>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdentifiedDevice {
    key: String,
    brand: Option<String>,
}

fn filters() -> Option<Vec<HidFilter>> {
    // Unit tests drive the helper through fakes that know nothing of the
    // registry, so they run against the vendor-id table.
    if cfg!(test) && std::env::var_os("OPENMOUSE_LIVE_REGISTRY").is_none() {
        return None;
    }
    let mut cached = FILTERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match &*cached {
        Some(Cached::Ready(filters)) => return Some(filters.clone()),
        Some(Cached::Failed(at)) if at.elapsed() < RETRY_AFTER => return None,
        _ => {}
    }
    let fetched =
        native_hid::run_helper(&serde_json::json!({ "action": "filters" })).and_then(|stdout| {
            let stdout = stdout.ok_or_else(|| anyhow::anyhow!("the helper has no registry"))?;
            Ok(serde_json::from_slice::<FiltersReply>(&stdout)?.filters)
        });
    match fetched {
        Ok(filters) => {
            *cached = Some(Cached::Ready(filters.clone()));
            Some(filters)
        }
        Err(error) => {
            tracing::warn!(%error, "Could not load the device registry; using Bridge's vendor list");
            *cached = Some(Cached::Failed(Instant::now()));
            None
        }
    }
}

/// Every supported device on the bus with the brand the registry gives it.
pub(super) fn devices() -> Option<Vec<(DescribedDevice, String)>> {
    let filters = filters()?;
    let described = crate::hid::describe_devices(&filters)
        .map_err(|error| tracing::debug!(%error, "Could not describe HID devices"))
        .ok()?;
    let brands = identify(&described)?;
    Some(
        described
            .into_iter()
            .filter_map(|device| {
                let brand = brands.get(&device.key)?.clone();
                Some((device, brand))
            })
            .collect(),
    )
}

fn identify(devices: &[DescribedDevice]) -> Option<HashMap<String, String>> {
    let mut signature: Vec<(String, bool)> = devices
        .iter()
        .map(|device| (device.key.clone(), device.described_fully()))
        .collect();
    signature.sort();
    let mut identified = IDENTIFIED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(previous) = &*identified
        && previous.signature == signature
    {
        match &previous.outcome {
            Cached::Ready(brands) => return Some(brands.clone()),
            Cached::Failed(at) if at.elapsed() < RETRY_AFTER => return None,
            Cached::Failed(_) => {}
        }
    }
    let outcome = if devices.is_empty() {
        Ok(HashMap::new())
    } else {
        native_hid::run_helper(&serde_json::json!({ "action": "identify", "devices": devices }))
            .and_then(|stdout| {
                let stdout = stdout.ok_or_else(|| anyhow::anyhow!("the helper has no registry"))?;
                Ok(brands_by_key(serde_json::from_slice(&stdout)?))
            })
    };
    let (cached, result) = match outcome {
        Ok(brands) => (Cached::Ready(brands.clone()), Some(brands)),
        Err(error) => {
            tracing::warn!(%error, "Could not identify HID devices");
            (Cached::Failed(Instant::now()), None)
        }
    };
    *identified = Some(Identified {
        signature,
        outcome: cached,
    });
    result
}

fn brands_by_key(reply: IdentifyReply) -> HashMap<String, String> {
    reply
        .devices
        .into_iter()
        .filter_map(|device| Some((device.key, device.brand?)))
        .collect()
}

/// The devices a `brand` request may use: those the registry gives that
/// brand. When none has it — a profile saved under the brand the driver
/// reports (`Attack Shark`) for hardware the registry files under another
/// (`Delux`) — every supported device is offered and the helper keeps the one
/// whose own status reports `brand`. `None` when the registry is unavailable.
pub(super) fn devices_for(brand: &str) -> Option<native_hid::Devices> {
    let devices = devices()?;
    let matching: Vec<DescribedDevice> = devices
        .iter()
        .filter(|(_, registry_brand)| registry_brand.eq_ignore_ascii_case(brand))
        .map(|(device, _)| device.clone())
        .collect();
    Some(if matching.is_empty() {
        native_hid::Devices {
            described: devices.into_iter().map(|(device, _)| device).collect(),
            match_status_brand: true,
        }
    } else {
        native_hid::Devices {
            described: matching,
            match_status_brand: false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lists the mice plugged in now and reads each, as Bridge's battery poll
    /// does: `OPENMOUSE_LIVE_REGISTRY=1 cargo test live_mice -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the real HID bus; run by hand"]
    fn live_mice() {
        assert!(
            std::env::var_os("OPENMOUSE_LIVE_REGISTRY").is_some(),
            "set OPENMOUSE_LIVE_REGISTRY so the registry is used"
        );
        for mouse in super::super::detect_mice() {
            let status = super::super::read_status(&mouse.device_id);
            println!("{} ({}): {status:?}", mouse.device_id, mouse.name);
        }
    }

    /// Writes what Bridge would send the helper for the devices plugged in
    /// now to the file `OPENMOUSE_DUMP` names, for checking the helper against
    /// real hardware:
    /// `OPENMOUSE_DUMP=devices.json cargo test dump_live_bus -- --ignored`.
    #[test]
    #[ignore = "reads the real HID bus; run by hand"]
    fn dump_live_bus() {
        let stdout = native_hid::run_helper(&serde_json::json!({ "action": "filters" }))
            .expect("the helper runs")
            .expect("the helper has a registry");
        let filters = serde_json::from_slice::<FiltersReply>(&stdout)
            .unwrap()
            .filters;
        let devices = crate::hid::describe_devices(&filters).unwrap();
        let path = std::env::var("OPENMOUSE_DUMP").expect("OPENMOUSE_DUMP names the output file");
        std::fs::write(path, serde_json::to_vec_pretty(&devices).unwrap()).unwrap();
    }

    #[test]
    fn devices_the_registry_does_not_claim_have_no_brand() {
        let reply: IdentifyReply = serde_json::from_str(
            r#"{"devices":[{"key":"hid-34340d05a-1","brand":"Keychron"},{"key":"hid-14627c92-2","brand":null}]}"#,
        )
        .unwrap();
        let brands = brands_by_key(reply);
        assert_eq!(
            brands.get("hid-34340d05a-1").map(String::as_str),
            Some("Keychron")
        );
        assert!(!brands.contains_key("hid-14627c92-2"));
    }
}
