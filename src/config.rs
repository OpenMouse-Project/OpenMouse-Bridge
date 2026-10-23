use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const DEFAULT_BATTERY_THRESHOLD: u8 = 20;
const DEFAULT_ALERT_COOLDOWN_MINUTES: u64 = 360;
const DEFAULT_NOTIFICATION_VOLUME: u8 = 60;
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
    /// Whether the on-screen banner plays a chime, and how loud (0–100).
    #[serde(default = "enabled_by_default")]
    pub notification_sound: bool,
    #[serde(default = "default_notification_volume")]
    pub notification_volume: u8,
    #[serde(default)]
    pub games: Vec<GameConfig>,
    /// Games the user added by hand. Kept apart from `games` because the
    /// catalog replaces that list on every start.
    #[serde(default)]
    pub custom_games: Vec<GameConfig>,
    #[serde(default)]
    pub profiles: Vec<ApplicationProfile>,
    #[serde(default)]
    pub default_profile: Option<ApplicationProfile>,
    #[serde(default = "default_origins")]
    pub allowed_origins: Vec<String>,
    /// Whether Bridge has already turned launch-at-login on by default, so a
    /// user who later turns it off keeps it off.
    #[serde(default)]
    pub autostart_configured: bool,
    /// Whether the tray panel has been shown once. Bridge stays in the tray
    /// after that; the first launch opens the panel so a window actually appears.
    #[serde(default)]
    pub tray_hint_shown: bool,
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
            notification_sound: true,
            notification_volume: DEFAULT_NOTIFICATION_VOLUME,
            games: Vec::new(),
            custom_games: Vec::new(),
            profiles: Vec::new(),
            default_profile: None,
            allowed_origins: default_origins(),
            autostart_configured: false,
            tray_hint_shown: false,
        }
    }
}

impl BridgeConfig {
    pub fn normalized(mut self) -> Self {
        self.battery_threshold_percent = self.battery_threshold_percent.min(100);
        self.notification_volume = self.notification_volume.min(100);
        self.alert_cooldown_minutes = self.alert_cooldown_minutes.max(1);
        normalize_games(&mut self.games);
        normalize_games(&mut self.custom_games);
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

    pub fn tracked_games(&self) -> Vec<GameConfig> {
        let mut games = self.games.clone();
        games.extend(
            self.custom_games
                .iter()
                .filter(|custom| {
                    !self
                        .games
                        .iter()
                        .any(|game| game.name.eq_ignore_ascii_case(&custom.name))
                })
                .cloned(),
        );
        games
    }
}

const MAX_GAMES: usize = 200;
const MAX_NAME_CHARS: usize = 80;
const MAX_EXECUTABLES: usize = 20;
const MAX_EXECUTABLE_CHARS: usize = 260;

fn normalize_games(games: &mut Vec<GameConfig>) {
    for game in games.iter_mut() {
        game.name = game.name.trim().chars().take(MAX_NAME_CHARS).collect();
        game.executables = game
            .executables
            .iter()
            .map(|entry| entry.trim().to_ascii_lowercase())
            .filter(|entry| !entry.is_empty() && entry.chars().count() <= MAX_EXECUTABLE_CHARS)
            .collect();
        game.executables.sort();
        game.executables.dedup();
        game.executables.truncate(MAX_EXECUTABLES);
    }
    games.retain(|game| !game.name.is_empty() && !game.executables.is_empty());
    // Sorted by the folded name, so names that differ only in case sit side by side for the dedup.
    games.sort_by_key(|game| game.name.to_ascii_lowercase());
    games.dedup_by(|left, right| left.name.eq_ignore_ascii_case(&right.name));
    games.truncate(MAX_GAMES);
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

const fn default_notification_volume() -> u8 {
    DEFAULT_NOTIFICATION_VOLUME
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
    fn names_that_differ_only_in_case_are_one_game_wherever_they_sort() {
        let game = |name: &str| GameConfig {
            name: name.into(),
            executables: vec!["g.exe".into()],
        };
        // Sorted by plain bytes these would be Alpha, Beta, alpha: the two Alphas apart.
        let mut games = vec![game("alpha"), game("Beta"), game("Alpha")];
        normalize_games(&mut games);
        assert_eq!(games.len(), 2);
    }

    #[test]
    fn game_lists_have_a_size_limit() {
        let many = |count: usize| -> Vec<GameConfig> {
            (0..count)
                .map(|i| GameConfig {
                    name: format!("game {i:04}"),
                    executables: vec![format!("g{i}.exe")],
                })
                .collect()
        };
        let mut games = many(MAX_GAMES + 50);
        normalize_games(&mut games);
        assert_eq!(games.len(), MAX_GAMES);

        let mut one = vec![GameConfig {
            name: "n".repeat(500),
            executables: (0..40)
                .map(|i| format!("{i}.exe"))
                .chain([format!("{}.exe", "x".repeat(300))])
                .collect(),
        }];
        normalize_games(&mut one);
        assert_eq!(one[0].name.chars().count(), MAX_NAME_CHARS);
        assert_eq!(one[0].executables.len(), MAX_EXECUTABLES);
        assert!(
            one[0]
                .executables
                .iter()
                .all(|entry| entry.chars().count() <= MAX_EXECUTABLE_CHARS)
        );
    }

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
            notification_sound: true,
            notification_volume: 180,
            custom_games: Vec::new(),
            profiles: Vec::new(),
            default_profile: None,
            allowed_origins: Vec::new(),
            autostart_configured: false,
            tray_hint_shown: false,
        }
        .normalized();
        assert_eq!(config.battery_threshold_percent, 100);
        assert_eq!(config.alert_cooldown_minutes, 1);
        assert_eq!(config.notification_volume, 100);
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
    fn custom_games_are_tracked_beside_the_catalog_and_the_catalog_wins_name_clashes() {
        let game = |name: &str, exe: &str| GameConfig {
            name: name.into(),
            executables: vec![exe.into()],
        };
        let config = BridgeConfig {
            games: vec![game("Valorant", "valorant.exe")],
            custom_games: vec![
                game("My Game", " MyGame.EXE "),
                game("valorant", "other.exe"),
            ],
            ..BridgeConfig::default()
        }
        .normalized();
        let names: Vec<_> = config
            .tracked_games()
            .into_iter()
            .map(|game| game.name)
            .collect();
        assert_eq!(names, ["Valorant", "My Game"]);
        assert_eq!(config.custom_games[0].executables, ["mygame.exe"]);
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
