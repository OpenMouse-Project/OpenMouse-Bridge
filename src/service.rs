use std::{
    collections::{HashMap, HashSet, VecDeque},
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
    config::{self, ApplicationProfile, BridgeConfig, GameConfig, LocalApplication},
    drivers,
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
#[cfg(test)]
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

fn profile_applications(
    running: &[ApplicationInfo],
    games: &[GameConfig],
    local_applications: &[LocalApplication],
) -> Vec<ApplicationInfo> {
    running
        .iter()
        .filter(|application| {
            is_registered_game(application, games)
                || local_applications.iter().any(|target| {
                    crate::games::matches_target(
                        &target.path,
                        &target.executable,
                        &application.path,
                        &application.executable,
                    )
                })
        })
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

/// A reading this old is shown as stale: more than two missed polls.
const BATTERY_STALE_AFTER: Duration = Duration::from_secs(12 * 60);

/// A reading this old belongs to a mouse that is gone, not just asleep: it is
/// dropped from the list instead of lingering as a ghost device.
const BATTERY_EVICT_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// How many samples of each device's percent history are kept for the tray
/// tooltip's time-remaining estimate.
const BATTERY_HISTORY_SAMPLES: usize = 48;

/// Percent samples older than this no longer count toward a trend, so a
/// mouse's days-old readings never poison the estimate.
const BATTERY_TREND_WINDOW: Duration = Duration::from_secs(6 * 60 * 60);

/// A trend needs at least this much history before an estimate is worth
/// showing: two samples 5 minutes apart are mostly quantization noise.
const BATTERY_MIN_TREND_SPAN: Duration = Duration::from_secs(30 * 60);

/// The smallest drift (percent per minute) that counts as a trend, about 1%
/// per 3 hours — the pace a mouse battery actually moves at.
const BATTERY_TREND_EPSILON: f64 = 0.005;

/// Estimates beyond this are noise for a mouse battery, not a plan.
const BATTERY_MAX_ESTIMATE_MINUTES: u32 = 48 * 60;

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
#[cfg(test)]
fn active_profile_for(
    config: &BridgeConfig,
    applications: &[ApplicationInfo],
    active_games: &[String],
) -> Option<ApplicationProfile> {
    resolve_profile(config, applications, active_games, &[])
}

#[cfg(test)]
fn resolve_profile(
    config: &BridgeConfig,
    applications: &[ApplicationInfo],
    active_games: &[String],
    processes: &[crate::games::RunningProcess],
) -> Option<ApplicationProfile> {
    resolve_profile_retaining(config, applications, active_games, processes, None)
}

fn resolve_profile_retaining(
    config: &BridgeConfig,
    applications: &[ApplicationInfo],
    active_games: &[String],
    processes: &[crate::games::RunningProcess],
    current: Option<&ApplicationProfile>,
) -> Option<ApplicationProfile> {
    // The catalog plus the user's custom games, so a custom game's profile activates too.
    let tracked_games = config.tracked_games();
    let eligible = |profile: &&ApplicationProfile| {
        if !profile.enabled {
            return false;
        }
        if let Some(id) = &profile.application.local_id {
            return config
                .local_applications
                .iter()
                .find(|target| &target.id == id)
                .is_some_and(|target| {
                    if target.path.is_empty() {
                        return false;
                    }
                    match target.effective_activation() {
                        config::LocalApplicationActivation::Running => {
                            processes.iter().any(|process| {
                                crate::games::matches_target(
                                    &target.path,
                                    &target.executable,
                                    &process.path,
                                    &process.executable,
                                )
                            })
                        }
                        config::LocalApplicationActivation::Focused => {
                            applications.iter().any(|application| {
                                application.foreground
                                    && crate::games::matches_target(
                                        &target.path,
                                        &target.executable,
                                        &application.path,
                                        &application.executable,
                                    )
                            })
                        }
                    }
                });
        }
        let application = &profile.application;
        let focused = applications.iter().any(|running| {
            running.foreground
                && if !application.path.is_empty() {
                    crate::games::matches_target(
                        &application.path,
                        &application.executable,
                        &running.path,
                        &running.executable,
                    )
                } else if !application.executable.is_empty() {
                    application
                        .executable
                        .eq_ignore_ascii_case(&running.executable)
                } else {
                    !application.name.is_empty()
                        && application.name.eq_ignore_ascii_case(&running.name)
                }
        });
        focused
            || tracked_games.iter().any(|game| {
                active_games.contains(&game.name)
                    && if application.path.is_empty() {
                        profile_matches_game(profile, game)
                    } else {
                        processes.iter().any(|process| {
                            crate::games::matches_target(
                                &application.path,
                                &application.executable,
                                &process.path,
                                &process.executable,
                            ) && game.executables.iter().any(|executable| {
                                executable.eq_ignore_ascii_case(&process.executable)
                            })
                        })
                    }
            })
    };
    let retained = current.and_then(|current| {
        config.profiles.iter().filter(eligible).find(|profile| {
            profile.application_key() == current.application_key()
                && profile.device.id == current.device.id
        })
    });
    retained
        .or_else(|| config.profiles.iter().find(eligible))
        .cloned()
        .or_else(|| {
            config
                .default_profile
                .clone()
                .filter(|profile| profile.enabled)
        })
}

fn profile_matches_game(profile: &ApplicationProfile, game: &GameConfig) -> bool {
    let application = &profile.application;
    if !application.path.is_empty() || application.local_id.is_some() {
        return false;
    }
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
    let application = if let Some(id) = &profile.application.local_id {
        id.clone()
    } else if profile.application.path.is_empty() {
        format!("preset:{}", profile.application.name.to_ascii_lowercase())
    } else {
        format!("path:{}", crate::games::path_key(&profile.application.path))
    };
    format!("{}:{}{}", application.len(), application, profile.device.id)
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
    if !panel_connected {
        match outcome {
            Some(ApplyOutcome::Failed) => {
                switch.detail = if current.is_none() {
                    "Couldn't restore original mouse settings — check Bridge status".into()
                } else {
                    "Couldn't reach the mouse — open OpenMouse to apply".into()
                };
            }
            Some(ApplyOutcome::NoDriver) => {
                switch.detail =
                    "Native automation unsupported — acknowledged device readback is required"
                        .into();
            }
            _ => {}
        }
    }
    Some(switch)
}

type PanelListener = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct BridgeService {
    inner: Arc<RwLock<BridgeState>>,
    config_path: Arc<PathBuf>,
    notice_listener: Arc<std::sync::Mutex<Option<NoticeListener>>>,
    show_panel: Arc<std::sync::Mutex<Option<PanelListener>>>,
    buttons: Arc<crate::buttons::ButtonListener>,
    native_automation: Arc<std::sync::Mutex<crate::drivers::automation::NativeAutomation>>,
}

/// A control panel is connected when it holds a HID WebSocket session or
/// handshook within the last few seconds.
fn panel_connected(state: &BridgeState) -> bool {
    crate::hid::client_session_active()
        || state
            .last_client_heartbeat
            .is_some_and(|heartbeat| heartbeat.elapsed() < Duration::from_secs(20))
}

struct BridgeState {
    config: BridgeConfig,
    active_games: Vec<String>,
    processes: Vec<crate::games::RunningProcess>,
    applications: Vec<ApplicationInfo>,
    // Every visible application, registered game or not — what a control
    // panel offers when someone requests a new game or app be added.
    running_applications: Vec<ApplicationInfo>,
    application_icons: HashMap<String, Option<Vec<u8>>>,
    battery: HashMap<String, BatteryState>,
    // Debounced active-profile tracking for switch notifications.
    active_profile_key: Option<String>,
    applied_profile: Option<ApplicationProfile>,
    native_automation_error: Option<String>,
    pending_native_restores: usize,
    pending_profile: Option<(Option<String>, Instant)>,
    profile_seeded: bool,
    started_at: Instant,
    last_client_heartbeat: Option<Instant>,
    /// Who currently drives the mouse: Bridge in onboard memory mode or the
    /// web panel in live mode. Tracked by the mode monitor.
    device_mode: DeviceMode,
}

/// Who currently drives the mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceMode {
    /// Bridge drives the mouse itself: the active profile's stored settings
    /// live in the onboard memory and the battery is read natively.
    Onboard,
    /// The OpenMouse web panel holds the HID session and drives the mouse.
    Live,
}

impl DeviceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Onboard => "onboard",
            Self::Live => "live",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatteryReading {
    pub device_id: String,
    pub device_name: String,
    pub percent: u8,
    #[serde(default)]
    pub charging: bool,
    /// The settings currently in the mouse's onboard memory, when the
    /// reader could see them — native reads report them; a control panel's
    /// `PUT /v1/battery` may not.
    #[serde(default)]
    pub dpi: Option<u32>,
    #[serde(default)]
    pub polling_rate_hz: Option<u32>,
    #[serde(default)]
    pub switch_mode: Option<String>,
}

struct BatteryState {
    last_alert: Option<Instant>,
    device_name: String,
    percent: u8,
    charging: bool,
    dpi: Option<u32>,
    polling_rate_hz: Option<u32>,
    switch_mode: Option<String>,
    updated_at: Instant,
    // Recent (reading time, percent) samples, oldest first, for the tray
    // tooltip's time-remaining estimate.
    history: VecDeque<(Instant, u8)>,
}

/// Estimates the minutes left until the battery is empty (discharging) or
/// full (charging) from the percent history: the level's drift across the
/// samples inside the trend window. `None` while there is no confident
/// trend yet — too little history, or the level is holding still.
fn estimate_minutes_remaining(
    history: &VecDeque<(Instant, u8)>,
    percent: u8,
    charging: bool,
) -> Option<u32> {
    let now = Instant::now();
    let recent = |at: &Instant| now.duration_since(*at) <= BATTERY_TREND_WINDOW;
    let (first_at, first_percent) = history.iter().find(|(at, _)| recent(at))?;
    let (last_at, last_percent) = history.iter().rev().find(|(at, _)| recent(at))?;
    if last_at <= first_at {
        return None;
    }
    let span = last_at.duration_since(*first_at);
    if span < BATTERY_MIN_TREND_SPAN {
        return None;
    }
    let span_minutes = span.as_secs_f64() / 60.0;
    let rate = f64::from(*last_percent as i16 - *first_percent as i16) / span_minutes;
    let raw_minutes = if charging {
        (rate > BATTERY_TREND_EPSILON).then(|| f64::from(100 - percent) / rate)?
    } else {
        (rate < -BATTERY_TREND_EPSILON).then(|| f64::from(percent) / -rate)?
    };
    // Round to 5-minute granularity so the estimate holds still instead of
    // jittering every snapshot.
    let minutes = (raw_minutes / 5.0).round() as u32 * 5;
    (minutes > 0 && minutes <= BATTERY_MAX_ESTIMATE_MINUTES).then_some(minutes)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceBattery {
    pub device_id: String,
    pub device_name: String,
    pub percent: u8,
    pub charging: bool,
    pub stale: bool,
    /// Minutes until the battery is empty (discharging) or full (charging),
    /// from the recent trend; `None` while there is no confident estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minutes_remaining: Option<u32>,
    /// The settings currently in the mouse's onboard memory, from the most
    /// recent native status read; `None` when unreadable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpi: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polling_rate_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_mode: Option<String>,
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
    pub tray_battery_style: String,
    pub hybrid_mode: bool,
    /// Who drives the mouse right now: `"onboard"` (Bridge applies the
    /// stored settings and reads the battery) or `"live"` (the web panel
    /// holds the HID session).
    pub device_mode: String,
    pub autostart_enabled: bool,
    pub automatic_updates: bool,
    pub notification_sound: bool,
    pub notification_volume: u8,
    pub foreground_application: Option<ApplicationInfo>,
    pub active_profile: Option<ApplicationProfile>,
    pub active_profile_is_default: bool,
    pub native_automation_error: Option<String>,
    pub pending_native_restores: usize,
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
                processes: Vec::new(),
                applications: Vec::new(),
                running_applications: Vec::new(),
                application_icons: HashMap::new(),
                battery: HashMap::new(),
                active_profile_key: None,
                applied_profile: None,
                native_automation_error: None,
                pending_native_restores: 0,
                pending_profile: None,
                profile_seeded: false,
                started_at: Instant::now(),
                last_client_heartbeat: None,
                device_mode: DeviceMode::Onboard,
            })),
            config_path: Arc::new(config_path),
            notice_listener: Arc::default(),
            show_panel: Arc::default(),
            buttons: Arc::default(),
            native_automation: Arc::default(),
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
        if self.inner.read().await.config.tray_hint_shown {
            return Ok(());
        }
        self.update_config(|config| {
            config.tray_hint_shown = true;
            Ok(())
        })
        .await
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
        let client_connected = panel_connected(&state);
        let device_mode = state.device_mode;
        let foreground_application = state
            .applications
            .iter()
            .find(|application| application.foreground)
            .cloned();
        let tracked_games = state.config.tracked_games();
        let active_profile = resolve_profile_retaining(
            &state.config,
            &state.applications,
            &state.active_games,
            &state.processes,
            state.applied_profile.as_ref(),
        );
        let active_profile_is_default =
            active_profile.is_some() && active_profile == state.config.default_profile;
        let mut batteries = state
            .battery
            .iter()
            .filter(|(_, entry)| entry.updated_at.elapsed() < BATTERY_EVICT_AFTER)
            .map(|(device_id, entry)| {
                let stale = entry.updated_at.elapsed() >= BATTERY_STALE_AFTER;
                DeviceBattery {
                    device_id: device_id.clone(),
                    device_name: entry.device_name.clone(),
                    percent: entry.percent,
                    charging: entry.charging,
                    minutes_remaining: if stale {
                        None
                    } else {
                        estimate_minutes_remaining(&entry.history, entry.percent, entry.charging)
                    },
                    dpi: entry.dpi,
                    polling_rate_hz: entry.polling_rate_hz,
                    switch_mode: entry.switch_mode.clone(),
                    stale,
                }
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
            active_games: state
                .active_games
                .iter()
                .filter_map(|active| {
                    state
                        .config
                        .local_applications
                        .iter()
                        .find(|target| &target.id == active)
                        .map(|target| target.name.clone())
                        .or_else(|| {
                            tracked_games
                                .iter()
                                .find(|game| &game.name == active)
                                .map(|game| game.name.clone())
                        })
                })
                .collect(),
            games: tracked_games
                .iter()
                .map(|game| GameActivity {
                    name: game.name.clone(),
                    active: state.active_games.iter().any(|active| active == &game.name),
                })
                .chain(
                    state
                        .config
                        .local_applications
                        .iter()
                        .filter(|target| target.kind == config::LocalApplicationKind::Game)
                        .map(|target| GameActivity {
                            name: target.name.clone(),
                            active: state.active_games.contains(&target.id),
                        }),
                )
                .collect(),
            tracked_game_count: tracked_games.len()
                + state
                    .config
                    .local_applications
                    .iter()
                    .filter(|target| target.kind == config::LocalApplicationKind::Game)
                    .count(),
            battery_threshold_percent: state.config.battery_threshold_percent,
            tray_battery_style: state.config.tray_battery_style.clone(),
            hybrid_mode: state.config.hybrid_mode,
            device_mode: device_mode.as_str().to_owned(),
            autostart_enabled: platform::autostart_enabled(),
            automatic_updates: state.config.automatic_updates,
            notification_sound: state.config.notification_sound,
            notification_volume: state.config.notification_volume,
            foreground_application,
            active_profile,
            active_profile_is_default,
            native_automation_error: state.native_automation_error.clone(),
            pending_native_restores: state.pending_native_restores,
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

    pub async fn games(&self) -> Vec<serde_json::Value> {
        let state = self.inner.read().await;
        let tracked = state.config.tracked_games();
        tracked.iter().map(|game| {
                let custom = !state.config.games.iter().any(|preset| preset.name == game.name);
                serde_json::json!({"name":game.name,"executables":game.executables,"source":if custom { "custom" } else { "preset" }})
            })
            .chain(state.config.local_applications.iter().map(|target| {
                let mut entry = serde_json::json!({"id":target.id,"name":target.name,"executables":[target.executable],"source":"local","path":target.path,"kind":target.kind});
                if let Some(activation) = target.activation { entry["activation"] = serde_json::json!(activation); }
                if let Some(path) = &target.bundle_path { entry["bundlePath"] = path.clone().into(); }
                if let Some(identifier) = &target.bundle_identifier { entry["bundleIdentifier"] = identifier.clone().into(); }
                entry
            })).collect()
    }

    pub async fn custom_games(&self) -> Vec<GameConfig> {
        self.inner.read().await.config.custom_games.clone()
    }

    pub async fn set_custom_games(&self, games: Vec<GameConfig>) -> Result<Vec<GameConfig>> {
        self.update_config(|config| {
            config.custom_games = games;
            Ok(())
        })
        .await?;
        let saved = self.custom_games().await;
        let names: Vec<&str> = saved.iter().map(|game| game.name.as_str()).collect();
        prune_game_images(&game_image_dir(&self.config_path), &names);
        Ok(saved)
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

    pub async fn local_applications(&self) -> Vec<config::LocalApplication> {
        self.inner.read().await.config.local_applications.clone()
    }

    pub async fn default_profile(&self) -> Option<ApplicationProfile> {
        self.inner.read().await.config.default_profile.clone()
    }

    async fn update_config(
        &self,
        update: impl FnOnce(&mut BridgeConfig) -> Result<()>,
    ) -> Result<()> {
        let mut state = self.inner.write().await;
        let mut next = state.config.clone();
        update(&mut next)?;
        next = next.normalized();
        for profile in next
            .profiles
            .iter_mut()
            .chain(next.default_profile.iter_mut())
        {
            crate::local_targets::canonicalize_profile(profile, &next.local_applications)?;
        }
        config::save(&self.config_path, &next)?;
        state.config = next;
        Ok(())
    }

    pub async fn replace_local_applications(
        &self,
        applications: Vec<config::LocalApplication>,
    ) -> Result<Vec<config::LocalApplication>> {
        let mut canonical = Vec::new();
        self.update_config(|config| {
            let applications = crate::local_targets::canonicalize_against(
                &applications,
                &config.local_applications,
            )?;
            canonical = applications.clone();
            config.profiles.retain(|profile| {
                profile
                    .application
                    .local_id
                    .as_ref()
                    .is_none_or(|id| applications.iter().any(|target| &target.id == id))
            });
            if config
                .default_profile
                .as_ref()
                .and_then(|profile| profile.application.local_id.as_ref())
                .is_some_and(|id| !applications.iter().any(|target| &target.id == id))
            {
                config.default_profile = None;
            }
            config.local_applications = applications;
            Ok(())
        })
        .await?;
        Ok(canonical)
    }

    pub async fn replace_profiles(&self, mut profiles: Vec<ApplicationProfile>) -> Result<()> {
        self.update_config(move |config| {
            for profile in &mut profiles {
                crate::local_targets::canonicalize_profile(profile, &config.local_applications)?;
            }
            config.profiles = profiles;
            Ok(())
        })
        .await
    }

    pub async fn set_default_profile(&self, mut profile: ApplicationProfile) -> Result<()> {
        self.update_config(move |config| {
            crate::local_targets::canonicalize_profile(&mut profile, &config.local_applications)?;
            config.default_profile = Some(profile);
            Ok(())
        })
        .await
    }

    /// Turns launch-at-login on the first time a release build runs; after
    /// that the user's choice stands. Debug builds never register themselves.
    pub async fn enable_autostart_once(&self) -> Result<()> {
        if cfg!(debug_assertions) || self.inner.read().await.config.autostart_configured {
            return Ok(());
        }
        platform::set_autostart(true)?;
        self.update_config(|config| {
            config.autostart_configured = true;
            Ok(())
        })
        .await
    }

    pub async fn set_battery_threshold(&self, percent: u8) -> Result<()> {
        self.update_config(|config| {
            config.battery_threshold_percent = percent;
            Ok(())
        })
        .await
    }

    pub async fn set_tray_battery_style(&self, style: &str) -> Result<()> {
        let config = {
            let mut state = self.inner.write().await;
            state.config.tray_battery_style = style.to_owned();
            state.config = state.config.clone().normalized();
            state.config.clone()
        };
        config::save(&self.config_path, &config)
    }

    pub async fn set_hybrid_mode(&self, enabled: bool) -> Result<()> {
        let config = {
            let mut state = self.inner.write().await;
            state.config.hybrid_mode = enabled;
            state.config.clone()
        };
        config::save(&self.config_path, &config)
    }

    pub async fn set_automatic_updates(&self, enabled: bool) -> Result<()> {
        self.update_config(|config| {
            config.automatic_updates = enabled;
            Ok(())
        })
        .await
    }

    pub async fn set_notification_sound(&self, enabled: bool) -> Result<()> {
        self.update_config(|config| {
            config.notification_sound = enabled;
            Ok(())
        })
        .await
    }

    pub async fn set_notification_volume(&self, percent: u8) -> Result<()> {
        self.update_config(|config| {
            config.notification_volume = percent;
            Ok(())
        })
        .await
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
            let previous = state.battery.get(&reading.device_id);
            let previous_alert = previous.and_then(|entry| entry.last_alert);
            let should_alert = !reading.charging
                && reading.percent <= threshold
                && previous_alert.is_none_or(|last| last.elapsed() >= cooldown);
            let mut history = previous
                .map(|entry| entry.history.clone())
                .unwrap_or_default();
            history.push_back((Instant::now(), reading.percent));
            while history.len() > BATTERY_HISTORY_SAMPLES {
                history.pop_front();
            }
            // A control panel's `PUT /v1/battery` reports the charge level
            // only; keep the settings a previous native status read saw
            // instead of blanking them until the next native poll.
            let (previous_dpi, previous_rate, previous_mode) = previous
                .map(|entry| (entry.dpi, entry.polling_rate_hz, entry.switch_mode.clone()))
                .unwrap_or((None, None, None));
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
                    dpi: reading.dpi.or(previous_dpi),
                    polling_rate_hz: reading.polling_rate_hz.or(previous_rate),
                    switch_mode: reading.switch_mode.or(previous_mode),
                    updated_at: Instant::now(),
                    history,
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
                poll_batteries_once(service.clone()).await;
            }
        });
    }

    /// Watches the connection state and flips between the two halves of
    /// hybrid mode: while the OpenMouse web panel is disconnected, Bridge
    /// drives the mouse in onboard memory mode (stored settings applied,
    /// battery read natively); the moment the panel connects it takes the
    /// mouse over live, and when it leaves, Bridge restores the stored
    /// settings and reads the battery right away.
    pub fn start_mode_monitor(&self) {
        let service = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let mode = {
                    let state = service.inner.read().await;
                    if panel_connected(&state) {
                        DeviceMode::Live
                    } else {
                        DeviceMode::Onboard
                    }
                };
                let previous = {
                    let mut state = service.inner.write().await;
                    let previous = state.device_mode;
                    state.device_mode = mode;
                    previous
                };
                if previous == mode {
                    continue;
                }
                match mode {
                    DeviceMode::Live => {
                        tracing::info!("the web panel connected — it drives the mouse live now");
                    }
                    DeviceMode::Onboard => {
                        tracing::info!("the web panel disconnected — resuming onboard memory mode");
                        let hybrid = service.inner.read().await.config.hybrid_mode;
                        if !hybrid {
                            continue;
                        }
                        resume_onboard(&service).await;
                    }
                }
            }
        });
    }

    async fn reconcile_native_with(
        &self,
        operation: impl FnOnce(Option<ApplicationProfile>) -> (Option<String>, usize) + Send + 'static,
    ) -> Option<ApplyOutcome> {
        let mut state = self.inner.clone().write_owned().await;
        let selected = resolve_profile_retaining(
            &state.config,
            &state.applications,
            &state.active_games,
            &state.processes,
            state.applied_profile.as_ref(),
        );
        if selected.is_some() && selected != state.applied_profile {
            return None;
        }
        let desired = selected;
        let result = tokio::task::spawn_blocking(move || {
            let has_profile = desired.is_some();
            let (error, pending) = operation(desired);
            let outcome = match &error {
                Some(message) if message.contains("unsupported") => Some(ApplyOutcome::NoDriver),
                Some(_) => Some(ApplyOutcome::Failed),
                None if has_profile => Some(ApplyOutcome::Applied),
                None => None,
            };
            state.native_automation_error = error;
            state.pending_native_restores = pending;
            outcome
        })
        .await;
        match result {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(%error, "Native automation worker failed without publishing a stale completion");
                Some(ApplyOutcome::Failed)
            }
        }
    }

    async fn reconcile_native(&self) -> Option<ApplyOutcome> {
        let automation = self.native_automation.clone();
        self.reconcile_native_with(move |desired| {
            let mut automation = automation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let result = crate::drivers::reconcile_automatic(&mut automation, desired.as_ref());
            (
                result.err().map(|error| format!("{error:#}")),
                automation.pending_restores_for(desired.as_ref()),
            )
        })
        .await
    }

    pub fn start_game_monitor(&self, extract_icons: Arc<AtomicBool>) {
        let service = self.clone();
        tokio::spawn(async move {
            let mut detector = GameDetector::default();
            let mut interval = tokio::time::interval(Duration::from_secs(3));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let (games, local_applications) = {
                    let state = service.inner.read().await;
                    (
                        state.config.tracked_games(),
                        state.config.local_applications.clone(),
                    )
                };
                let mut active = detector.detect(&games);
                let processes = detector.running_processes();
                active.extend(crate::games::matches_local_games(
                    &local_applications,
                    &processes,
                ));
                let running = applications::visible_applications();
                let applications = profile_applications(&running, &games, &local_applications);
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
                state.processes = processes;
                state.applications = applications;
                state.application_icons.extend(icons);
                prune_application_icons(&mut state.application_icons, &running);
                state.running_applications = running;

                // Detect a debounced active-profile switch, push the new
                // profile's DPI/polling rate to the mouse over native HID
                // when Bridge has a driver for it, and announce the switch
                // once the new profile has stayed active long enough.
                let current = resolve_profile_retaining(
                    &state.config,
                    &state.applications,
                    &state.active_games,
                    &state.processes,
                    state.applied_profile.as_ref(),
                );
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
                } else if current_key == state.active_profile_key
                    && current == state.applied_profile
                {
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
                let panel_connected = crate::hid::client_session_active();
                // A control panel holding the mouse is not interrupted by a new helper.
                service.buttons.update(desired_buttons, !panel_connected);
                let outcome = service.reconcile_native().await;
                if let Some((current, previous, true)) = switch
                    && let Some(message) = switch_message(
                        previous.as_ref(),
                        current.as_ref(),
                        default.as_ref(),
                        outcome,
                        panel_connected,
                    )
                {
                    service.announce(message);
                }
            }
        });
    }
}

/// One battery-polling pass: covers every device named by the saved profiles
/// plus every mouse Bridge discovered on the HID bus whose brand no profile
/// already names, reads each mouse's battery natively (skipped while the web
/// panel holds the session — it reports the battery itself over
/// `PUT /v1/battery`) and records it. Used by the five-minute monitor and by
/// onboard-mode handovers.
async fn poll_batteries_once(service: BridgeService) {
    let profiled = profile_devices(&service.inner.read().await.config);
    let discovered = tokio::task::spawn_blocking(drivers::detect_mice)
        .await
        .unwrap_or_default();
    let devices = merge_detected_mice(profiled, &discovered);
    // Pruning is in-memory only, so it runs even while the
    // control panel holds a HID session and reads are paused.
    prune_batteries(&mut service.inner.write().await.battery, &devices);
    if crate::hid::client_session_active() {
        return;
    }
    for (device_id, device_name) in devices {
        let id = device_id.clone();
        let outcome = tokio::task::spawn_blocking(move || crate::drivers::read_status(&id)).await;
        match outcome {
            Ok(Ok(Some(status))) => {
                // The web panel reports batteries as `<brand>:<model>` (the
                // driver's own model name), while discovery ids use the HID
                // descriptor's tail (a LIGHTSPEED receiver reads
                // `Logitech:046d:c547`). Recording under the panel's name
                // for the same physical mouse keeps one entry instead of
                // duplicate rows; the discovery-shaped entry is dropped.
                let record_id = status
                    .battery
                    .reported_name
                    .as_ref()
                    .map(|name| canonical_battery_identity(&device_id, name))
                    .unwrap_or_else(|| device_id.clone());
                if record_id != device_id {
                    service.inner.write().await.battery.remove(&device_id);
                }
                let record_name = status.battery.reported_name.clone().unwrap_or(device_name);
                let reading = BatteryReading {
                    device_id: record_id,
                    device_name: record_name,
                    percent: status.battery.percent,
                    charging: status.battery.charging,
                    dpi: status.dpi,
                    polling_rate_hz: status.polling_rate_hz,
                    switch_mode: status.switch_mode,
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

/// The device id the web panel would use for the same physical mouse:
/// `<brand>:<model>`, from the driver's own `MouseStatus.name`.
fn canonical_battery_identity(device_id: &str, reported_name: &str) -> String {
    let brand = device_id.split(':').next().unwrap_or_default();
    format!("{brand}:{reported_name}")
}

/// The devices battery polling should cover: the saved profiles' devices plus
/// discovered mice whose brand no profile already names — discovery fills the
/// gap before the panel has ever saved a profile, without double-reporting a
/// mouse a profile already covers (profile and discovery ids for the same
/// physical mouse differ, so brand-level dedup keeps one battery entry).
fn merge_detected_mice(
    mut profiled: Vec<(String, String)>,
    detected: &[drivers::DetectedMouse],
) -> Vec<(String, String)> {
    for mouse in detected {
        let brand = mouse.device_id.split(':').next().unwrap_or_default();
        let already_covered = profiled.iter().any(|(id, _)| {
            id.split(':')
                .next()
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(brand))
        });
        if !already_covered {
            profiled.push((mouse.device_id.clone(), mouse.name.clone()));
        }
    }
    profiled
}

/// Enters onboard memory mode: pushes the stored settings to the mouse's
/// onboard memory (when a default profile exists — without one, the device
/// simply keeps the memory it already has) and reads the battery right away.
/// Shared by the startup seed and the panel-disconnect handover.
async fn resume_onboard(service: &BridgeService) {
    let profile = {
        let state = service.inner.read().await;
        resolve_profile_retaining(
            &state.config,
            &state.applications,
            &state.active_games,
            &state.processes,
            None,
        )
    };
    if let Some(profile) = profile {
        tracing::info!(
            profile = %profile.application.name,
            "applying the stored settings to the onboard memory"
        );
        std::thread::spawn(move || {
            apply_natively(&profile);
        });
    }
    tokio::spawn(poll_batteries_once(service.clone()));
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
                local_id: None,
                activation: None,
                bundle_path: None,
                bundle_identifier: None,
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

    #[tokio::test]
    async fn failed_native_join_does_not_publish_unowned_stale_completion() {
        let bridge = service(BridgeConfig::default());
        bridge.inner.write().await.native_automation_error = Some("owned status".into());
        assert_eq!(
            bridge
                .reconcile_native_with(|_| panic!("inert worker failure"))
                .await,
            Some(ApplyOutcome::Failed)
        );
        assert_eq!(
            bridge.inner.read().await.native_automation_error.as_deref(),
            Some("owned status")
        );
    }

    #[tokio::test]
    async fn native_worker_retains_serialization_after_waiter_cancellation() {
        let bridge = service(BridgeConfig::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let running = bridge.clone();
        let waiter = tokio::spawn(async move {
            running
                .reconcile_native_with(move |_| {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    (Some("retained completion".into()), 1)
                })
                .await
        });
        started_rx.await.unwrap();
        waiter.abort();
        let _ = waiter.await;
        assert!(bridge.inner.try_write().is_err());
        release_tx.send(()).unwrap();
        let state = tokio::time::timeout(Duration::from_secs(5), bridge.inner.write())
            .await
            .unwrap();
        assert_eq!(
            state.native_automation_error.as_deref(),
            Some("retained completion")
        );
        assert_eq!(state.pending_native_restores, 1);
    }

    #[tokio::test]
    async fn native_pending_handoff_keeps_original_baseline_without_old_writes() {
        let old = test_profile("Old", "/old", Some(1600), None);
        let next = test_profile("Next", "/next", Some(3200), None);
        let bridge = service(BridgeConfig {
            profiles: vec![old.clone(), next],
            ..BridgeConfig::default()
        });
        {
            let mut state = bridge.inner.write().await;
            state.applied_profile = Some(old);
            state.applications = vec![test_app("Next", "/next", true)];
        }
        assert_eq!(bridge.reconcile_native_with(|_| panic!("pending handoff must retain journal without applying/restoring until arbitration settles")).await, None);
    }

    #[tokio::test]
    async fn native_selection_rechecks_disabled_profile_before_hardware() {
        let game = test_profile("Game", "/game", Some(1600), None);
        let bridge = service(BridgeConfig {
            profiles: vec![game.clone()],
            ..BridgeConfig::default()
        });
        {
            let mut state = bridge.inner.write().await;
            state.applied_profile = Some(game);
            state.applications = vec![test_app("Game", "/game", true)];
            state.config.profiles[0].enabled = false;
        }
        bridge
            .reconcile_native_with(|desired| {
                assert!(
                    desired.is_none(),
                    "disabled profile must never reach the native writer"
                );
                (None, 0)
            })
            .await;
    }

    #[tokio::test]
    async fn native_reconcile_serializes_configuration_and_reports_truthful_failure() {
        let game = test_profile("Game", "/game", Some(1600), None);
        let bridge = service(BridgeConfig {
            profiles: vec![game.clone()],
            ..BridgeConfig::default()
        });
        {
            let mut state = bridge.inner.write().await;
            state.applied_profile = Some(game.clone());
            state.applications = vec![test_app("Game", "/game", true)];
        }
        let checked = bridge.clone();
        bridge
            .reconcile_native_with(move |desired| {
                assert_eq!(desired, Some(game));
                assert!(
                    checked.inner.try_write().is_err(),
                    "configuration must not change during the bounded HID operation"
                );
                (Some("unsupported acknowledged readback".into()), 1)
            })
            .await;
        let snapshot = bridge.snapshot().await;
        assert_eq!(
            snapshot.native_automation_error.as_deref(),
            Some("unsupported acknowledged readback")
        );
        assert_eq!(snapshot.pending_native_restores, 1);
        assert!(snapshot.active_profile.is_some());
    }

    #[tokio::test]
    async fn running_arbitration_retains_current_then_uses_persisted_order() {
        let mut first = test_profile("Zed", "/opt/zed", Some(800), None);
        first.application.local_id = Some("local:zed".into());
        let mut second = test_profile("Alpha", "/opt/alpha", Some(1600), None);
        second.application.local_id = Some("local:alpha".into());
        let config = BridgeConfig {
            local_applications: serde_json::from_value(serde_json::json!([
                {"id":"local:zed","name":"Zed","executable":"zed","path":"/opt/zed","kind":"app","activation":"running"},
                {"id":"local:alpha","name":"Alpha","executable":"alpha","path":"/opt/alpha","kind":"app","activation":"running"}
            ])).unwrap(),
            profiles: vec![first.clone(), second.clone()],
            ..BridgeConfig::default()
        }.normalized();
        let first = config.profiles[0].clone();
        let second = config.profiles[1].clone();
        let bridge = service(config);
        {
            let mut state = bridge.inner.write().await;
            state.active_games = vec!["local:alpha".into(), "local:zed".into()];
            state.processes = vec![
                crate::games::RunningProcess {
                    executable: "zed".into(),
                    path: "/opt/zed".into(),
                },
                crate::games::RunningProcess {
                    executable: "alpha".into(),
                    path: "/opt/alpha".into(),
                },
            ];
        }
        assert_eq!(bridge.snapshot().await.active_profile, Some(first.clone()));
        bridge.inner.write().await.applied_profile = Some(second.clone());
        assert_eq!(bridge.snapshot().await.active_profile, Some(second));
        bridge.inner.write().await.config.profiles[1].enabled = false;
        assert_eq!(bridge.snapshot().await.active_profile, Some(first));
        bridge.inner.write().await.active_games.clear();
        bridge.inner.write().await.processes.clear();
        assert_eq!(bridge.snapshot().await.active_profile, None);
    }

    #[test]
    fn local_running_ids_cannot_override_changed_executable_identity() {
        let target: config::LocalApplication = serde_json::from_value(serde_json::json!({
            "id":"local:editor","name":"Editor","executable":"editor","path":"/new/editor","kind":"app","activation":"running"
        })).unwrap();
        let mut profile = test_profile("Editor", "/new/editor", Some(800), None);
        profile.application.local_id = Some(target.id.clone());
        let config = BridgeConfig {
            local_applications: vec![target],
            profiles: vec![profile],
            ..BridgeConfig::default()
        };
        assert_eq!(
            resolve_profile(
                &config,
                &[],
                &["local:editor".into()],
                &[crate::games::RunningProcess {
                    executable: "editor".into(),
                    path: "/old/editor".into()
                }]
            ),
            None
        );
    }

    #[test]
    fn full_path_never_falls_back_to_name_or_executable() {
        let profile = test_profile("Editor", "C:/Trusted/Editor.exe", Some(800), None);
        let config = BridgeConfig {
            profiles: vec![profile.clone()],
            ..BridgeConfig::default()
        };
        assert_eq!(
            active_profile_for(
                &config,
                &[test_app("Editor", "C:/Other/Editor.exe", true)],
                &[]
            ),
            None
        );
        assert_eq!(
            active_profile_for(
                &config,
                &[test_app("Editor", r"c:\trusted\EDITOR.exe", true)],
                &[]
            ),
            Some(profile)
        );
        let profile = test_profile("Editor", "/Apps/Editor", Some(800), None);
        let config = BridgeConfig {
            profiles: vec![profile],
            ..BridgeConfig::default()
        };
        assert_eq!(
            active_profile_for(&config, &[test_app("Editor", "/apps/editor", true)], &[]),
            None
        );
    }

    #[test]
    fn local_game_background_activity_is_id_linked_and_not_catalog_gated() {
        let mut profile = test_profile("Editor", "/opt/Editor.exe", Some(1200), None);
        profile.application.local_id = Some("local:editor".into());
        let config = BridgeConfig {
            local_applications: vec![serde_json::from_value(serde_json::json!({"id":"local:editor","name":"Editor","executable":"Editor.exe","path":"/opt/Editor.exe","kind":"game"})).unwrap()],
            profiles: vec![profile.clone()],
            default_profile: Some(test_profile("Default", "", Some(800), None)),
            ..BridgeConfig::default()
        };
        assert_eq!(
            resolve_profile(
                &config,
                &[],
                &["local:editor".into()],
                &[crate::games::RunningProcess {
                    executable: "Editor.exe".into(),
                    path: "/opt/Editor.exe".into()
                }]
            ),
            Some(profile)
        );
    }

    #[test]
    fn local_foreground_apps_bypass_catalog_but_do_not_stay_active_in_background() {
        let mut profile = test_profile("Editor", "C:/Tools/Editor.exe", Some(1200), None);
        profile.application.local_id = Some("local:editor".into());
        let target = serde_json::from_value(serde_json::json!({"id":"local:editor","name":"Editor","executable":"Editor.exe","path":"C:/Tools/Editor.exe","kind":"app"})).unwrap();
        let config = BridgeConfig {
            local_applications: vec![target],
            profiles: vec![profile.clone()],
            ..BridgeConfig::default()
        };
        let application = test_app("Editor", "C:/Tools/Editor.exe", true);
        let selected = profile_applications(
            std::slice::from_ref(&application),
            &config.games,
            &config.local_applications,
        );
        assert_eq!(selected, [application]);
        assert_eq!(active_profile_for(&config, &selected, &[]), Some(profile));
        let background = test_app("Editor", "C:/Tools/Editor.exe", false);
        assert_eq!(
            active_profile_for(&config, &[background], &["local:editor".into()]),
            None
        );
    }

    #[tokio::test]
    async fn local_activity_status_uses_display_names_not_internal_ids() {
        let target = serde_json::from_value(serde_json::json!({"id":"local:game","name":"Custom Game","executable":"custom.exe","path":"","kind":"game"})).unwrap();
        let bridge = service(BridgeConfig {
            local_applications: vec![target],
            ..BridgeConfig::default()
        });
        bridge.inner.write().await.active_games = vec!["local:game".into()];
        let status = bridge.snapshot().await;
        assert_eq!(status.active_games, ["Custom Game"]);
        assert_eq!(status.tracked_game_count, 1);
        assert!(
            status
                .games
                .iter()
                .any(|game| game.name == "Custom Game" && game.active)
        );
    }

    #[test]
    fn local_display_name_does_not_activate_an_unrelated_preset() {
        let local: config::LocalApplication = serde_json::from_value(serde_json::json!({"id":"local:editor","name":"Editor","executable":"custom.exe","path":"","kind":"app"})).unwrap();
        let preset = test_profile("Editor", "", Some(800), None);
        let config = BridgeConfig {
            games: vec![GameConfig {
                name: "Editor".into(),
                executables: vec!["Editor.exe".into()],
            }],
            local_applications: vec![local],
            profiles: vec![preset],
            ..BridgeConfig::default()
        };
        let mut application = test_app("Editor", "C:/Tools/custom.exe", true);
        application.executable = "custom.exe".into();
        let candidates =
            profile_applications(&[application], &config.games, &config.local_applications);
        assert_eq!(active_profile_for(&config, &candidates, &[]), None);
    }

    #[test]
    fn explicit_local_paths_win_and_disabled_locals_return_to_default() {
        let broad: config::LocalApplication = serde_json::from_value(serde_json::json!({"id":"local:broad","name":"Same","executable":"Editor.exe","path":"","kind":"game"})).unwrap();
        let exact = config::LocalApplication {
            id: "local:exact".into(),
            path: "C:/Tools/Editor.exe".into(),
            ..broad.clone()
        };
        let mut broad_profile = test_profile("Same", "", Some(800), None);
        broad_profile.application.local_id = Some(broad.id.clone());
        let mut exact_profile = test_profile("Same", &exact.path, Some(1600), None);
        exact_profile.application.local_id = Some(exact.id.clone());
        let default = test_profile("Default", "", Some(400), None);
        let mut config = BridgeConfig {
            local_applications: vec![broad, exact],
            profiles: vec![exact_profile.clone(), broad_profile],
            default_profile: Some(default.clone()),
            ..BridgeConfig::default()
        };
        let foreground = test_app("Editor", "C:/Tools/Editor.exe", true);
        assert_eq!(
            resolve_profile(
                &config,
                std::slice::from_ref(&foreground),
                &[],
                &[crate::games::RunningProcess {
                    executable: foreground.executable.clone(),
                    path: foreground.path.clone()
                }]
            ),
            Some(exact_profile.clone())
        );
        let processes = vec![crate::games::RunningProcess {
            path: foreground.path.clone(),
            executable: foreground.executable.clone(),
        }];
        let active = crate::games::matches_local_games(&config.local_applications, &processes);
        assert_eq!(
            resolve_profile(&config, &[], &active, &processes),
            Some(exact_profile)
        );
        for profile in &mut config.profiles {
            profile.enabled = false;
        }
        assert_eq!(
            resolve_profile(&config, &[foreground], &active, &processes),
            Some(default)
        );
    }

    #[test]
    fn background_preset_matching_never_ignores_a_saved_full_path() {
        let profile = test_profile("Editor", "C:/Trusted/Editor.exe", Some(800), None);
        let config = BridgeConfig {
            games: vec![GameConfig {
                name: "Editor".into(),
                executables: vec!["Editor.exe".into()],
            }],
            profiles: vec![profile.clone()],
            ..BridgeConfig::default()
        };
        let wrong = crate::games::RunningProcess {
            executable: "Editor.exe".into(),
            path: "C:/Other/Editor.exe".into(),
        };
        assert_eq!(
            resolve_profile(&config, &[], &["Editor".into()], &[wrong]),
            None
        );
        let matching = crate::games::RunningProcess {
            executable: "Editor.exe".into(),
            path: r"c:\trusted\EDITOR.exe".into(),
        };
        assert_eq!(
            resolve_profile(&config, &[], &["Editor".into()], &[matching]),
            Some(profile)
        );
    }

    #[test]
    fn local_and_preset_same_name_have_distinct_switch_keys() {
        let preset = test_profile("Editor", "", Some(800), None);
        let mut local = preset.clone();
        local.application.local_id = Some("local:editor".into());
        assert_ne!(profile_key(&preset), profile_key(&local));
        let mut other_device = local.clone();
        other_device.device.id = "other-device".into();
        assert_ne!(profile_key(&local), profile_key(&other_device));
        let mut other = local.clone();
        other.application.local_id = Some("local:other".into());
        assert_ne!(profile_key(&local), profile_key(&other));
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
        let game_key = profile_key(&config.profiles[0]);
        let default_key = profile_key(config.default_profile.as_ref().unwrap());

        let (game, is_default) = current_profile(&config, &game_key).expect("the game profile");
        assert_eq!(
            (game.application.name.as_str(), is_default),
            ("Valorant", false)
        );
        let (default, is_default) =
            current_profile(&config, &default_key).expect("the default profile");
        assert_eq!(
            (default.application.name.as_str(), is_default),
            ("Default", true)
        );

        // Editing a profile keeps its key, so the lookup finds the new settings.
        config.default_profile = Some(test_profile("Default", "", Some(1600), None));
        let (edited, _) =
            current_profile(&config, &default_key).expect("the edited default profile");
        assert_eq!(edited.settings.dpi, Some(1600));
        let other_key = profile_key(&test_profile("Other", "/games/other", None, None));
        assert!(current_profile(&config, &other_key).is_none());

        // A disabled profile of the same game is not the one that is applied.
        let mut disabled = test_profile("Valorant", "/games/valorant", Some(400), None);
        disabled.enabled = false;
        config.profiles = vec![disabled];
        assert!(current_profile(&config, &game_key).is_none());
    }

    #[test]
    fn a_custom_games_profile_activates_while_the_game_runs() {
        let mut profile = test_profile("My Game", "", Some(1600), None);
        profile.application.executable = "mygame.exe".into();
        let config = BridgeConfig {
            custom_games: vec![GameConfig {
                name: "My Game".into(),
                executables: vec!["mygame.exe".into()],
            }],
            profiles: vec![profile],
            ..BridgeConfig::default()
        }
        .normalized();
        let active = resolve_profile_retaining(&config, &[], &["My Game".into()], &[], None)
            .expect("the custom game's profile");
        assert_eq!(active.settings.dpi, Some(1600));
        assert!(resolve_profile_retaining(&config, &[], &[], &[], None).is_none());
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
    fn failed_native_restore_is_not_announced_as_profile_off() {
        let game = test_profile("Game", "/game", Some(1600), None);
        let notice =
            switch_message(Some(&game), None, None, Some(ApplyOutcome::Failed), false).unwrap();
        assert_eq!(
            notice.detail,
            "Couldn't restore original mouse settings — check Bridge status"
        );
        let unsupported =
            switch_message(None, Some(&game), None, Some(ApplyOutcome::NoDriver), false).unwrap();
        assert_eq!(
            unsupported.detail,
            "Native automation unsupported — acknowledged device readback is required"
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
                    dpi: None,
                    polling_rate_hz: None,
                    switch_mode: None,
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
                    dpi: None,
                    polling_rate_hz: None,
                    switch_mode: None,
                })
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn settings_less_panel_reports_keep_the_stored_settings() {
        let bridge = service(BridgeConfig::default());
        // Native status read: battery plus onboard settings.
        bridge
            .record_battery(BatteryReading {
                device_id: "Logitech:PRO X SUPERLIGHT 2 SE".into(),
                device_name: "PRO X SUPERLIGHT 2 SE".into(),
                percent: 81,
                charging: false,
                dpi: Some(800),
                polling_rate_hz: Some(1000),
                switch_mode: Some("Hybrid".into()),
            })
            .await
            .unwrap();
        // The panel's PUT carries the charge level only.
        bridge
            .record_battery(BatteryReading {
                device_id: "Logitech:PRO X SUPERLIGHT 2 SE".into(),
                device_name: "PRO X SUPERLIGHT 2 SE".into(),
                percent: 80,
                charging: false,
                dpi: None,
                polling_rate_hz: None,
                switch_mode: None,
            })
            .await
            .unwrap();
        let battery = bridge
            .snapshot()
            .await
            .batteries
            .into_iter()
            .next()
            .expect("battery entry survives");
        assert_eq!(battery.percent, 80);
        assert_eq!(battery.dpi, Some(800));
        assert_eq!(battery.polling_rate_hz, Some(1000));
        assert_eq!(battery.switch_mode.as_deref(), Some("Hybrid"));
    }

    fn battery_entry(age: Duration) -> BatteryState {
        BatteryState {
            last_alert: None,
            device_name: "Mouse".into(),
            percent: 50,
            charging: false,
            dpi: None,
            polling_rate_hz: None,
            switch_mode: None,
            updated_at: Instant::now() - age,
            history: VecDeque::new(),
        }
    }

    fn history(minutes_ago_percent: &[(u64, u8)]) -> VecDeque<(Instant, u8)> {
        let now = Instant::now();
        minutes_ago_percent
            .iter()
            .map(|(minutes_ago, percent)| (now - Duration::from_secs(minutes_ago * 60), *percent))
            .collect()
    }

    #[test]
    fn a_steady_drain_estimates_time_to_empty() {
        let samples = history(&[(60, 50), (30, 48), (0, 45)]);
        assert_eq!(estimate_minutes_remaining(&samples, 45, false), Some(540));
    }

    #[test]
    fn a_steady_charge_estimates_time_to_full() {
        let samples = history(&[(60, 20), (0, 80)]);
        assert_eq!(estimate_minutes_remaining(&samples, 80, true), Some(20));
    }

    #[test]
    fn a_flat_level_has_no_estimate() {
        let samples = history(&[(60, 50), (0, 50)]);
        assert_eq!(estimate_minutes_remaining(&samples, 50, false), None);
    }

    #[test]
    fn too_little_history_has_no_estimate() {
        let samples = history(&[(10, 50), (0, 45)]);
        assert_eq!(estimate_minutes_remaining(&samples, 45, false), None);
    }

    #[test]
    fn samples_outside_the_trend_window_are_ignored() {
        let samples = history(&[(10 * 60, 50), (0, 45)]);
        assert_eq!(estimate_minutes_remaining(&samples, 45, false), None);
    }

    #[test]
    fn an_estimate_past_two_days_is_noise() {
        let samples = history(&[(60, 50), (0, 49)]);
        assert_eq!(estimate_minutes_remaining(&samples, 49, false), None);
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
                    dpi: None,
                    polling_rate_hz: None,
                    switch_mode: None,
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

    #[tokio::test]
    async fn the_mode_flips_to_live_when_the_panel_handshakes() {
        let bridge = service(BridgeConfig::default());
        assert_eq!(bridge.snapshot().await.device_mode, "onboard");

        let monitored = bridge.clone();
        monitored.start_mode_monitor();
        bridge.record_client_heartbeat().await;
        // The mode monitor ticks every second; give it a beat to spot the
        // handshake and flip the mode.
        tokio::time::sleep(Duration::from_millis(1400)).await;

        assert_eq!(bridge.snapshot().await.device_mode, "live");
        assert!(bridge.snapshot().await.hybrid_mode);
    }

    #[test]
    fn the_discovered_battery_id_becomes_the_panel_style_id() {
        // The panel reports `<brand>:<model>`; discovery's descriptor tail
        // (a LIGHTSPEED receiver) must canonicalize onto that id so the
        // same mouse never appears as two rows.
        assert_eq!(
            canonical_battery_identity("Logitech:046d:c547", "PRO X SUPERLIGHT 2 SE"),
            "Logitech:PRO X SUPERLIGHT 2 SE"
        );
        // Brands with no model name keep their discovered id.
        assert_eq!(
            canonical_battery_identity("Logitech:046d:c547", ""),
            "Logitech:"
        );
    }

    #[test]
    fn discovered_mice_fill_the_gap_without_shadowing_profiled_devices() {
        let detected = |device_id: &str, name: &str| drivers::DetectedMouse {
            device_id: device_id.into(),
            name: name.into(),
        };
        // No profiles: every discovered mouse is polled.
        assert_eq!(
            merge_detected_mice(
                Vec::new(),
                &[detected("Pulsar:0d1f0200", "Pulsar X2 CrazyLight")]
            ),
            vec![(
                "Pulsar:0d1f0200".to_owned(),
                "Pulsar X2 CrazyLight".to_owned()
            )]
        );
        // A profile already names the brand: discovery adds nothing, so the
        // same physical mouse is never reported under two ids.
        assert_eq!(
            merge_detected_mice(
                vec![("Pulsar:X2 CrazyLight".to_owned(), "X2".to_owned())],
                &[detected("Pulsar:0d1f0200", "Pulsar X2 CrazyLight")]
            ),
            vec![("Pulsar:X2 CrazyLight".to_owned(), "X2".to_owned())]
        );
        // Brand matching is case-insensitive; other brands still merge.
        assert_eq!(
            merge_detected_mice(
                vec![("pulsar:X2".to_owned(), "X2".to_owned())],
                &[
                    detected("Pulsar:0d1f0200", "Pulsar X2 CrazyLight"),
                    detected("Delux:M800", "Delux M800")
                ]
            ),
            vec![
                ("pulsar:X2".to_owned(), "X2".to_owned()),
                ("Delux:M800".to_owned(), "Delux M800".to_owned())
            ]
        );
    }
}
