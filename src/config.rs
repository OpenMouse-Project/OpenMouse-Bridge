use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const DEFAULT_BATTERY_THRESHOLD: u8 = 20;
const DEFAULT_ALERT_COOLDOWN_MINUTES: u64 = 360;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeConfig {
    #[serde(default = "default_battery_threshold")]
    pub battery_threshold_percent: u8,
    #[serde(default = "default_alert_cooldown")]
    pub alert_cooldown_minutes: u64,
    #[serde(default)]
    pub games: Vec<GameConfig>,
    #[serde(default = "default_origins")]
    pub allowed_origins: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GameConfig {
    pub name: String,
    pub executables: Vec<String>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            battery_threshold_percent: DEFAULT_BATTERY_THRESHOLD,
            alert_cooldown_minutes: DEFAULT_ALERT_COOLDOWN_MINUTES,
            games: Vec::new(),
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
        "https://openmouse.io".to_owned(),
        "https://www.openmouse.io".to_owned(),
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
            allowed_origins: Vec::new(),
        }
        .normalized();
        assert_eq!(config.battery_threshold_percent, 100);
        assert_eq!(config.alert_cooldown_minutes, 1);
        assert_eq!(config.games[0].name, "Valorant");
        assert_eq!(config.games[0].executables, ["valorant-win64-shipping.exe"]);
    }
}
