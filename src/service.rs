use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
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

/// The running applications that are registered games. Profile matching only
/// looks at these; the unfiltered list feeds the game-request picker.
fn registered_applications(
    running: &[ApplicationInfo],
    games: &[GameConfig],
) -> Vec<ApplicationInfo> {
    running
        .iter()
        .filter(|application| is_registered_game(application, games))
        .cloned()
        .collect()
}

/// Drops cached icons for applications that are no longer running, so the
/// cache stays the size of the running list instead of every app ever seen.
fn prune_application_icons(
    icons: &mut HashMap<String, Option<Vec<u8>>>,
    running: &[ApplicationInfo],
) {
    icons.retain(|icon_id, _| {
        running
            .iter()
            .any(|application| &application.icon_id == icon_id)
    });
}

/// How long a new active profile must persist before a switch notification
/// fires, so rapid alt-tabbing does not spam notifications.
const PROFILE_DEBOUNCE: Duration = Duration::from_millis(2500);

/// How often Bridge reads mouse batteries itself, so low-battery alerts work
/// without the control panel open. Each read spawns the native-hid helper.
const BATTERY_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// A reading older than this is shown as stale: more than two missed polls.
const BATTERY_STALE_AFTER: Duration = Duration::from_secs(12 * 60);

/// A reading this old belongs to a mouse that is gone, not just asleep: it is
/// dropped from the list instead of lingering as a ghost device.
const BATTERY_EVICT_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Drops battery entries that can no longer describe a live mouse: anything
/// past `BATTERY_EVICT_AFTER`, and anything outside the saved profiles that
/// has also gone stale (a renamed or removed device, or an id a client pushed
/// once and never again).
fn prune_batteries(battery: &mut HashMap<String, BatteryState>, known: &[(String, String)]) {
    battery.retain(|device_id, entry| {
        let age = entry.updated_at.elapsed();
        age < BATTERY_EVICT_AFTER
            && (age < BATTERY_STALE_AFTER || known.iter().any(|(id, _)| id == device_id))
    });
}

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
            let game = config
                .tracked_games()
                .into_iter()
                .find(|game| &game.name == name)?;
            enabled().find(|profile| profile_matches_game(profile, &game))
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

const MAX_IMAGE_BYTES: usize = 512 * 1024;

fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    crate::remote_image::image_type(bytes).filter(|kind| *kind != "image/gif")
}

fn game_image_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("custom-game-images")
}

fn game_image_file(dir: &Path, name: &str) -> PathBuf {
    // FNV-1a: stable across runs, unlike the standard hasher.
    let hash = name
        .to_ascii_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    dir.join(format!("{hash:016x}.img"))
}

fn prune_game_images(dir: &Path, keep: &[&str]) {
    let wanted: HashSet<PathBuf> = keep.iter().map(|name| game_image_file(dir, name)).collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "img") && !wanted.contains(&path) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The profile `key` names as it is in the config now, and whether it is the
/// default profile. A profile keeps its key while its settings are edited, so
/// the copy Bridge applied earlier can be out of date.
fn current_profile<'a>(
    config: &'a BridgeConfig,
    key: &str,
) -> Option<(&'a ApplicationProfile, bool)> {
    // Only an enabled profile is ever the active one, so a disabled copy of the same game is not it.
    config
        .profiles
        .iter()
        .find(|profile| profile.enabled && profile_key(profile) == key)
        .map(|profile| (profile, false))
        .or_else(|| {
            config
                .default_profile
                .as_ref()
                .filter(|profile| profile_key(profile) == key)
                .map(|profile| (profile, true))
        })
}

/// Something worth telling the user about, already worded: a title such as the
/// game's name and a short detail line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub detail: String,
}

/// Receives notices instead of the platform notification, e.g. the desktop
/// app's on-screen overlay.
pub type NoticeListener = Arc<dyn Fn(Notice) + Send + Sync>;

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
) -> Option<Notice> {
    let is_default = |profile: &ApplicationProfile| default == Some(profile);
    let left = previous.filter(|profile| !is_default(profile));
    let mut switch = match current {
        Some(profile) if !is_default(profile) => Notice {
            title: profile.application.name.clone(),
            detail: with_settings("Profile on".into(), profile),
        },
        Some(profile) => Notice {
            title: "Default profile".into(),
            detail: with_settings(format!("{} closed", left?.application.name), profile),
        },
        None => Notice {
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
    notice_listener: Arc<std::sync::Mutex<Option<NoticeListener>>>,
    show_panel: Arc<std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
    buttons: Arc<crate::buttons::ButtonListener>,
}

struct BridgeState {
    config: BridgeConfig,
    active_games: Vec<String>,
    applications: Vec<ApplicationInfo>,
    // Every visible application, registered game or not — what a control
    // panel offers when someone requests a new game or app be added.
    running_applications: Vec<ApplicationInfo>,
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

/// A mouse the control panel currently holds open over native HID, most
/// recently opened first. Unlike the battery list — which accumulates every
/// device that ever reported a charge level — this reflects the live HID
/// session, so the status view can follow device switches (including wired
/// mice that never report battery).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveDevice {
    pub key: String,
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
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
    pub notification_sound: bool,
    pub notification_volume: u8,
    pub foreground_application: Option<ApplicationInfo>,
    pub active_profile: Option<ApplicationProfile>,
    pub active_profile_is_default: bool,
    pub visible_application_count: usize,
    pub profile_count: usize,
    pub client_connected: bool,
    pub active_devices: Vec<ActiveDevice>,
    pub batteries: Vec<DeviceBattery>,
}

impl BridgeService {
    pub fn new(config: BridgeConfig, config_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(RwLock::new(BridgeState {
                config,
                active_games: Vec::new(),
                applications: Vec::new(),
                running_applications: Vec::new(),
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
            notice_listener: Arc::default(),
            show_panel: Arc::default(),
            buttons: Arc::default(),
        }
    }

    /// Opens the tray panel. A second Bridge launch calls this over localhost
    /// when it finds the port already taken.
    pub fn on_show_panel(&self, listener: impl Fn() + Send + Sync + 'static) {
        *self
            .show_panel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(listener));
    }

    pub fn request_show_panel(&self) {
        let listener = self
            .show_panel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(listener) = listener {
            listener();
        }
    }

    pub async fn acknowledge_tray_hint(&self) -> Result<()> {
        let config = {
            let mut state = self.inner.write().await;
            if state.config.tray_hint_shown {
                return Ok(());
            }
            state.config.tray_hint_shown = true;
            state.config.clone()
        };
        config::save(&self.config_path, &config)
    }

    /// Sends profile switches and low-battery alerts to `listener` instead of
    /// showing a platform notification for them.
    pub fn on_notice(&self, listener: impl Fn(Notice) + Send + Sync + 'static) {
        *self
            .notice_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(listener));
    }

    /// Hands `notice` to the listener, or shows it as a platform notification
    /// when there is none. Showing a notification can block (e.g. an unbundled
    /// macOS binary pops a chooser dialog), so that runs detached: never stall
    /// the caller, and never let a notification failure fail it.
    fn announce(&self, notice: Notice) {
        let listener = self
            .notice_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match listener {
            Some(listener) => listener(notice),
            None => {
                std::thread::spawn(move || {
                    if let Err(error) = platform::notify(&notice.title, &notice.detail) {
                        tracing::warn!(%error, "Could not show the notification");
                    }
                });
            }
        }
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
        let tracked_games = state.config.tracked_games();
        let active_profile =
            active_profile_for(&state.config, &state.applications, &state.active_games);
        let active_profile_is_default =
            active_profile.is_some() && active_profile == state.config.default_profile;
        let mut batteries = state
            .battery
            .iter()
            .filter(|(_, entry)| entry.updated_at.elapsed() < BATTERY_EVICT_AFTER)
            .map(|(device_id, entry)| DeviceBattery {
                device_id: device_id.clone(),
                device_name: entry.device_name.clone(),
                percent: entry.percent,
                charging: entry.charging,
                stale: entry.updated_at.elapsed() >= BATTERY_STALE_AFTER,
            })
            .collect::<Vec<_>>();
        batteries.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        let active_devices = crate::hid::active_devices()
            .into_iter()
            .map(|device| ActiveDevice {
                key: device.key,
                name: device.name,
                vendor_id: device.vendor_id,
                product_id: device.product_id,
            })
            .collect::<Vec<_>>();
        BridgeSnapshot {
            version: crate::BRIDGE_VERSION,
            platform: platform::platform_name(),
            linux_distribution: platform::linux_distribution(),
            uptime_seconds: state.started_at.elapsed().as_secs(),
            active_games: state.active_games.clone(),
            games: tracked_games
                .iter()
                .map(|game| GameActivity {
                    name: game.name.clone(),
                    active: state.active_games.iter().any(|active| active == &game.name),
                })
                .collect(),
            tracked_game_count: tracked_games.len(),
            battery_threshold_percent: state.config.battery_threshold_percent,
            autostart_enabled: platform::autostart_enabled(),
            automatic_updates: state.config.automatic_updates,
            notification_sound: state.config.notification_sound,
            notification_volume: state.config.notification_volume,
            foreground_application,
            active_profile,
            active_profile_is_default,
            visible_application_count: state.applications.len(),
            profile_count: state.config.profiles.len(),
            client_connected,
            active_devices,
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

    pub async fn running_applications(&self) -> Vec<ApplicationInfo> {
        self.inner.read().await.running_applications.clone()
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
        self.inner.read().await.config.tracked_games()
    }

    pub async fn custom_games(&self) -> Vec<GameConfig> {
        self.inner.read().await.config.custom_games.clone()
    }

    pub async fn set_custom_games(&self, games: Vec<GameConfig>) -> Result<Vec<GameConfig>> {
        let config = {
            let mut state = self.inner.write().await;
            state.config.custom_games = games;
            state.config = state.config.clone().normalized();
            state.config.clone()
        };
        config::save(&self.config_path, &config)?;
        let names: Vec<&str> = config
            .custom_games
            .iter()
            .map(|game| game.name.as_str())
            .collect();
        prune_game_images(&game_image_dir(&self.config_path), &names);
        Ok(config.custom_games)
    }

    pub async fn custom_game_image(&self, name: &str) -> Option<(Vec<u8>, &'static str)> {
        let bytes = tokio::fs::read(game_image_file(&game_image_dir(&self.config_path), name))
            .await
            .ok()?;
        let kind = image_type(&bytes)?;
        Some((bytes, kind))
    }

    pub async fn set_custom_game_image(&self, name: &str, image: &[u8]) -> Result<()> {
        let known = self
            .inner
            .read()
            .await
            .config
            .custom_games
            .iter()
            .any(|game| game.name.eq_ignore_ascii_case(name));
        anyhow::ensure!(known, "{name} is not a custom game");
        anyhow::ensure!(
            image_type(image).is_some(),
            "the image must be a PNG, JPEG or WebP of at most {MAX_IMAGE_BYTES} bytes"
        );
        let dir = game_image_dir(&self.config_path);
        tokio::fs::create_dir_all(&dir)
            .await
            .context("could not create the image folder")?;
        tokio::fs::write(game_image_file(&dir, name), image)
            .await
            .context("could not save the image")
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

    pub async fn default_profile(&self) -> Option<ApplicationProfile> {
        self.inner.read().await.config.default_profile.clone()
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

    pub async fn set_notification_sound(&self, enabled: bool) -> Result<()> {
        let config = {
            let mut state = self.inner.write().await;
            state.config.notification_sound = enabled;
            state.config.clone()
        };
        config::save(&self.config_path, &config)
    }

    pub async fn set_notification_volume(&self, percent: u8) -> Result<()> {
        let config = {
            let mut state = self.inner.write().await;
            state.config.notification_volume = percent;
            state.config = state.config.clone().normalized();
            state.config.clone()
        };
        config::save(&self.config_path, &config)
    }

    /// Forgets a lingering battery entry so a dead device leaves the list.
    /// Returns whether an entry was removed.
    pub async fn forget_battery(&self, device_id: &str) -> bool {
        self.inner.write().await.battery.remove(device_id).is_some()
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
            self.announce(Notice {
                title: "Mouse battery is low".into(),
                detail: format!(
                    "{} has {}% battery remaining",
                    reading.device_name, reading.percent
                ),
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
                let devices = profile_devices(&service.inner.read().await.config);
                // Pruning is in-memory only, so it runs even while the
                // control panel holds a HID session and reads are paused.
                prune_batteries(&mut service.inner.write().await.battery, &devices);
                if crate::hid::client_session_active() {
                    continue;
                }
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
                let games = service.inner.read().await.config.tracked_games();
                let active = detector.detect(&games);
                let running = applications::visible_applications();
                let applications = registered_applications(&running, &games);
                // Icon extraction can be disabled by callers that never serve
                // the application's icon endpoint.
                let icons = if extract_icons.load(Ordering::Acquire) {
                    let missing_icons = {
                        let state = service.inner.read().await;
                        running
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
                prune_application_icons(&mut state.application_icons, &running);
                state.running_applications = running;

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
                // Read from the config each time: editing the active profile's
                // keys does not change its key, so the applied copy goes stale.
                let desired_buttons = state
                    .active_profile_key
                    .as_deref()
                    .and_then(|key| current_profile(&state.config, key))
                    .and_then(|(profile, is_default)| {
                        crate::buttons::Assignments::from_profile(profile, is_default)
                    });
                drop(state);
                // A control panel holding the mouse is not interrupted by a new helper.
                service
                    .buttons
                    .update(desired_buttons, !crate::hid::client_session_active());
                if let Some((current, previous, announce)) = switch {
                    let service = service.clone();
                    std::thread::spawn(move || {
                        // A connected control panel applies the profile over its
                        // own HID session; writing natively as well would
                        // interleave two writers on the same mouse.
                        let panel_connected = crate::hid::client_session_active();
                        let outcome = if panel_connected {
                            None
                        } else {
                            current.as_ref().map(apply_natively)
                        };
                        if !announce {
                            return;
                        }
                        let Some(message) = switch_message(
                            previous.as_ref(),
                            current.as_ref(),
                            default.as_ref(),
                            outcome,
                            panel_connected,
                        ) else {
                            return;
                        };
                        service.announce(message);
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
    use crate::remote_image::PNG_SIGNATURE;

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
    fn a_game_image_is_kept_under_a_stable_name_whatever_the_case() {
        let dir = Path::new("icons");
        assert_eq!(
            game_image_file(dir, "My Game"),
            game_image_file(dir, "my game")
        );
        assert_ne!(
            game_image_file(dir, "My Game"),
            game_image_file(dir, "Other")
        );
        let file = game_image_file(dir, "../../evil\\name");
        assert_eq!(
            file.parent(),
            Some(dir),
            "a hostile name cannot leave the image folder"
        );
    }

    #[test]
    fn only_a_png_jpeg_or_webp_of_a_sensible_size_is_kept_as_an_image() {
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend_from_slice(&[0; 32]);
        assert_eq!(image_type(&png), Some("image/png"));
        assert_eq!(
            image_type(&[0xff, 0xd8, 0xff, 0xe0, 0, 0]),
            Some("image/jpeg")
        );
        let mut webp = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
        webp.extend_from_slice(&[0; 8]);
        assert_eq!(image_type(&webp), Some("image/webp"));
        assert_eq!(image_type(b"not an image"), None);
        assert_eq!(image_type(b"GIF89a......"), None, "a GIF is not accepted");
        png.resize(MAX_IMAGE_BYTES + 1, 0);
        assert_eq!(image_type(&png), None, "an oversized file is refused");
    }

    #[test]
    fn removing_a_game_removes_its_image() {
        let dir = std::env::temp_dir().join(format!("bridge-icons-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        for name in ["Kept", "Gone"] {
            std::fs::write(game_image_file(&dir, name), PNG_SIGNATURE).expect("image");
        }
        prune_game_images(&dir, &["Kept"]);
        assert!(game_image_file(&dir, "Kept").exists());
        assert!(!game_image_file(&dir, "Gone").exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_active_profile_is_read_from_the_config_so_edits_are_seen() {
        let mut config = BridgeConfig {
            default_profile: Some(test_profile("Default", "", Some(800), None)),
            profiles: vec![test_profile("Valorant", "/games/valorant", Some(800), None)],
            ..BridgeConfig::default()
        };

        let (game, is_default) =
            current_profile(&config, "/games/valorant").expect("the game profile");
        assert_eq!(
            (game.application.name.as_str(), is_default),
            ("Valorant", false)
        );
        let (default, is_default) =
            current_profile(&config, "default").expect("the default profile");
        assert_eq!(
            (default.application.name.as_str(), is_default),
            ("Default", true)
        );

        // Editing a profile keeps its key, so the lookup finds the new settings.
        config.default_profile = Some(test_profile("Default", "", Some(1600), None));
        let (edited, _) = current_profile(&config, "default").expect("the edited default profile");
        assert_eq!(edited.settings.dpi, Some(1600));
        assert!(current_profile(&config, "/games/other").is_none());

        // A disabled profile of the same game is not the one that is applied.
        let mut disabled = test_profile("Valorant", "/games/valorant", Some(400), None);
        disabled.enabled = false;
        config.profiles = vec![disabled];
        assert!(current_profile(&config, "/games/valorant").is_none());
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
            Some(Notice {
                title: "Counter-Strike 2".into(),
                detail: "Profile on · 1600 DPI".into(),
            })
        );
        assert_eq!(
            switch_message(Some(&game), Some(&default), Some(&default), applied, false),
            Some(Notice {
                title: "Default profile".into(),
                detail: "Counter-Strike 2 closed · 800 DPI · 1000 Hz".into(),
            })
        );
        assert_eq!(
            switch_message(Some(&game), None, None, None, false),
            Some(Notice {
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

    fn battery_entry(age: Duration) -> BatteryState {
        BatteryState {
            last_alert: None,
            device_name: "Mouse".into(),
            percent: 50,
            charging: false,
            updated_at: Instant::now() - age,
        }
    }

    #[test]
    fn prune_evicts_old_entries_and_unknown_stale_ones() {
        let mut battery = HashMap::new();
        battery.insert("fresh-unknown".to_string(), battery_entry(Duration::ZERO));
        battery.insert(
            "stale-known".to_string(),
            battery_entry(BATTERY_STALE_AFTER),
        );
        battery.insert(
            "stale-unknown".to_string(),
            battery_entry(BATTERY_STALE_AFTER),
        );
        battery.insert(
            "ancient-known".to_string(),
            battery_entry(BATTERY_EVICT_AFTER),
        );
        let known = vec![
            ("stale-known".to_string(), "Mouse".to_string()),
            ("ancient-known".to_string(), "Mouse".to_string()),
        ];

        prune_batteries(&mut battery, &known);

        let mut kept = battery.keys().cloned().collect::<Vec<_>>();
        kept.sort();
        assert_eq!(kept, ["fresh-unknown", "stale-known"]);
    }

    #[tokio::test]
    async fn forgetting_a_battery_removes_only_that_device() {
        let bridge = service(BridgeConfig::default());
        for id in ["a", "b"] {
            bridge
                .record_battery(BatteryReading {
                    device_id: id.into(),
                    device_name: id.into(),
                    percent: 80,
                    charging: false,
                })
                .await
                .unwrap();
        }
        assert!(bridge.forget_battery("a").await);
        assert!(!bridge.forget_battery("a").await);
        let ids = bridge
            .snapshot()
            .await
            .batteries
            .into_iter()
            .map(|battery| battery.device_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["b"]);
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

    #[test]
    fn running_applications_include_unregistered_apps() {
        let games = vec![GameConfig {
            name: "Counter-Strike 2".into(),
            executables: vec!["cs2.exe".into()],
        }];
        let application = |name: &str, executable: &str| ApplicationInfo {
            name: name.into(),
            executable: executable.into(),
            path: executable.into(),
            foreground: false,
            icon_id: executable.into(),
        };
        let running = vec![
            application("Counter-Strike 2", "cs2.exe"),
            application("Some New Game", "newgame.exe"),
        ];

        let registered = registered_applications(&running, &games);

        assert_eq!(registered, vec![running[0].clone()]);
        assert!(running.iter().any(|app| app.executable == "newgame.exe"));
        assert!(!registered.iter().any(|app| app.executable == "newgame.exe"));
    }

    #[test]
    fn application_icons_are_pruned_to_running_applications() {
        let running = vec![ApplicationInfo {
            name: "Some New Game".into(),
            executable: "newgame.exe".into(),
            path: "newgame.exe".into(),
            foreground: true,
            icon_id: "running".into(),
        }];
        let mut icons = HashMap::from([
            ("running".to_string(), Some(vec![1])),
            ("closed".to_string(), None),
        ]);

        prune_application_icons(&mut icons, &running);

        assert_eq!(icons.len(), 1);
        assert!(icons.contains_key("running"));
    }

    #[tokio::test]
    async fn client_is_connected_only_after_a_heartbeat() {
        let bridge = service(BridgeConfig::default());
        assert!(!bridge.snapshot().await.client_connected);

        bridge.record_client_heartbeat().await;

        assert!(bridge.snapshot().await.client_connected);
    }
}
