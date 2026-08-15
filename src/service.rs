use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::{
    applications::{self, ApplicationInfo},
    config::{self, ApplicationProfile, BridgeConfig, GameConfig},
    games::GameDetector,
    platform,
};

#[derive(Clone)]
pub struct BridgeService {
    inner: Arc<RwLock<BridgeState>>,
    config_path: Arc<PathBuf>,
}

struct BridgeState {
    config: BridgeConfig,
    active_games: Vec<String>,
    applications: Vec<ApplicationInfo>,
    application_icons: HashMap<String, Option<Vec<u8>>>,
    battery: HashMap<String, BatteryState>,
    started_at: Instant,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatteryReading {
    pub device_id: String,
    pub device_name: String,
    pub percent: u8,
    #[serde(default)]
    pub charging: bool,
}

struct BatteryState {
    last_alert: Option<Instant>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameActivity {
    pub name: String,
    pub active: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeSnapshot {
    pub version: &'static str,
    pub platform: &'static str,
    pub uptime_seconds: u64,
    pub active_games: Vec<String>,
    pub games: Vec<GameActivity>,
    pub tracked_game_count: usize,
    pub battery_threshold_percent: u8,
    pub autostart_enabled: bool,
    pub foreground_application: Option<ApplicationInfo>,
    pub visible_application_count: usize,
    pub profile_count: usize,
}

impl BridgeService {
    pub fn new(config: BridgeConfig, config_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(RwLock::new(BridgeState {
                config,
                active_games: Vec::new(),
                applications: Vec::new(),
                application_icons: HashMap::new(),
                battery: HashMap::new(),
                started_at: Instant::now(),
            })),
            config_path: Arc::new(config_path),
        }
    }

    pub async fn snapshot(&self) -> BridgeSnapshot {
        let state = self.inner.read().await;
        BridgeSnapshot {
            version: crate::BRIDGE_VERSION,
            platform: platform::platform_name(),
            uptime_seconds: state.started_at.elapsed().as_secs(),
            active_games: state.active_games.clone(),
            games: state
                .config
                .games
                .iter()
                .map(|game| GameActivity {
                    name: game.name.clone(),
                    active: state.active_games.iter().any(|active| active == &game.name),
                })
                .collect(),
            tracked_game_count: state.config.games.len(),
            battery_threshold_percent: state.config.battery_threshold_percent,
            autostart_enabled: platform::autostart_enabled(),
            foreground_application: state
                .applications
                .iter()
                .find(|application| application.foreground)
                .cloned(),
            visible_application_count: state.applications.len(),
            profile_count: state.config.profiles.len(),
        }
    }

    pub async fn config(&self) -> BridgeConfig {
        self.inner.read().await.config.clone()
    }

    pub async fn applications(&self) -> Vec<ApplicationInfo> {
        self.inner.read().await.applications.clone()
    }

    pub async fn application_icon(&self, icon_id: &str) -> Option<Vec<u8>> {
        self.inner
            .read()
            .await
            .application_icons
            .get(icon_id)
            .cloned()
            .flatten()
    }

    pub async fn profiles(&self) -> Vec<ApplicationProfile> {
        self.inner.read().await.config.profiles.clone()
    }

    pub async fn replace_profiles(&self, profiles: Vec<ApplicationProfile>) -> Result<()> {
        let config = {
            let mut state = self.inner.write().await;
            state.config.profiles = profiles;
            state.config = state.config.clone().normalized();
            state.config.clone()
        };
        config::save(&self.config_path, &config)
    }

    pub async fn replace_games(&self, games: Vec<GameConfig>) -> Result<()> {
        let config = {
            let mut state = self.inner.write().await;
            state.config.games = games;
            state.config = state.config.clone().normalized();
            state.config.clone()
        };
        config::save(&self.config_path, &config)
    }

    pub async fn record_battery(&self, reading: BatteryReading) -> Result<bool> {
        let percent = reading.percent.min(100);
        let mut reading = reading;
        reading.percent = percent;
        let alert = {
            let mut state = self.inner.write().await;
            let threshold = state.config.battery_threshold_percent;
            let cooldown = Duration::from_secs(state.config.alert_cooldown_minutes * 60);
            let previous_alert = state
                .battery
                .get(&reading.device_id)
                .and_then(|entry| entry.last_alert);
            let should_alert = !reading.charging
                && reading.percent <= threshold
                && previous_alert.is_none_or(|last| last.elapsed() >= cooldown);
            state.battery.insert(
                reading.device_id.clone(),
                BatteryState {
                    last_alert: if should_alert {
                        Some(Instant::now())
                    } else {
                        previous_alert
                    },
                },
            );
            should_alert
        };
        if alert {
            platform::notify(
                "Mouse battery is low",
                &format!(
                    "{} has {}% battery remaining.",
                    reading.device_name, reading.percent
                ),
            )?;
        }
        Ok(alert)
    }

    pub fn start_game_monitor(&self) {
        let service = self.clone();
        tokio::spawn(async move {
            let mut detector = GameDetector::default();
            let mut interval = tokio::time::interval(Duration::from_secs(3));
            loop {
                interval.tick().await;
                let games = service.inner.read().await.config.games.clone();
                let active = detector.detect(&games);
                let applications = applications::visible_applications();
                let missing_icons = {
                    let state = service.inner.read().await;
                    applications
                        .iter()
                        .filter(|application| {
                            !state.application_icons.contains_key(&application.icon_id)
                        })
                        .map(|application| (application.icon_id.clone(), application.path.clone()))
                        .collect::<Vec<_>>()
                };
                let icons = missing_icons
                    .into_iter()
                    .map(|(icon_id, path)| (icon_id, applications::application_icon(&path)))
                    .collect::<Vec<_>>();
                let mut state = service.inner.write().await;
                state.active_games = active;
                state.applications = applications;
                state.application_icons.extend(icons);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(config: BridgeConfig) -> BridgeService {
        BridgeService::new(config, PathBuf::from("unused-test-config.json"))
    }

    #[tokio::test]
    async fn charging_and_healthy_readings_do_not_alert() {
        let bridge = service(BridgeConfig::default());
        assert!(
            !bridge
                .record_battery(BatteryReading {
                    device_id: "mouse".into(),
                    device_name: "Mouse".into(),
                    percent: 90,
                    charging: false,
                })
                .await
                .unwrap()
        );
        assert!(
            !bridge
                .record_battery(BatteryReading {
                    device_id: "mouse".into(),
                    device_name: "Mouse".into(),
                    percent: 10,
                    charging: true,
                })
                .await
                .unwrap()
        );
    }
}
