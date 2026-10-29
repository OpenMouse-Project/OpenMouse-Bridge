//! The mice Bridge has seen, kept in `devices.json` next to the config so the
//! tray can still show the last mouse after a restart or while it is unplugged.
//!
//! This is app-written state, separate from the user's settings in
//! `config.json`. A missing or damaged file is treated as an empty list; it
//! never stops Bridge from starting.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const FILE_VERSION: u32 = 1;
/// More than this and the least recently seen mouse is dropped.
const MAX_KNOWN: usize = 16;
/// A sighting that changes nothing only rewrites the file this often.
const REWRITE_AFTER_SECS: u64 = 10 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownDevice {
    pub id: String,
    pub name: String,
    /// Unix seconds of the last time the mouse was detected or read.
    pub last_seen: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charging: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpi: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polling_rate_hz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_mode: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevicesFile {
    version: u32,
    devices: Vec<KnownDevice>,
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[derive(Debug)]
pub struct DeviceStore {
    path: PathBuf,
    devices: Vec<KnownDevice>,
    /// `last_seen` as last written, so unchanged sightings do not rewrite the
    /// file on every poll.
    persisted_seen: std::collections::HashMap<String, u64>,
}

impl DeviceStore {
    /// `devices.json` beside the config file.
    pub fn path_for(config_path: &Path) -> PathBuf {
        config_path.with_file_name("devices.json")
    }

    /// Loads the file; anything missing or unreadable gives an empty store.
    pub fn load(path: PathBuf) -> Self {
        let devices = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<DevicesFile>(&bytes) {
                Ok(file) if file.version == FILE_VERSION => file.devices,
                Ok(file) => {
                    tracing::warn!(
                        version = file.version,
                        "ignoring devices.json from another version"
                    );
                    Vec::new()
                }
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "could not parse devices.json; starting with no remembered mice");
                    Vec::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "could not read devices.json");
                Vec::new()
            }
        };
        let persisted_seen = devices
            .iter()
            .map(|device| (device.id.clone(), device.last_seen))
            .collect();
        Self {
            path,
            devices,
            persisted_seen,
        }
    }

    pub fn devices(&self) -> &[KnownDevice] {
        &self.devices
    }

    /// Records a sighting. Fields the update leaves empty keep what was known
    /// before. Returns whether the file should be rewritten.
    pub fn remember(&mut self, update: KnownDevice) -> bool {
        let Some(existing) = self
            .devices
            .iter_mut()
            .find(|device| device.id == update.id)
        else {
            self.devices.push(update);
            self.trim();
            return true;
        };
        let before = existing.clone();
        existing.name = update.name;
        existing.last_seen = update.last_seen.max(existing.last_seen);
        existing.percent = update.percent.or(existing.percent);
        existing.charging = update.charging.or(existing.charging);
        existing.dpi = update.dpi.or(existing.dpi);
        existing.polling_rate_hz = update.polling_rate_hz.or(existing.polling_rate_hz);
        existing.switch_mode = update.switch_mode.or_else(|| existing.switch_mode.take());
        let mut changed = before.name != existing.name
            || before.percent != existing.percent
            || before.charging != existing.charging
            || before.dpi != existing.dpi
            || before.polling_rate_hz != existing.polling_rate_hz
            || before.switch_mode != existing.switch_mode;
        if !changed {
            let written = self.persisted_seen.get(&existing.id).copied().unwrap_or(0);
            changed = existing.last_seen.saturating_sub(written) >= REWRITE_AFTER_SECS;
        }
        changed
    }

    /// Forgets a mouse. Returns whether the file should be rewritten.
    pub fn forget(&mut self, id: &str) -> bool {
        let before = self.devices.len();
        self.devices.retain(|device| device.id != id);
        self.persisted_seen.remove(id);
        self.devices.len() != before
    }

    fn trim(&mut self) {
        while self.devices.len() > MAX_KNOWN {
            if let Some(oldest) = self
                .devices
                .iter()
                .enumerate()
                .min_by_key(|(_, device)| device.last_seen)
                .map(|(index, _)| index)
            {
                let removed = self.devices.remove(oldest);
                self.persisted_seen.remove(&removed.id);
            }
        }
    }

    pub fn save(&mut self) -> Result<()> {
        let file = DevicesFile {
            version: FILE_VERSION,
            devices: self.devices.clone(),
        };
        let json = serde_json::to_vec_pretty(&file)?;
        crate::config::write_atomically(&self.path, &json)
            .with_context(|| format!("could not save {}", self.path.display()))?;
        self.persisted_seen = self
            .devices
            .iter()
            .map(|device| (device.id.clone(), device.last_seen))
            .collect();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("bridge-devices-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory.join("devices.json")
    }

    fn device(id: &str, name: &str, seen: u64) -> KnownDevice {
        KnownDevice {
            id: id.into(),
            name: name.into(),
            last_seen: seen,
            percent: None,
            charging: None,
            dpi: None,
            polling_rate_hz: None,
            switch_mode: None,
        }
    }

    #[test]
    fn lives_beside_the_config_file() {
        assert_eq!(
            DeviceStore::path_for(Path::new("/a/b/config.json")),
            Path::new("/a/b/devices.json")
        );
    }

    #[test]
    fn remembered_mice_survive_a_restart() {
        let path = temp_path("restart");
        let mut store = DeviceStore::load(path.clone());
        let mut mouse = device("Logitech:G Pro", "G Pro", 1_000);
        mouse.percent = Some(64);
        mouse.charging = Some(true);
        mouse.dpi = Some(1600);
        assert!(store.remember(mouse.clone()));
        store.save().unwrap();

        let reloaded = DeviceStore::load(path);
        assert_eq!(reloaded.devices(), [mouse]);
    }

    #[test]
    fn missing_or_damaged_files_give_an_empty_list() {
        let path = temp_path("damaged");
        assert!(DeviceStore::load(path.clone()).devices().is_empty());
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(DeviceStore::load(path.clone()).devices().is_empty());
        std::fs::write(&path, br#"{"version":99,"devices":[]}"#).unwrap();
        assert!(DeviceStore::load(path).devices().is_empty());
    }

    #[test]
    fn a_sighting_without_readings_keeps_the_last_ones() {
        let mut store = DeviceStore::load(temp_path("merge"));
        let mut first = device("Razer:x", "Razer X", 100);
        first.percent = Some(80);
        first.dpi = Some(800);
        store.remember(first);
        store.remember(device("Razer:x", "Razer X", 200));
        let kept = &store.devices()[0];
        assert_eq!(kept.last_seen, 200);
        assert_eq!(kept.percent, Some(80));
        assert_eq!(kept.dpi, Some(800));
    }

    #[test]
    fn unchanged_sightings_rarely_rewrite_the_file() {
        let mut store = DeviceStore::load(temp_path("rewrite"));
        assert!(store.remember(device("a", "A", 1_000)));
        store.save().unwrap();
        assert!(!store.remember(device("a", "A", 1_060)), "a minute later");
        assert!(store.remember(device("a", "A", 1_000 + REWRITE_AFTER_SECS)));
        let mut changed = device("a", "A", 1_070);
        changed.percent = Some(50);
        assert!(store.remember(changed), "a new reading is always written");
    }

    #[test]
    fn forgetting_removes_the_mouse_and_reports_it() {
        let mut store = DeviceStore::load(temp_path("forget"));
        store.remember(device("a", "A", 1));
        assert!(store.forget("a"));
        assert!(!store.forget("a"));
        assert!(store.devices().is_empty());
    }

    #[test]
    fn only_the_most_recent_sixteen_are_kept() {
        let mut store = DeviceStore::load(temp_path("cap"));
        for index in 0..20u64 {
            store.remember(device(&format!("id{index}"), "M", index + 1));
        }
        assert_eq!(store.devices().len(), MAX_KNOWN);
        assert!(store.devices().iter().all(|device| device.last_seen > 4));
    }
}
