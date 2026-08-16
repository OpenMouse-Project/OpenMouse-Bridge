use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const DEFAULT_BATTERY_THRESHOLD: u8 = 20;
const DEFAULT_ALERT_COOLDOWN_MINUTES: u64 = 360;
const OFFICIAL_ORIGINS: &[&str] = &[
    "https://dev.openmouse.app",
    "https://openmouse.app",
    "https://www.openmouse.app",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeConfig {
    #[serde(default = "default_battery_threshold")]
    pub battery_threshold_percent: u8,
    #[serde(default = "default_alert_cooldown")]
    pub alert_cooldown_minutes: u64,
    #[serde(default)]
    pub games: Vec<GameConfig>,
    #[serde(default)]
    pub profiles: Vec<ApplicationProfile>,
    #[serde(default)]
    pub default_profile: Option<ApplicationProfile>,
    #[serde(default = "default_origins")]
    pub allowed_origins: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GameConfig {
    pub name: String,
    pub executables: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationProfile {
    pub application: ProfileApplication,
    pub device: ProfileDevice,
    pub settings: ProfileSettings,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileApplication {
    pub name: String,
    pub executable: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDevice {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSettings {
    pub dpi: Option<u32>,
    pub polling_rate_hz: Option<u32>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            battery_threshold_percent: DEFAULT_BATTERY_THRESHOLD,
            alert_cooldown_minutes: DEFAULT_ALERT_COOLDOWN_MINUTES,
            games: crate::games::catalog(),
            profiles: Vec::new(),
            default_profile: None,
            allowed_origins: default_origins(),
        }
    }
}

impl BridgeConfig {
    pub fn normalized(mut self) -> Self {
        self.battery_threshold_percent = self.battery_threshold_percent.min(100);
        self.alert_cooldown_minutes = self.alert_cooldown_minutes.max(1);
        crate::games::merge_catalog(&mut self.games);
        for game in &mut self.games {
            game.name = game.name.trim().to_owned();
            game.executables = game
                .executables
                .iter()
                .map(|entry| entry.trim().to_ascii_lowercase())
                .filter(|entry| !entry.is_empty())
                .collect();
            game.executables.sort();
            game.executables.dedup();
        }
        self.games
            .retain(|game| !game.name.is_empty() && !game.executables.is_empty());
        self.games.sort_by(|left, right| left.name.cmp(&right.name));
        self.games
            .dedup_by(|left, right| left.name.eq_ignore_ascii_case(&right.name));
        for profile in &mut self.profiles {
            profile.application.name = profile.application.name.trim().to_owned();
            profile.application.executable = profile.application.executable.trim().to_owned();
            profile.application.path = profile.application.path.trim().to_owned();
            profile.device.id = profile.device.id.trim().to_owned();
            profile.device.name = profile.device.name.trim().to_owned();
        }
        self.profiles.retain(|profile| {
            !profile.application.executable.is_empty()
                && !profile.application.path.is_empty()
                && !profile.device.id.is_empty()
        });
        self.profiles.sort_by(|left, right| {
            (&left.application.path, &left.device.id)
                .cmp(&(&right.application.path, &right.device.id))
        });
        self.profiles.dedup_by(|left, right| {
            left.application
                .path
                .eq_ignore_ascii_case(&right.application.path)
                && left.device.id == right.device.id
        });
        if let Some(profile) = &mut self.default_profile {
            profile.application.name = profile.application.name.trim().to_owned();
            profile.device.id = profile.device.id.trim().to_owned();
            profile.device.name = profile.device.name.trim().to_owned();
            if profile.application.name.is_empty()
                || profile.device.id.is_empty()
                || profile.device.name.is_empty()
            {
                self.default_profile = None;
            }
        }
        for origin in OFFICIAL_ORIGINS {
            if !self.allowed_origins.iter().any(|entry| entry == origin) {
                self.allowed_origins.push((*origin).to_owned());
            }
        }
        self.allowed_origins.sort();
        self.allowed_origins.dedup();
        self
    }
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("OPENMOUSE_BRIDGE_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let dirs = ProjectDirs::from("io", "OpenMouse", "OpenMouse Bridge")
        .context("the operating system did not provide an application-data directory")?;
    Ok(dirs.config_dir().join("config.json"))
}

pub fn load_or_create() -> Result<(BridgeConfig, PathBuf)> {
    let path = config_path()?;
    if path.exists() {
        let bytes =
            fs::read(&path).with_context(|| format!("could not read {}", path.display()))?;
        let config = serde_json::from_slice::<BridgeConfig>(&bytes)
            .with_context(|| format!("could not parse {}", path.display()))?
            .normalized();
        return Ok((config, path));
    }
    let config = BridgeConfig::default();
    save(&path, &config)?;
    Ok((config, path))
}

pub fn save(path: &PathBuf, config: &BridgeConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(config)?;
    fs::write(path, json).with_context(|| format!("could not write {}", path.display()))
}

const fn default_battery_threshold() -> u8 {
    DEFAULT_BATTERY_THRESHOLD
}

const fn default_alert_cooldown() -> u64 {
    DEFAULT_ALERT_COOLDOWN_MINUTES
}

fn default_origins() -> Vec<String> {
    vec![
        "https://dev.openmouse.app".to_owned(),
        "https://openmouse.app".to_owned(),
        "https://www.openmouse.app".to_owned(),
        "http://localhost:5173".to_owned(),
        "http://127.0.0.1:5173".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_clamps_threshold_and_cleans_executables() {
        let config = BridgeConfig {
            battery_threshold_percent: 140,
            alert_cooldown_minutes: 0,
            games: vec![GameConfig {
                name: " Valorant ".into(),
                executables: vec![" VALORANT-Win64-Shipping.exe ".into(), "".into()],
            }],
            profiles: Vec::new(),
            default_profile: None,
            allowed_origins: Vec::new(),
        }
        .normalized();
        assert_eq!(config.battery_threshold_percent, 100);
        assert_eq!(config.alert_cooldown_minutes, 1);
        let valorant = config
            .games
            .iter()
            .find(|game| game.name == "Valorant")
            .expect("Valorant should be in the built-in catalog");
        assert_eq!(valorant.executables, ["valorant-win64-shipping.exe"]);
        assert!(
            config
                .allowed_origins
                .iter()
                .any(|origin| origin == "https://dev.openmouse.app")
        );
    }
}
