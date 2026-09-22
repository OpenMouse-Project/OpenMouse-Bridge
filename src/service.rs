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

/// How often Bridge reads mouse batteries itself, so low-battery alerts work
/// without the control panel open. Each read spawns the native-hid helper.
const BATTERY_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// A reading older than this is shown as stale: more than two missed polls.
const BATTERY_STALE_AFTER: Duration = Duration::from_secs(12 * 60);

/// The distinct devices named by the saved profiles, as `(id, name)`.
fn profile_devices(config: &BridgeConfig) -> Vec<(String, String)> {
    let mut devices: Vec<(String, String)> = Vec::new();
    for profile in config.default_profile.iter().chain(&config.profiles) {
        if !devices.iter().any(|(id, _)| id == &profile.device.id) {
            devices.push((profile.device.id.clone(), profile.device.name.clone()));
        }
    }
    devices
}

/// The profile that applies right now: one for the application in front, else
/// one for a game that is still running, so alt-tabbing out of a game keeps
/// its profile until the game closes, else the configured default profile.
/// Shared by the snapshot and the monitor.
fn active_profile_for(
    config: &BridgeConfig,
    applications: &[ApplicationInfo],
    active_games: &[String],
) -> Option<ApplicationProfile> {
    let enabled = || config.profiles.iter().filter(|profile| profile.enabled);
    let foreground = applications
        .iter()
        .find(|application| application.foreground)
        .and_then(|application| {
            enabled().find(|profile| {
                // Game profiles leave some of these empty; an empty field
                // must not match an application that also reports none.
                let matches = |saved: &str, running: &str| {
                    !saved.is_empty() && saved.eq_ignore_ascii_case(running)
                };
                matches(&profile.application.path, &application.path)
                    || matches(&profile.application.name, &application.name)
                    || matches(&profile.application.executable, &application.executable)
            })
        });
    let running_game = || {
        active_games.iter().find_map(|name| {
            let game = config.games.iter().find(|game| &game.name == name)?;
            enabled().find(|profile| profile_matches_game(profile, game))
        })
    };
    foreground
        .or_else(running_game)
        .cloned()
        .or_else(|| config.default_profile.clone())
}

fn profile_matches_game(profile: &ApplicationProfile, game: &GameConfig) -> bool {
    let application = &profile.application;
    (!application.name.is_empty()
        && normalized_name(&application.name) == normalized_name(&game.name))
        || (!application.executable.is_empty()
            && game
                .executables
                .iter()
                .any(|executable| executable.eq_ignore_ascii_case(&application.executable)))
}

/// A stable identity for a profile, used to detect switches.
fn profile_key(profile: &ApplicationProfile) -> String {
    if profile.application.path.is_empty() {
        profile.application.name.to_ascii_lowercase()
    } else {
        profile.application.path.to_ascii_lowercase()
    }
}

/// A profile switch worth telling the user about, already worded: a title such
/// as the game's name and a short detail line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSwitch {
    pub title: String,
    pub detail: String,
}

/// Receives profile switches instead of the platform notification, e.g. the
/// desktop app's on-screen overlay.
pub type ProfileSwitchListener = Arc<dyn Fn(ProfileSwitch) + Send + Sync>;

/// How pushing a profile to the mouse over native HID went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplyOutcome {
    Applied,
    NoDriver,
    Failed,
}

/// The settings a profile sets, e.g. "1600 DPI · 1000 Hz".
fn settings_summary(profile: &ApplicationProfile) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(dpi) = profile.settings.dpi {
        parts.push(format!("{dpi} DPI"));
    }
    if let Some(rate) = profile.settings.polling_rate_hz {
        parts.push(format!("{rate} Hz"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn with_settings(lead: String, profile: &ApplicationProfile) -> String {
    match settings_summary(profile) {
        Some(settings) => format!("{lead} · {settings}"),
        None => lead,
    }
}

/// Words a switch from `previous` to `current`. Returns `None` when nothing
/// the user cares about changed (e.g. default to default).
///
/// When the control panel is connected it applies the profile itself, so a
/// native apply that could not reach the mouse is not worth mentioning then.
fn switch_message(
    previous: Option<&ApplicationProfile>,
    current: Option<&ApplicationProfile>,
    default: Option<&ApplicationProfile>,
    outcome: Option<ApplyOutcome>,
    panel_connected: bool,
) -> Option<ProfileSwitch> {
    let is_default = |profile: &ApplicationProfile| default == Some(profile);
    let left = previous.filter(|profile| !is_default(profile));
    let mut switch = match current {
        Some(profile) if !is_default(profile) => ProfileSwitch {
            title: profile.application.name.clone(),
            detail: with_settings("Profile on".into(), profile),
        },
        Some(profile) => ProfileSwitch {
            title: "Default profile".into(),
            detail: with_settings(format!("{} closed", left?.application.name), profile),
        },
        None => ProfileSwitch {
            title: format!("{} closed", left?.application.name),
            detail: "Profile off".into(),
        },
    };
    if current.is_some() && !panel_connected {
        match outcome {
            Some(ApplyOutcome::Failed) => {
                switch.detail = "Couldn't reach the mouse — open OpenMouse to apply".into();
            }
            Some(ApplyOutcome::NoDriver) => {
                switch.detail = "Open OpenMouse to apply this profile".into();
            }
            _ => {}
        }
    }
    Some(switch)
}

#[derive(Clone)]
pub struct BridgeService {
    inner: Arc<RwLock<BridgeState>>,
    config_path: Arc<PathBuf>,
    switch_listener: Arc<std::sync::Mutex<Option<ProfileSwitchListener>>>,
}

struct BridgeState {
    config: BridgeConfig,
    active_games: Vec<String>,
    applications: Vec<ApplicationInfo>,
    application_icons: HashMap<String, Option<Vec<u8>>>,
    battery: HashMap<String, BatteryState>,
    // Debounced active-profile tracking for switch notifications.
    active_profile_key: Option<String>,
    applied_profile: Option<ApplicationProfile>,
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
    pub automatic_updates: bool,
    pub foreground_application: Option<ApplicationInfo>,
    pub active_profile: Option<ApplicationProfile>,
    pub active_profile_is_default: bool,
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
                applied_profile: None,
                pending_profile: None,
                profile_seeded: false,
                started_at: Instant::now(),
                last_client_heartbeat: None,
            })),
            config_path: Arc::new(config_path),
            switch_listener: Arc::default(),
        }
    }

    /// Sends profile switches to `listener` instead of showing a platform
    /// notification for them.
    pub fn on_profile_switch(&self, listener: impl Fn(ProfileSwitch) + Send + Sync + 'static) {
        *self
            .switch_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(listener));
    }

    pub async fn snapshot(&self) -> BridgeSnapshot {
        let state = self.inner.read().await;
        let client_connected = crate::hid::client_session_active()
            || state
                .last_client_heartbeat
                .is_some_and(|heartbeat| heartbeat.elapsed() < Duration::from_secs(20));
        let foreground_application = state
            .applications
            .iter()
            .find(|application| application.foreground)
            .cloned();
        let active_profile =
            active_profile_for(&state.config, &state.applications, &state.active_games);
        let active_profile_is_default =
            active_profile.is_some() && active_profile == state.config.default_profile;
        let mut batteries = state
            .battery
            .iter()
            .map(|(device_id, entry)| DeviceBattery {
                device_id: device_id.clone(),
                device_name: entry.device_name.clone(),
                percent: entry.percent,
                charging: entry.charging,
                stale: entry.updated_at.elapsed() >= BATTERY_STALE_AFTER,
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
            automatic_updates: state.config.automatic_updates,
            foreground_application,
            active_profile,
            active_profile_is_default,
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

    /// Turns launch-at-login on the first time a release build runs; after
    /// that the user's choice stands. Debug builds never register themselves.
    pub async fn enable_autostart_once(&self) -> Result<()> {
        if cfg!(debug_assertions) || self.inner.read().await.config.autostart_configured {
            return Ok(());
        }
        platform::set_autostart(true)?;
        let config = {
            let mut state = self.inner.write().await;
            state.config.autostart_configured = true;
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

    pub async fn set_automatic_updates(&self, enabled: bool) -> Result<()> {
        let config = {
            let mut state = self.inner.write().await;
            state.config.automatic_updates = enabled;
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

    /// Reads the battery of every mouse in the saved profiles on an interval
    /// and records it like a reading from the control panel, which raises the
    /// low-battery notification. Polling pauses while the control panel holds
    /// a HID session, so Bridge never talks to a mouse mid-session.
    pub fn start_battery_monitor(&self) {
        let service = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(BATTERY_POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if crate::hid::client_session_active() {
                    continue;
                }
                let devices = profile_devices(&service.inner.read().await.config);
                for (device_id, device_name) in devices {
                    let id = device_id.clone();
                    let outcome =
                        tokio::task::spawn_blocking(move || crate::drivers::read_battery(&id))
                            .await;
                    match outcome {
                        Ok(Ok(Some(battery))) => {
                            let reading = BatteryReading {
                                device_id,
                                device_name,
                                percent: battery.percent,
                                charging: battery.charging,
                            };
                            if let Err(error) = service.record_battery(reading).await {
                                tracing::warn!(%error, "Could not record the mouse battery");
                            }
                        }
                        Ok(Ok(None)) => {
                            tracing::debug!(%device_id, "No native battery reader for this device");
                        }
                        Ok(Err(error)) => {
                            tracing::debug!(%error, %device_id, "Could not read the mouse battery");
                        }
                        Err(error) => {
                            tracing::warn!(%error, "The battery reader stopped unexpectedly");
                        }
                    }
                }
            }
        });
    }

    pub fn start_game_monitor(&self, extract_icons: Arc<AtomicBool>) {
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
                // Icon extraction can be disabled by callers that never serve
                // the application's icon endpoint.
                let icons = if extract_icons.load(Ordering::Acquire) {
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
                // when Bridge has a driver for it, and announce the switch
                // once the new profile has stayed active long enough.
                let current =
                    active_profile_for(&state.config, &state.applications, &state.active_games);
                let current_key = current.as_ref().map(profile_key);
                // (new profile, the one it replaces, whether to announce it)
                let mut switch: Option<(
                    Option<ApplicationProfile>,
                    Option<ApplicationProfile>,
                    bool,
                )> = None;
                if !state.profile_seeded {
                    // Adopt the initial profile silently so startup is quiet,
                    // but still push it to the mouse — Bridge should reflect
                    // the right settings from a cold start, not only switches.
                    state.active_profile_key = current_key;
                    state.applied_profile = current.clone();
                    state.pending_profile = None;
                    state.profile_seeded = true;
                    switch = Some((current, None, false));
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
                        let previous =
                            std::mem::replace(&mut state.applied_profile, current.clone());
                        switch = Some((current, previous, true));
                    } else if !waiting {
                        state.pending_profile = Some((current_key, Instant::now()));
                    }
                }
                let default = state.config.default_profile.clone();
                drop(state);
                if let Some((current, previous, announce)) = switch {
                    let listener = service
                        .switch_listener
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    std::thread::spawn(move || {
                        let outcome = current.as_ref().map(apply_natively);
                        if !announce {
                            return;
                        }
                        let Some(message) = switch_message(
                            previous.as_ref(),
                            current.as_ref(),
                            default.as_ref(),
                            outcome,
                            crate::hid::client_session_active(),
                        ) else {
                            return;
                        };
                        match listener {
                            Some(listener) => listener(message),
                            None => {
                                if let Err(error) =
                                    platform::notify(&message.title, &message.detail)
                                {
                                    tracing::warn!(%error, "Could not show the profile notification");
                                }
                            }
                        }
                    });
                }
            }
        });
    }
}

fn apply_natively(profile: &ApplicationProfile) -> ApplyOutcome {
    match crate::drivers::apply_profile(profile) {
        Ok(true) => {
            tracing::info!(
                profile = %profile.application.name,
                device = %profile.device.name,
                "Applied the mouse profile natively"
            );
            ApplyOutcome::Applied
        }
        Ok(false) => {
            tracing::debug!(
                profile = %profile.application.name,
                device = %profile.device.name,
                "No native driver for this device; not applying natively"
            );
            ApplyOutcome::NoDriver
        }
        Err(error) => {
            tracing::warn!(
                %error,
                profile = %profile.application.name,
                device = %profile.device.name,
                "Could not apply the mouse profile natively"
            );
            ApplyOutcome::Failed
        }
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
                snapshot: None,
            },
            enabled: true,
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
    fn profile_devices_are_distinct_and_include_the_default() {
        let mut config = BridgeConfig::default();
        let mut default = test_profile("Default", "", None, None);
        default.device.id = "Logitech:PRO X SUPERLIGHT 2c".into();
        default.device.name = "PRO X SUPERLIGHT 2c".into();
        let mut game = test_profile("Valorant", "/games/valorant", Some(800), None);
        game.device = default.device.clone();
        let mut other = test_profile("CS2", "/games/cs2", Some(400), None);
        other.device.id = "Razer:Viper V3 Pro".into();
        other.device.name = "Viper V3 Pro".into();
        config.default_profile = Some(default);
        config.profiles = vec![game, other];

        assert_eq!(
            profile_devices(&config),
            vec![
                (
                    "Logitech:PRO X SUPERLIGHT 2c".to_owned(),
                    "PRO X SUPERLIGHT 2c".to_owned()
                ),
                ("Razer:Viper V3 Pro".to_owned(), "Viper V3 Pro".to_owned()),
            ]
        );
    }

    #[test]
    fn empty_profile_fields_never_match_empty_application_fields() {
        let mut config = BridgeConfig::default();
        let mut apex = test_profile("Apex Legends", "", Some(800), None);
        apex.application.executable = String::new();
        config.profiles = vec![apex];

        let mut unnamed = test_app("Other", "", true);
        unnamed.executable = String::new();
        assert_eq!(active_profile_for(&config, &[unnamed], &[]), None);
    }

    #[test]
    fn disabled_profiles_are_never_matched() {
        let mut config = BridgeConfig::default();
        let mut valorant = test_profile("Valorant", "", Some(800), None);
        valorant.enabled = false;
        config.profiles = vec![valorant];

        let apps = vec![test_app("Valorant", "/games/valorant", true)];
        assert_eq!(active_profile_for(&config, &apps, &[]), None);
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
        assert_eq!(active_profile_for(&config, &apps, &[]), Some(valorant));

        let apps = vec![test_app("Chrome", "/apps/chrome", true)];
        assert_eq!(
            active_profile_for(&config, &apps, &[])
                .unwrap()
                .application
                .name,
            "Default"
        );

        let apps = vec![test_app("Chrome", "/apps/chrome", false)];
        assert_eq!(
            active_profile_for(&config, &apps, &[])
                .unwrap()
                .application
                .name,
            "Default"
        );
    }

    #[test]
    fn running_game_keeps_its_profile_while_another_app_is_in_front() {
        let mut cs2 = test_profile("Counter-Strike 2", "", Some(1600), None);
        cs2.application.executable = "cs2.exe".into();
        let config = BridgeConfig {
            games: vec![GameConfig {
                name: "Counter-Strike 2".into(),
                executables: vec!["cs2".into(), "cs2.exe".into()],
            }],
            profiles: vec![cs2.clone()],
            default_profile: Some(test_profile("Default", "", Some(800), None)),
            ..BridgeConfig::default()
        };
        let running = ["Counter-Strike 2".to_owned()];

        let browser = [test_app("Chrome", "/apps/chrome", true)];
        assert_eq!(active_profile_for(&config, &browser, &running), Some(cs2));
        assert_eq!(
            active_profile_for(&config, &browser, &[])
                .unwrap()
                .application
                .name,
            "Default"
        );
    }

    #[test]
    fn switch_messages_name_the_game_and_the_return_to_default() {
        let default = test_profile("PRO X SUPERLIGHT 2c", "", Some(800), Some(1000));
        let game = test_profile("Counter-Strike 2", "", Some(1600), None);
        let applied = Some(ApplyOutcome::Applied);

        assert_eq!(
            switch_message(Some(&default), Some(&game), Some(&default), applied, false),
            Some(ProfileSwitch {
                title: "Counter-Strike 2".into(),
                detail: "Profile on · 1600 DPI".into(),
            })
        );
        assert_eq!(
            switch_message(Some(&game), Some(&default), Some(&default), applied, false),
            Some(ProfileSwitch {
                title: "Default profile".into(),
                detail: "Counter-Strike 2 closed · 800 DPI · 1000 Hz".into(),
            })
        );
        assert_eq!(
            switch_message(Some(&game), None, None, None, false),
            Some(ProfileSwitch {
                title: "Counter-Strike 2 closed".into(),
                detail: "Profile off".into(),
            })
        );
        assert_eq!(
            switch_message(
                Some(&default),
                Some(&default),
                Some(&default),
                applied,
                false
            ),
            None
        );
    }

    #[test]
    fn switch_messages_mention_an_unreachable_mouse_only_without_the_panel() {
        let game = test_profile("Counter-Strike 2", "", Some(1600), None);
        let failed = Some(ApplyOutcome::Failed);
        assert_eq!(
            switch_message(None, Some(&game), None, failed, false)
                .unwrap()
                .detail,
            "Couldn't reach the mouse — open OpenMouse to apply"
        );
        assert_eq!(
            switch_message(None, Some(&game), None, failed, true)
                .unwrap()
                .detail,
            "Profile on · 1600 DPI"
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
