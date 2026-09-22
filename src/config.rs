use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const DEFAULT_BATTERY_THRESHOLD: u8 = 20;
const DEFAULT_ALERT_COOLDOWN_MINUTES: u64 = 360;
const OFFICIAL_ORIGINS: &[&str] = &[
    "https://control.openmouse.app",
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
    pub automatic_updates: bool,
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
    /// A disabled profile is kept (so the control panel can store a game's
    /// settings while its automatic apply is off) but never matched.
    /// Configs written before this field existed only held active profiles.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

impl ApplicationProfile {
    /// Which application the profile is for, as the matcher sees it: the full
    /// path when there is one (Windows application profiles), otherwise the
    /// executable, otherwise the name (game profiles from the control panel,
    /// which only know a game's executables). Lowercased, as matching is.
    pub fn application_key(&self) -> String {
        let application = &self.application;
        [
            &application.path,
            &application.executable,
            &application.name,
        ]
        .into_iter()
        .find(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default()
    }
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
    /// Every other device setting the profile carries, as the control
    /// panel's `Partial<MouseStatus>` field map. Bridge never interprets it:
    /// it only stores it and hands it back in `/v1/status`'s activeProfile so
    /// an open OpenMouse tab can apply it over WebHID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<serde_json::Value>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            battery_threshold_percent: DEFAULT_BATTERY_THRESHOLD,
            alert_cooldown_minutes: DEFAULT_ALERT_COOLDOWN_MINUTES,
            automatic_updates: false,
            games: Vec::new(),
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
        // Game profiles from the control panel carry no path (a game is known
        // by its executables), so a profile only needs something to match on.
        self.profiles.retain(|profile| {
            !profile.application_key().is_empty() && !profile.device.id.is_empty()
        });
        self.profiles
            .sort_by_cached_key(|profile| (profile.application_key(), profile.device.id.clone()));
        self.profiles.dedup_by(|left, right| {
            left.application_key() == right.application_key() && left.device.id == right.device.id
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

pub fn log_dir() -> Result<PathBuf> {
    let config = config_path()?;
    let parent = config
        .parent()
        .context("the Bridge config path has no parent directory")?;
    Ok(parent.join("logs"))
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

pub async fn load_with_catalog() -> Result<(BridgeConfig, PathBuf)> {
    let (mut config, path) = load_or_create()?;
    match crate::games::fetch_catalog().await {
        Ok(games) => {
            let cached_games = std::mem::replace(&mut config.games, games);
            config = config.normalized();
            let changed = config.games != cached_games;
            if changed {
                save(&path, &config)?;
            }
            tracing::info!(
                url = %crate::games::catalog_url(),
                games = config.games.len(),
                changed,
                "game catalog loaded"
            );
        }
        Err(error) => {
            tracing::warn!(
                %error,
                cached_games = config.games.len(),
                "game catalog unavailable; using cached catalog"
            );
        }
    }
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
        "https://control.openmouse.app".to_owned(),
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
            automatic_updates: false,
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
            .expect("configured game should survive normalization");
        assert_eq!(valorant.executables, ["valorant-win64-shipping.exe"]);
        assert!(
            config
                .allowed_origins
                .iter()
                .any(|origin| origin == "https://control.openmouse.app")
        );
    }

    #[test]
    fn profile_settings_keep_the_snapshot_and_accept_profiles_without_one() {
        let json =
            r#"{"dpi":800,"pollingRateHz":null,"snapshot":{"lightforceSwitchMode":"Optical"}}"#;
        let settings: ProfileSettings =
            serde_json::from_str(json).expect("snapshot profile parses");
        assert_eq!(
            settings.snapshot,
            Some(serde_json::json!({ "lightforceSwitchMode": "Optical" }))
        );
        assert_eq!(serde_json::to_string(&settings).unwrap(), json);

        let legacy: ProfileSettings = serde_json::from_str(r#"{"dpi":800,"pollingRateHz":1000}"#)
            .expect("legacy profile parses");
        assert_eq!(legacy.snapshot, None);
        assert!(!serde_json::to_string(&legacy).unwrap().contains("snapshot"));
    }

    fn game_profile(name: &str, executable: &str, device: &str) -> ApplicationProfile {
        ApplicationProfile {
            application: ProfileApplication {
                name: name.into(),
                executable: executable.into(),
                path: String::new(),
            },
            device: ProfileDevice {
                id: device.into(),
                name: "Mouse".into(),
            },
            settings: ProfileSettings {
                dpi: Some(800),
                polling_rate_hz: None,
                snapshot: None,
            },
            enabled: true,
        }
    }

    #[test]
    fn normalization_keeps_path_less_game_profiles_and_dedupes_by_executable() {
        let config = BridgeConfig {
            profiles: vec![
                game_profile("Apex Legends", "r5apex.exe", "Logitech:Mouse"),
                game_profile("Apex Legends", "R5Apex.exe", "Logitech:Mouse"),
                game_profile("VALORANT", "valorant.exe", "Logitech:Mouse"),
                game_profile("", "", "Logitech:Mouse"),
            ],
            ..BridgeConfig::default()
        };
        let names: Vec<_> = config
            .normalized()
            .profiles
            .into_iter()
            .map(|profile| profile.application.name)
            .collect();
        assert_eq!(names, ["Apex Legends", "VALORANT"]);
    }

    #[test]
    fn profiles_saved_before_the_enabled_flag_load_as_enabled() {
        let json = r#"{"application":{"name":"Apex Legends","executable":"r5apex.exe","path":""},"device":{"id":"d","name":"Mouse"},"settings":{"dpi":800,"pollingRateHz":null}}"#;
        let profile: ApplicationProfile =
            serde_json::from_str(json).expect("legacy profile parses");
        assert!(profile.enabled);
    }
}
