use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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

fn normalized_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_registered_game(application: &ApplicationInfo, games: &[GameConfig]) -> bool {
    let application_name = normalized_name(&application.name);
    let executable = application.executable.to_ascii_lowercase();
    let executable_stem = executable.strip_suffix(".exe").unwrap_or(&executable);
    games.iter().any(|game| {
        normalized_name(&game.name) == application_name
            || normalized_name(&game.name) == normalized_name(executable_stem)
            || game
                .executables
                .iter()
                .any(|registered| registered.eq_ignore_ascii_case(&application.executable))
    })
}

/// How long a new active profile must persist before a switch notification
/// fires, so rapid alt-tabbing does not spam notifications.
const PROFILE_DEBOUNCE: Duration = Duration::from_millis(2500);

/// The profile that applies to the current foreground application, falling back
/// to the configured default profile. Shared by the snapshot and the monitor.
fn active_profile_for(
    config: &BridgeConfig,
    applications: &[ApplicationInfo],
) -> Option<ApplicationProfile> {
    applications
        .iter()
        .find(|application| application.foreground)
        .and_then(|application| {
            config
                .profiles
                .iter()
                .find(|profile| {
                    profile
                        .application
                        .path
                        .eq_ignore_ascii_case(&application.path)
                        || profile
                            .application
                            .name
                            .eq_ignore_ascii_case(&application.name)
                        || profile
                            .application
                            .executable
                            .eq_ignore_ascii_case(&application.executable)
                })
                .cloned()
        })
        .or_else(|| config.default_profile.clone())
}

/// A stable identity for a profile, used to detect switches.
fn profile_key(profile: &ApplicationProfile) -> String {
    if profile.application.path.is_empty() {
        profile.application.name.to_ascii_lowercase()
    } else {
        profile.application.path.to_ascii_lowercase()
    }
}

fn profile_notification_body(profile: &ApplicationProfile) -> String {
    let mut parts = Vec::new();
    if let Some(dpi) = profile.settings.dpi {
        parts.push(format!("{dpi} DPI"));
    }
    if let Some(rate) = profile.settings.polling_rate_hz {
        parts.push(format!("{rate} Hz"));
    }
    if parts.is_empty() {
        profile.application.name.clone()
    } else {
        format!("{} · {}", profile.application.name, parts.join(" · "))
    }
}

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
    // Debounced active-profile tracking for switch notifications.
    active_profile_key: Option<String>,
    pending_profile: Option<(Option<String>, Instant)>,
    profile_seeded: bool,
    started_at: Instant,
    last_client_heartbeat: Option<Instant>,
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
    device_name: String,
    percent: u8,
    charging: bool,
    updated_at: Instant,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceBattery {
    pub device_id: String,
    pub device_name: String,
    pub percent: u8,
    pub charging: bool,
    pub stale: bool,
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
    pub linux_distribution: Option<String>,
    pub uptime_seconds: u64,
    pub active_games: Vec<String>,
    pub games: Vec<GameActivity>,
    pub tracked_game_count: usize,
    pub battery_threshold_percent: u8,
    pub autostart_enabled: bool,
    pub foreground_application: Option<ApplicationInfo>,
    pub active_profile: Option<ApplicationProfile>,
    pub visible_application_count: usize,
    pub profile_count: usize,
    pub client_connected: bool,
    pub batteries: Vec<DeviceBattery>,
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
                active_profile_key: None,
                pending_profile: None,
                profile_seeded: false,
                started_at: Instant::now(),
                last_client_heartbeat: None,
            })),
            config_path: Arc::new(config_path),
        }
    }

    pub async fn snapshot(&self) -> BridgeSnapshot {
        let state = self.inner.read().await;
        let client_connected = state
            .last_client_heartbeat
            .is_some_and(|heartbeat| heartbeat.elapsed() < Duration::from_secs(20));
        let foreground_application = state
            .applications
            .iter()
            .find(|application| application.foreground)
            .cloned();
        let active_profile = active_profile_for(&state.config, &state.applications);
        let mut batteries = state
            .battery
            .iter()
            .map(|(device_id, entry)| DeviceBattery {
                device_id: device_id.clone(),
                device_name: entry.device_name.clone(),
                percent: entry.percent,
                charging: entry.charging,
                stale: entry.updated_at.elapsed() >= Duration::from_secs(120),
            })
            .collect::<Vec<_>>();
        batteries.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        BridgeSnapshot {
            version: crate::BRIDGE_VERSION,
            platform: platform::platform_name(),
            linux_distribution: platform::linux_distribution(),
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
            foreground_application,
            active_profile,
            visible_application_count: state.applications.len(),
            profile_count: state.config.profiles.len(),
            client_connected,
            batteries,
        }
    }

    pub async fn record_client_heartbeat(&self) {
        self.inner.write().await.last_client_heartbeat = Some(Instant::now());
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

    pub async fn games(&self) -> Vec<GameConfig> {
        self.inner.read().await.config.games.clone()
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

    pub async fn set_default_profile(&self, profile: ApplicationProfile) -> Result<()> {
        let config = {
            let mut state = self.inner.write().await;
            state.config.default_profile = Some(profile);
            state.config = state.config.clone().normalized();
            state.config.clone()
        };
        config::save(&self.config_path, &config)
    }

    pub async fn set_battery_threshold(&self, percent: u8) -> Result<()> {
        let config = {
            let mut state = self.inner.write().await;
            state.config.battery_threshold_percent = percent;
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
                    device_name: reading.device_name.clone(),
                    percent: reading.percent,
                    charging: reading.charging,
                    updated_at: Instant::now(),
                },
            );
            should_alert
        };
        if alert {
            let body = format!(
                "{} has {}% battery remaining.",
                reading.device_name, reading.percent
            );
            // Showing a notification can block (e.g. an unbundled macOS binary
            // pops a chooser dialog), so run it detached: never stall the request
            // or the runtime, and never let a notification failure fail the write.
            std::thread::spawn(move || {
                if let Err(error) = platform::notify("Mouse battery is low", &body) {
                    tracing::warn!(%error, "Could not show the low-battery notification");
                }
            });
        }
        Ok(alert)
    }

    pub fn start_game_monitor(&self, window_active: Arc<AtomicBool>) {
        let service = self.clone();
        tokio::spawn(async move {
            let mut detector = GameDetector::default();
            let mut interval = tokio::time::interval(Duration::from_secs(3));
            loop {
                interval.tick().await;
                let games = service.inner.read().await.config.games.clone();
                let active = detector.detect(&games);
                let applications = applications::visible_applications()
                    .into_iter()
                    .filter(|application| is_registered_game(application, &games))
                    .collect::<Vec<_>>();
                // Application icons are only used by the status window, so skip
                // the expensive extraction while it is hidden to the tray.
                let icons = if window_active.load(Ordering::Acquire) {
                    let missing_icons = {
                        let state = service.inner.read().await;
                        applications
                            .iter()
                            .filter(|application| {
                                !state.application_icons.contains_key(&application.icon_id)
                            })
                            .map(|application| {
                                (application.icon_id.clone(), application.path.clone())
                            })
                            .collect::<Vec<_>>()
                    };
                    missing_icons
                        .into_iter()
                        .map(|(icon_id, path)| (icon_id, applications::application_icon(&path)))
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let mut state = service.inner.write().await;
                state.active_games = active;
                state.applications = applications;
                state.application_icons.extend(icons);

                // Detect a debounced active-profile switch, push the new
                // profile's DPI/polling rate to the mouse over native HID
                // when Bridge has a driver for it, and notify once the new
                // profile has stayed active long enough.
                let current = active_profile_for(&state.config, &state.applications);
                let current_key = current.as_ref().map(profile_key);
                // (profile to push to hardware, whether to notify about it)
                let mut apply: Option<(ApplicationProfile, bool)> = None;
                if !state.profile_seeded {
                    // Adopt the initial profile silently so startup is quiet,
                    // but still push it to the mouse — Bridge should reflect
                    // the right settings from a cold start, not only switches.
                    state.active_profile_key = current_key;
                    state.pending_profile = None;
                    state.profile_seeded = true;
                    apply = current.clone().map(|profile| (profile, false));
                } else if current_key == state.active_profile_key {
                    state.pending_profile = None;
                } else {
                    let ready = matches!(
                        &state.pending_profile,
                        Some((key, since)) if *key == current_key && since.elapsed() >= PROFILE_DEBOUNCE
                    );
                    let waiting =
                        matches!(&state.pending_profile, Some((key, _)) if *key == current_key);
                    if ready {
                        state.active_profile_key = current_key;
                        state.pending_profile = None;
                        apply = current.clone().map(|profile| (profile, true));
                    } else if !waiting {
                        state.pending_profile = Some((current_key, Instant::now()));
                    }
                }
                drop(state);
                if let Some((profile, announce)) = apply {
                    std::thread::spawn(move || {
                        let outcome = crate::drivers::apply_profile(&profile);
                        match &outcome {
                            Ok(true) => tracing::info!(
                                profile = %profile.application.name,
                                device = %profile.device.name,
                                "Applied the mouse profile natively"
                            ),
                            Ok(false) => tracing::debug!(
                                profile = %profile.application.name,
                                device = %profile.device.name,
                                "No native driver for this device; not applying natively"
                            ),
                            Err(error) => tracing::warn!(
                                %error,
                                profile = %profile.application.name,
                                device = %profile.device.name,
                                "Could not apply the mouse profile natively"
                            ),
                        }
                        if !announce {
                            return;
                        }
                        let (title, body) = match outcome {
                            Ok(true) => {
                                ("Mouse profile applied", profile_notification_body(&profile))
                            }
                            Ok(false) => (
                                "Mouse profile selected",
                                format!(
                                    "{} — open OpenMouse to apply it to this mouse.",
                                    profile.application.name
                                ),
                            ),
                            Err(_) => (
                                "Mouse profile selected",
                                format!(
                                    "{} — could not reach the mouse. Open OpenMouse to apply it.",
                                    profile.application.name
                                ),
                            ),
                        };
                        if let Err(error) = platform::notify(title, &body) {
                            tracing::warn!(%error, "Could not show the profile notification");
                        }
                    });
                }
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

    fn test_profile(
        name: &str,
        path: &str,
        dpi: Option<u32>,
        hz: Option<u32>,
    ) -> ApplicationProfile {
        ApplicationProfile {
            application: config::ProfileApplication {
                name: name.into(),
                executable: format!("{name}.exe"),
                path: path.into(),
            },
            device: config::ProfileDevice {
                id: "device".into(),
                name: "Mouse".into(),
            },
            settings: config::ProfileSettings {
                dpi,
                polling_rate_hz: hz,
            },
        }
    }

    fn test_app(name: &str, path: &str, foreground: bool) -> ApplicationInfo {
        ApplicationInfo {
            name: name.into(),
            executable: format!("{name}.exe"),
            path: path.into(),
            foreground,
            icon_id: String::new(),
        }
    }

    #[test]
    fn active_profile_matches_foreground_then_falls_back_to_default() {
        let mut config = BridgeConfig::default();
        let valorant = test_profile("Valorant", "/games/valorant", Some(800), Some(1000));
        config.profiles = vec![valorant.clone()];
        config.default_profile = Some(test_profile("Default", "", Some(400), None));

        let apps = vec![
            test_app("Chrome", "/apps/chrome", false),
            test_app("Valorant", "/games/valorant", true),
        ];
        assert_eq!(active_profile_for(&config, &apps), Some(valorant));

        let apps = vec![test_app("Chrome", "/apps/chrome", true)];
        assert_eq!(
            active_profile_for(&config, &apps).unwrap().application.name,
            "Default"
        );

        let apps = vec![test_app("Chrome", "/apps/chrome", false)];
        assert_eq!(
            active_profile_for(&config, &apps).unwrap().application.name,
            "Default"
        );
    }

    #[test]
    fn profile_notification_body_lists_available_settings() {
        assert_eq!(
            profile_notification_body(&test_profile("Valorant", "/g", Some(800), Some(1000))),
            "Valorant · 800 DPI · 1000 Hz"
        );
        assert_eq!(
            profile_notification_body(&test_profile("CS", "/g", Some(400), None)),
            "CS · 400 DPI"
        );
        assert_eq!(
            profile_notification_body(&test_profile("App", "/g", None, None)),
            "App"
        );
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

    #[test]
    fn application_picker_only_accepts_catalog_games() {
        let games = vec![GameConfig {
            name: "Counter-Strike 2".into(),
            executables: vec!["cs2.exe".into()],
        }];
        let application = |name: &str, executable: &str| ApplicationInfo {
            name: name.into(),
            executable: executable.into(),
            path: executable.into(),
            foreground: false,
            icon_id: "icon".into(),
        };

        assert!(is_registered_game(
            &application("Counter-Strike 2", "cs2"),
            &games
        ));
        assert!(is_registered_game(
            &application("Counter-Strike 2", "cs2.exe"),
            &games
        ));
        assert!(!is_registered_game(
            &application("Google Chrome", "chrome.exe"),
            &games
        ));
    }

    #[tokio::test]
    async fn client_is_connected_only_after_a_heartbeat() {
        let bridge = service(BridgeConfig::default());
        assert!(!bridge.snapshot().await.client_connected);

        bridge.record_client_heartbeat().await;

        assert!(bridge.snapshot().await.client_connected);
    }
}
