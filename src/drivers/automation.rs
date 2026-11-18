use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{ApplicationProfile, ProfileSettings};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeviceIdentity {
    vendor_id: u16,
    product_id: u16,
    path: String,
    serial_number: String,
    interface: Option<i32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Baseline {
    pub identity: DeviceIdentity,
    /// `None` when the driver cannot read it back: Bridge still applies a
    /// profile's value then, but has nothing to restore afterwards.
    pub dpi: Option<u32>,
    pub polling_rate_hz: Option<u32>,
}

pub(crate) trait AutomationIo {
    fn read(&mut self, device_id: &str) -> Result<Baseline>;
    fn write(
        &mut self,
        device_id: &str,
        baseline: &Baseline,
        settings: &ProfileSettings,
    ) -> Result<()>;
    /// What the mouse is running now, read just before a restore so a value
    /// something else changed since Bridge set it is left alone. `None` when
    /// this path does not check (Zaunkoenig's helper tracks manual intent
    /// itself).
    fn current(&mut self, _device_id: &str) -> Result<Option<Readback>> {
        Ok(None)
    }
}

struct Entry {
    baseline: Baseline,
    dpi: bool,
    polling: bool,
    /// The values Bridge last wrote for the fields it owns.
    applied: Readback,
    failed: bool,
    restore_after: Option<Instant>,
    restore_delay: Duration,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ManualIntent {
    dpi: u64,
    polling: u64,
}

impl ManualIntent {
    pub const fn new() -> Self {
        Self { dpi: 0, polling: 0 }
    }

    pub fn invalidate(&mut self, brand: &str, dpi: bool, polling: bool) -> Result<()> {
        if brand
            .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
            .eq_ignore_ascii_case("Zaunkoenig")
        {
            if dpi {
                self.dpi = self
                    .dpi
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("manual DPI ownership generation exhausted"))?;
            }
            if polling {
                self.polling = self.polling.checked_add(1).ok_or_else(|| {
                    anyhow::anyhow!("manual polling ownership generation exhausted")
                })?;
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct NativeAutomation {
    entries: HashMap<String, Entry>,
    attempted: Option<ApplicationProfile>,
    apply_deferred: bool,
    apply_error: Option<String>,
    observed_manual: ManualIntent,
}

impl NativeAutomation {
    pub fn observe_manual_intent(&mut self, intent: &ManualIntent) {
        let dpi = intent.dpi != self.observed_manual.dpi;
        let polling = intent.polling != self.observed_manual.polling;
        if dpi || polling {
            let mut superseded = false;
            self.entries.retain(|id, entry| {
                if id
                    .split_once(':')
                    .is_some_and(|(brand, _)| brand.eq_ignore_ascii_case("Zaunkoenig"))
                {
                    superseded |= (dpi && entry.dpi) || (polling && entry.polling);
                    entry.dpi &= !dpi;
                    entry.polling &= !polling;
                }
                entry.dpi || entry.polling
            });
            if superseded {
                self.apply_error = Some("native automatic settings superseded by manual intent; untouched fields remain owned".into());
            }
        }
        self.observed_manual = *intent;
    }

    pub fn pending_restores_for(&self, desired: Option<&ApplicationProfile>) -> usize {
        self.entries
            .iter()
            .filter(|(id, entry)| {
                entry.failed || desired.is_none_or(|profile| &profile.device.id != *id)
            })
            .count()
    }

    pub fn reconcile(
        &mut self,
        desired: Option<&ApplicationProfile>,
        io: &mut impl AutomationIo,
    ) -> Result<()> {
        self.reconcile_at(desired, io, Instant::now())
    }

    fn reconcile_at(
        &mut self,
        desired: Option<&ApplicationProfile>,
        io: &mut impl AutomationIo,
        now: Instant,
    ) -> Result<()> {
        let mut errors = Vec::new();
        let mut removed: Vec<_> = self
            .entries
            .keys()
            .filter(|id| {
                desired.is_none_or(|profile| &profile.device.id != *id) || self.entries[*id].failed
            })
            .cloned()
            .collect();
        removed.sort();
        for id in removed {
            let entry = self.entries.get_mut(&id).unwrap();
            entry.failed = true;
            if entry.restore_after.is_some_and(|deadline| now < deadline) {
                errors.push(format!(
                    "{id}: native restoration pending; retry delayed, original baseline retained"
                ));
                continue;
            }
            let result = (|| {
                // A DPI button, a stage switch, a manual change or another
                // mouse since Bridge applied the profile: that value stays.
                if let Some((dpi, polling_rate_hz)) = io.current(&id)? {
                    entry.dpi &= dpi.is_some() && dpi == entry.applied.0;
                    entry.polling &=
                        polling_rate_hz.is_some() && polling_rate_hz == entry.applied.1;
                }
                let settings = ProfileSettings {
                    dpi: entry.baseline.dpi.filter(|_| entry.dpi),
                    polling_rate_hz: entry.baseline.polling_rate_hz.filter(|_| entry.polling),
                    snapshot: None,
                };
                io.write(&id, &entry.baseline, &settings)
            })();
            match result {
                Ok(()) => {
                    self.entries.remove(&id);
                }
                Err(error) => {
                    entry.restore_after = Some(now + entry.restore_delay);
                    entry.restore_delay = entry
                        .restore_delay
                        .saturating_mul(2)
                        .min(Duration::from_secs(60));
                    errors.push(format!("{id}: native restoration pending: {error:#}"));
                }
            }
        }
        let needs_apply = desired != self.attempted.as_ref() || self.apply_deferred;
        let blocked = needs_apply
            && desired.is_some_and(|profile| match self.entries.get(&profile.device.id) {
                Some(entry) => entry.failed,
                None => self.entries.len() >= 64,
            });
        if blocked {
            self.apply_deferred = true;
            self.apply_error = Some("native apply deferred until restoration completes".into());
        } else if needs_apply {
            self.apply_deferred = false;
            self.attempted = desired.cloned();
            self.apply_error = None;
            if let Some(profile) = desired {
                let result = self.apply(profile, io);
                if let Err(error) = result {
                    self.apply_error = Some(format!("{}: {error:#}", profile.device.id));
                }
            }
        }
        if let Some(error) = &self.apply_error {
            errors.push(error.clone());
        }
        if !errors.is_empty() {
            anyhow::bail!("{}", errors.join("; "));
        }
        Ok(())
    }

    fn apply(&mut self, profile: &ApplicationProfile, io: &mut impl AutomationIo) -> Result<()> {
        let id = &profile.device.id;
        if self.entries.get(id).is_some_and(|entry| entry.failed) {
            anyhow::bail!("previous native restoration must finish before another write");
        }
        if profile.settings.dpi.is_none()
            && profile.settings.polling_rate_hz.is_none()
            && !self.entries.contains_key(id)
        {
            return Ok(());
        }
        if let Some(entry) = self.entries.get_mut(id) {
            let acquire_dpi = profile.settings.dpi.is_some() && !entry.dpi;
            let acquire_polling = profile.settings.polling_rate_hz.is_some() && !entry.polling;
            if acquire_dpi || acquire_polling {
                let fresh = io.read(id)?;
                if fresh.identity != entry.baseline.identity
                    || fresh.dpi == Some(0)
                    || fresh.polling_rate_hz == Some(0)
                {
                    anyhow::bail!("fresh native baseline identity/readback is missing or changed");
                }
                if acquire_dpi {
                    entry.baseline.dpi = fresh.dpi;
                }
                if acquire_polling {
                    entry.baseline.polling_rate_hz = fresh.polling_rate_hz;
                }
            }
        }
        if !self.entries.contains_key(id) {
            if self.entries.len() >= 64 {
                anyhow::bail!(
                    "native restoration journal is full; refusing another hardware change"
                );
            }
            let baseline = io.read(id)?;
            if baseline.dpi == Some(0)
                || baseline.polling_rate_hz == Some(0)
                || baseline.identity.path.is_empty()
                || baseline.identity.path.len() > 2048
                || baseline.identity.serial_number.is_empty()
                || baseline.identity.serial_number.len() > 256
            {
                anyhow::bail!(
                    "unsupported or invalid acknowledged native baseline identity/readback"
                );
            }
            if self
                .entries
                .values()
                .any(|entry| entry.baseline.identity == baseline.identity)
            {
                anyhow::bail!(
                    "resolved native device is already owned by another profile device identity"
                );
            }
            self.entries.insert(
                id.clone(),
                Entry {
                    baseline,
                    dpi: false,
                    polling: false,
                    applied: (None, None),
                    failed: false,
                    restore_after: None,
                    restore_delay: Duration::from_secs(1),
                },
            );
        }
        let entry = self.entries.get_mut(id).unwrap();
        entry.dpi |= profile.settings.dpi.is_some();
        entry.polling |= profile.settings.polling_rate_hz.is_some();
        let settings = ProfileSettings {
            dpi: profile
                .settings
                .dpi
                .or_else(|| entry.baseline.dpi.filter(|_| entry.dpi)),
            polling_rate_hz: profile
                .settings
                .polling_rate_hz
                .or_else(|| entry.baseline.polling_rate_hz.filter(|_| entry.polling)),
            snapshot: None,
        };
        // Recorded before the write, so a write that fails part-way still
        // restores whichever value did land.
        entry.applied = (settings.dpi, settings.polling_rate_hz);
        entry.failed = true;
        io.write(id, &entry.baseline, &settings)?;
        entry.failed = false;
        Ok(())
    }
}

pub(crate) struct HelperIo<F>(pub F);

impl<F: FnMut(&serde_json::Value) -> Result<Option<Vec<u8>>>> AutomationIo for HelperIo<F> {
    fn read(&mut self, device_id: &str) -> Result<Baseline> {
        let brand = supported_brand(device_id)?;
        let request =
            serde_json::json!({"brand":brand,"deviceId":device_id,"action":"automatic-read"});
        let bytes =
            (self.0)(&request)?.ok_or_else(|| anyhow::anyhow!("unsupported native readback"))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn write(
        &mut self,
        device_id: &str,
        baseline: &Baseline,
        settings: &ProfileSettings,
    ) -> Result<()> {
        let brand = supported_brand(device_id)?;
        let mut request = serde_json::json!({"brand":brand,"deviceId":device_id,"action":"automatic-apply","identity":baseline.identity});
        if let Some(dpi) = settings.dpi {
            request["dpi"] = dpi.into();
        }
        if let Some(hz) = settings.polling_rate_hz {
            request["pollingRateHz"] = hz.into();
        }
        let bytes =
            (self.0)(&request)?.ok_or_else(|| anyhow::anyhow!("unsupported native restoration"))?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            identity: DeviceIdentity,
        }
        let reply: Reply = serde_json::from_slice(&bytes)?;
        if reply.identity != baseline.identity {
            anyhow::bail!("native acknowledgement identity changed");
        }
        Ok(())
    }
}

/// A mouse's DPI and polling rate as its driver read them back; `None` for a
/// value the driver cannot read.
pub(crate) type Readback = (Option<u32>, Option<u32>);

/// Routes each mouse to its own driver. Zaunkoenig keeps the helper's
/// identity-checked automatic path; every other brand uses the same driver
/// Bridge reads its battery and applies manual changes with (`read` returns
/// `Ok(None)` and `write` `Ok(false)` when there is no driver for the brand).
pub(crate) struct DriverIo<H, R, W> {
    pub helper: HelperIo<H>,
    pub read: R,
    pub write: W,
}

impl<H, R, W> AutomationIo for DriverIo<H, R, W>
where
    H: FnMut(&serde_json::Value) -> Result<Option<Vec<u8>>>,
    R: FnMut(&str) -> Result<Option<Readback>>,
    W: FnMut(&str, Option<u32>, Option<u32>) -> Result<bool>,
{
    fn read(&mut self, device_id: &str) -> Result<Baseline> {
        if is_zaunkoenig(device_id)? {
            return self.helper.read(device_id);
        }
        let (dpi, polling_rate_hz) = (self.read)(device_id)?.ok_or_else(|| no_driver(device_id))?;
        Ok(Baseline {
            identity: DeviceIdentity::of_profile(device_id),
            dpi,
            polling_rate_hz,
        })
    }

    fn write(
        &mut self,
        device_id: &str,
        baseline: &Baseline,
        settings: &ProfileSettings,
    ) -> Result<()> {
        if is_zaunkoenig(device_id)? {
            return self.helper.write(device_id, baseline, settings);
        }
        if settings.dpi.is_none() && settings.polling_rate_hz.is_none() {
            return Ok(());
        }
        if !(self.write)(device_id, settings.dpi, settings.polling_rate_hz)? {
            return Err(no_driver(device_id));
        }
        Ok(())
    }

    fn current(&mut self, device_id: &str) -> Result<Option<Readback>> {
        if is_zaunkoenig(device_id)? {
            return Ok(None);
        }
        // Without a driver nothing can be checked, so nothing is restored.
        Ok(Some((self.read)(device_id)?.unwrap_or_default()))
    }
}

impl DeviceIdentity {
    /// These drivers find the mouse by its brand, so the profile's device id
    /// is all that identifies it.
    fn of_profile(device_id: &str) -> Self {
        Self {
            vendor_id: 0,
            product_id: 0,
            path: device_id.into(),
            serial_number: device_id.into(),
            interface: None,
        }
    }
}

fn is_zaunkoenig(device_id: &str) -> Result<bool> {
    let (brand, name) = device_id
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid native profile device identity {device_id}"))?;
    if name.is_empty() || device_id.len() > 512 {
        anyhow::bail!("invalid native profile device identity {device_id}");
    }
    Ok(brand.eq_ignore_ascii_case("Zaunkoenig"))
}

fn no_driver(device_id: &str) -> anyhow::Error {
    anyhow::anyhow!("{NO_DRIVER} ({device_id})")
}

/// What a brand Bridge has no driver for reports. It is not a failure: the
/// control panel applies that mouse's settings itself, so the panel does not
/// show it.
const NO_DRIVER: &str = "Bridge has no driver for this mouse";

/// What a pass reports for several devices, joined, minus the errors that only
/// mean Bridge has no driver for a mouse. `None` when nothing else is left, so
/// a real failure next to a no-driver one still shows.
pub(crate) fn without_no_driver(message: &str) -> Option<String> {
    let kept: Vec<&str> = message
        .split("; ")
        .filter(|error| !error.contains(NO_DRIVER))
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

fn supported_brand(device_id: &str) -> Result<&str> {
    let (brand, name) = device_id
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid native profile device identity {device_id}"))?;
    if !brand.eq_ignore_ascii_case("Zaunkoenig") {
        return Err(no_driver(device_id));
    }
    // A Zaunkoenig profile that cannot be addressed is a real problem.
    if name.is_empty() || device_id.len() > 512 {
        anyhow::bail!("invalid native profile device identity {device_id}");
    }
    Ok(brand)
}

#[cfg(test)]
mod tests;
