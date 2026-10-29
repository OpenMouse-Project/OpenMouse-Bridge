//! What the GPUI window and the headless mode both sit on: the background
//! server (the service, its HTTP API and HID, on their own thread), the commands
//! and update events that cross to the window, the update flow, and the helpers
//! for restarting, opening logs and the control panel, and relaunching after a
//! crash.

use std::{
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::AtomicBool,
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow};
use openmouse_bridge::{
    BRIDGE_PORT, api, config, platform,
    service::{BridgeService, BridgeSnapshot, Notice},
    updater::{self, UpdateInfo},
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::io::Cursor;
#[cfg(target_os = "windows")]
use std::ptr::null_mut;
use tokio::{
    net::TcpListener,
    sync::{mpsc as tokio_mpsc, oneshot},
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tray_icon::Icon;
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

const OPENMOUSE_URL: &str = "https://control.openmouse.app";

/// Present when the user explicitly wants no tray panel (or when no display
/// exists): Bridge serves its HTTP API, HID, and game profiles without a
/// window. Checked first so servers and SSH sessions never touch winit.
fn headless_requested() -> bool {
    env::var_os("OPENMOUSE_BRIDGE_HEADLESS").is_some() || env::args().any(|arg| arg == "--headless")
}

/// Whether a display server is reachable for the window.
fn gui_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        env::var_os("WAYLAND_DISPLAY").is_some() || env::var_os("DISPLAY").is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Serves the Bridge runtime without a window: drains notices as native
/// notifications, applies update restarts, and blocks on Ctrl-C.
fn run_headless(
    server: BackgroundServer,
    snapshots: Receiver<BridgeSnapshot>,
    commands: tokio_mpsc::UnboundedSender<BridgeCommand>,
    updates: Receiver<UpdateEvent>,
    notices: Receiver<Notice>,
) -> Result<()> {
    // The snapshot channel only holds the latest; keep it drained so the
    // publisher never blocks, and keep the update channel drained so the
    // updater's try_send calls keep working.
    let _ = (snapshots, commands);
    // Ctrl-C watcher on its own thread: the multi-thread Bridge runtime
    // already lives on the server thread, so this gets a current-thread one.
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    thread::Builder::new()
        .name("openmouse-bridge-signal".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(runtime) = runtime {
                let _ = runtime.block_on(tokio::signal::ctrl_c());
                let _ = shutdown_tx.send(());
            }
        })
        .ok();
    tracing::info!("OpenMouse Bridge running headless (no tray panel)");
    loop {
        while let Ok(notice) = notices.try_recv() {
            notify_natively(&notice);
        }
        while let Ok(event) = updates.try_recv() {
            // Match the GUI path: the updater stages the new binary and
            // spawns its own post-exit relaunch helper, so exiting here
            // hands off to the updated Bridge.
            if matches!(event, UpdateEvent::Restarting(_)) {
                server.stop()?;
                return Ok(());
            }
        }
        if shutdown_rx.try_recv().is_ok() {
            server.stop()?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Runs Bridge: the GPUI window, or, when asked to (or when no display exists),
/// the same runtime with no window at all.
pub fn run() -> Result<()> {
    if headless_requested() || !gui_available() {
        if !headless_requested() {
            tracing::warn!(
                "no usable display for the tray panel (set WAYLAND_DISPLAY or DISPLAY); running headless: HTTP API, HID, and game profiles still work"
            );
        }
        let (notice_tx, notices) = mpsc::channel();
        let Some((server, snapshots, commands, updates, _panel_requests, _show_panel)) =
            BackgroundServer::start(notice_tx)?
        else {
            return Ok(());
        };
        return run_headless(server, snapshots, commands, updates, notices);
    }
    crate::gpui_shell::run()
}

pub(crate) const WINDOW_WIDTH: f32 = 360.0;

pub(crate) const WINDOW_HEIGHT: f32 = 280.0;

/// Glow/winit often tear down after Windows hibernate. If the tray loop ends
/// without an Exit or in-app update, spawn a replacement (issue #16).
const RELAUNCH_ENV: &str = "OPENMOUSE_BRIDGE_RELAUNCH";

const RESUME_GAP: Duration = Duration::from_secs(10);

const GUI_STABLE_AFTER: Duration = Duration::from_secs(30);

const MAX_QUICK_RELAUNCHES: u32 = 5;

pub(crate) const RELAUNCH_DELAY: Duration = Duration::from_secs(2);

pub(crate) fn service_label(snapshot: Option<&BridgeSnapshot>) -> &'static str {
    if snapshot.is_some() {
        "Bridge service ready"
    } else {
        "Starting Bridge…"
    }
}

pub(crate) enum BridgeCommand {
    SetBatteryThreshold(u8),
    SetNotificationSound(bool),
    SetNotificationVolume(u8),
    SetAutomaticUpdates(bool),
    SetHybridMode(bool),
    SetTrayBatteryStyle(platform::TrayBatteryStyle),
    CheckForUpdates,
    InstallUpdate(UpdateInfo),
    AcknowledgeTrayHint,
    ForgetBattery(String),
}

enum Startup {
    Serving { show_panel: bool },
    AlreadyRunning,
}

pub(crate) enum UpdateEvent {
    Checking,
    UpToDate,
    Available(UpdateInfo),
    Downloading(String),
    Restarting(String),
    Failed(String),
}

pub(crate) struct BackgroundServer {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

/// Where the runtime sends notices, and the tray app's context to
/// wake so it shows them even while its panel is hidden.
pub(crate) type NoticeSink = Sender<Notice>;

pub(crate) type BackgroundRuntime = (
    BackgroundServer,
    Receiver<BridgeSnapshot>,
    tokio_mpsc::UnboundedSender<BridgeCommand>,
    Receiver<UpdateEvent>,
    Receiver<()>,
    bool,
);

impl BackgroundServer {
    pub(crate) fn start(notices: NoticeSink) -> Result<Option<BackgroundRuntime>> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (snapshot_tx, snapshots) = mpsc::sync_channel(1);
        let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
        let (update_tx, updates) = mpsc::sync_channel(8);
        let (panel_tx, panel_requests) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("openmouse-bridge-runtime".into())
            .spawn(move || {
                let outcome = run_server(
                    shutdown_rx,
                    ready_tx.clone(),
                    snapshot_tx,
                    command_rx,
                    update_tx,
                    notices,
                    panel_tx,
                );
                if let Err(error) = &outcome {
                    let _ = ready_tx.send(Err(format!("{error:#}")));
                }
                outcome
            })
            .context("could not start the Bridge runtime")?;

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(Startup::AlreadyRunning)) => {
                let _ = shutdown_tx.send(());
                let _ = thread.join();
                Ok(None)
            }
            Ok(Ok(Startup::Serving { show_panel })) => Ok(Some((
                Self {
                    shutdown: Some(shutdown_tx),
                    thread: Some(thread),
                },
                snapshots,
                command_tx,
                updates,
                panel_requests,
                show_panel,
            ))),
            Ok(Err(error)) => {
                let _ = shutdown_tx.send(());
                let _ = thread.join();
                Err(anyhow!(error))
            }
            Err(error) => {
                let _ = shutdown_tx.send(());
                let _ = thread.join();
                Err(anyhow!("Bridge did not become ready: {error}"))
            }
        }
    }

    pub(crate) fn stop(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        match self.thread.take().map(|thread| thread.join()) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(anyhow!("the Bridge runtime thread stopped unexpectedly")),
            None => Ok(()),
        }
    }
}

/// The time since `previous` when it is long enough that the PC must have
/// slept; a backwards clock change is not a resume.
pub(crate) fn resume_gap(previous: SystemTime, now: SystemTime) -> Option<Duration> {
    now.duration_since(previous)
        .ok()
        .filter(|gap| *gap >= RESUME_GAP)
}

pub(crate) fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|value| (*value).to_owned())
        })
        .unwrap_or_else(|| "unknown panic".into())
}

pub(crate) fn current_relaunch_generation() -> u32 {
    env::var(RELAUNCH_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// After Glow/winit die (common on Windows hibernate), replace this process
/// unless the user quit or an in-app update is already relaunching. Quick
/// crash loops stop after `MAX_QUICK_RELAUNCHES`; a panel that stayed up
/// for `GUI_STABLE_AFTER` resets the counter.
pub(crate) fn next_relaunch_generation(
    current: u32,
    gui_lived: Duration,
    keep_running: bool,
) -> Option<u32> {
    if !keep_running {
        return None;
    }
    let generation = if gui_lived >= GUI_STABLE_AFTER {
        0
    } else {
        current.saturating_add(1)
    };
    (generation <= MAX_QUICK_RELAUNCHES).then_some(generation)
}

pub(crate) fn restart_bridge(generation: u32) -> Result<()> {
    let executable = env::current_exe().context("could not locate the Bridge executable")?;
    let working_directory = executable
        .parent()
        .context("Bridge executable has no parent directory")?;
    std::process::Command::new(&executable)
        .current_dir(working_directory)
        .env(RELAUNCH_ENV, generation.to_string())
        .spawn()
        .context("could not restart OpenMouse Bridge")?;
    Ok(())
}

async fn activate_running_bridge() -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!(%error, "could not ask the running Bridge to open its panel");
            return false;
        }
    };
    match client
        .put(format!("http://127.0.0.1:{BRIDGE_PORT}/v1/panel"))
        .send()
        .await
    {
        Ok(response) => response.status().is_success(),
        Err(error) => {
            tracing::debug!(%error, "the running Bridge did not open its panel");
            false
        }
    }
}

fn run_server(
    shutdown: oneshot::Receiver<()>,
    ready: SyncSender<Result<Startup, String>>,
    snapshots: SyncSender<BridgeSnapshot>,
    mut commands: tokio_mpsc::UnboundedReceiver<BridgeCommand>,
    update_events: SyncSender<UpdateEvent>,
    notices: NoticeSink,
    panel_requests: Sender<()>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not create the Bridge async runtime")?;
    runtime.block_on(async move {
        let (bridge_config, path) = config::load_with_catalog().await?;
        let show_panel = !bridge_config.tray_hint_shown;
        let automatic_updates = bridge_config.automatic_updates;
        let origins = bridge_config.allowed_origins.clone();
        let service = BridgeService::new(bridge_config, path.clone());
        let notices = std::sync::Mutex::new(notices);
        service.on_notice(move |notice| {
            let _ = notices.lock().map(|sender| sender.send(notice));
        });
        service.on_show_panel(move || {
            let _ = panel_requests.send(());
        });
        if let Err(error) = service.enable_autostart_once().await {
            tracing::warn!(%error, "Could not turn on launch at login");
        }
        service.start_game_monitor(Arc::new(AtomicBool::new(true)));
        service.start_battery_monitor();

        let snapshot_service = service.clone();
        let snapshot_publisher = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let _ = snapshots.try_send(snapshot_service.snapshot().await);
            }
        });

        let command_service = service.clone();
        let command_handler = tokio::spawn(async move {
            // A stub install can't run its HID helper yet; finishing it comes
            // before any ordinary update check.
            if updater::is_stub_install() {
                tokio::spawn(finish_stub_install(update_events.clone()));
            } else if automatic_updates {
                check_for_updates(true, &update_events).await;
            }
            while let Some(command) = commands.recv().await {
                match command {
                    BridgeCommand::SetBatteryThreshold(percent) => {
                        if let Err(error) = command_service.set_battery_threshold(percent).await {
                            tracing::error!(%error, "could not save Bridge settings");
                        }
                    }
                    BridgeCommand::SetNotificationSound(enabled) => {
                        if let Err(error) = command_service.set_notification_sound(enabled).await {
                            tracing::error!(%error, "could not save Bridge settings");
                        }
                    }
                    BridgeCommand::SetNotificationVolume(percent) => {
                        if let Err(error) = command_service.set_notification_volume(percent).await {
                            tracing::error!(%error, "could not save Bridge settings");
                        }
                    }
                    BridgeCommand::SetAutomaticUpdates(enabled) => {
                        match command_service.set_automatic_updates(enabled).await {
                            Ok(()) if enabled => check_for_updates(true, &update_events).await,
                            Ok(()) => {}
                            Err(error) => {
                                tracing::error!(%error, "could not save Bridge settings");
                            }
                        }
                    }
                    BridgeCommand::SetHybridMode(enabled) => {
                        if let Err(error) = command_service.set_hybrid_mode(enabled).await {
                            tracing::error!(%error, "could not save Bridge settings");
                        }
                    }
                    BridgeCommand::SetTrayBatteryStyle(style) => {
                        if let Err(error) = command_service
                            .set_tray_battery_style(style.as_config_str())
                            .await
                        {
                            tracing::error!(%error, "could not save Bridge settings");
                        }
                    }
                    BridgeCommand::CheckForUpdates => {
                        check_for_updates(false, &update_events).await;
                    }
                    BridgeCommand::InstallUpdate(update) => {
                        install_update(update, &update_events).await;
                    }
                    BridgeCommand::ForgetBattery(device_id) => {
                        command_service.forget_battery(&device_id).await;
                    }
                    BridgeCommand::AcknowledgeTrayHint => {
                        if let Err(error) = command_service.acknowledge_tray_hint().await {
                            tracing::error!(%error, "could not save Bridge settings");
                        }
                    }
                }
            }
        });

        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), BRIDGE_PORT);
        let listener = match TcpListener::bind(address).await {
            Ok(listener) => listener,
            Err(error) => {
                // The first launch still holds 17846. Ask it to open the panel
                // instead of exiting with nothing on screen.
                if activate_running_bridge().await {
                    tracing::info!("OpenMouse Bridge is already running; opening its tray panel");
                    let _ = ready.send(Ok(Startup::AlreadyRunning));
                    return Ok(());
                }
                return Err(error).context(format!(
                    "could not bind http://{address}; is Bridge already running?"
                ));
            }
        };
        tracing::info!(%address, config = %path.display(), "OpenMouse Bridge is ready");
        let _ = ready.send(Ok(Startup::Serving { show_panel }));
        let outcome = axum::serve(listener, api::router(service, &origins))
            .with_graceful_shutdown(async {
                let _ = shutdown.await;
            })
            .await;
        snapshot_publisher.abort();
        command_handler.abort();
        outcome?;
        Ok(())
    })
}

async fn check_for_updates(install_automatically: bool, events: &SyncSender<UpdateEvent>) {
    let _ = events.try_send(UpdateEvent::Checking);
    match updater::check_for_update().await {
        Ok(Some(update)) if install_automatically => install_update(update, events).await,
        Ok(Some(update)) => {
            let _ = events.try_send(UpdateEvent::Available(update));
        }
        Ok(None) => {
            let _ = events.try_send(UpdateEvent::UpToDate);
        }
        Err(error) => {
            tracing::warn!(%error, "Bridge update check failed");
            let _ = events.try_send(UpdateEvent::Failed(format!("{error:#}")));
        }
    }
}

/// Fetches the full package a bridge install is missing, retrying with a
/// growing delay until it is staged; the update helper then restarts Bridge.
async fn finish_stub_install(events: SyncSender<UpdateEvent>) {
    let mut delay = Duration::from_secs(15);
    loop {
        let _ = events.try_send(UpdateEvent::Checking);
        let outcome = match updater::check_for_stub_completion().await {
            Ok(update) => {
                let version = update.version.clone();
                let _ = events.try_send(UpdateEvent::Downloading(version.clone()));
                updater::download_and_stage(&update).await.map(|()| version)
            }
            Err(error) => Err(error),
        };
        match outcome {
            Ok(version) => {
                let _ = events.try_send(UpdateEvent::Restarting(version));
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "could not finish installing Bridge; will retry");
                let _ = events.try_send(UpdateEvent::Failed(format!("{error:#}")));
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(15 * 60));
    }
}

async fn install_update(update: UpdateInfo, events: &SyncSender<UpdateEvent>) {
    let version = update.version.clone();
    let _ = events.try_send(UpdateEvent::Downloading(version.clone()));
    match updater::download_and_stage(&update).await {
        Ok(()) => {
            let _ = events.try_send(UpdateEvent::Restarting(version));
        }
        Err(error) => {
            tracing::error!(%error, "Bridge update failed");
            let _ = events.try_send(UpdateEvent::Failed(format!("{error:#}")));
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn configure_tray_only_application() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    let Some(marker) = MainThreadMarker::new() else {
        tracing::error!("OpenMouse Bridge must initialize its tray on the macOS main thread");
        return;
    };
    let application = NSApplication::sharedApplication(marker);
    if !application.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        tracing::warn!("macOS refused the tray-only application policy");
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) fn configure_tray_only_application() {}

pub(crate) fn notify_natively(notice: &Notice) {
    if let Err(error) = platform::notify(&notice.title, &notice.detail) {
        tracing::warn!(%error, "Could not show the notification");
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn openmouse_icon_rgba() -> Result<(Vec<u8>, u32, u32)> {
    let mut decoder =
        png::Decoder::new(Cursor::new(include_bytes!("../assets/openmouse-logo.png")));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .context("could not decode the OpenMouse logo")?;
    let mut pixels = vec![
        0;
        reader
            .output_buffer_size()
            .expect("logo is within PNG limits")
    ];
    let info = reader
        .next_frame(&mut pixels)
        .context("could not read the OpenMouse logo")?;
    if info.color_type != png::ColorType::Rgba {
        return Err(anyhow!("the OpenMouse logo must decode as RGBA"));
    }
    pixels.truncate(info.buffer_size());
    Ok((pixels, info.width, info.height))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn tray_icon() -> Result<Icon> {
    let (rgba, width, height) = openmouse_icon_rgba()?;
    Icon::from_rgba(rgba, width, height).context("could not create the OpenMouse tray icon")
}

pub(crate) fn open_logs() -> Result<()> {
    let directory = config::log_dir()?;
    #[cfg(target_os = "windows")]
    let program = "explorer.exe";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(directory)
        .spawn()
        .context("Could not open the Bridge log folder")?;
    Ok(())
}

pub(crate) fn open_openmouse() -> Result<()> {
    open_url(OPENMOUSE_URL)
}

#[cfg(target_os = "windows")]
fn open_url(url: &str) -> Result<()> {
    let operation = wide("open");
    let target = wide(url);
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            null_mut(),
            null_mut(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result <= 32 {
        return Err(anyhow!("Windows could not open {url} (code {result})"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_url(url: &str) -> Result<()> {
    let status = std::process::Command::new("open")
        .arg(url)
        .status()
        .with_context(|| format!("macOS could not open {url}"))?;
    if !status.success() {
        return Err(anyhow!("macOS could not open {url}"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_url(url: &str) -> Result<()> {
    let status = std::process::Command::new("xdg-open")
        .arg(url)
        .status()
        .with_context(|| format!("Linux could not open {url}"))?;
    if !status.success() {
        return Err(anyhow!("Linux could not open {url}"));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_exit_does_not_relaunch() {
        assert_eq!(
            next_relaunch_generation(0, Duration::from_secs(5), false),
            None
        );
    }

    #[test]
    fn unexpected_gui_exit_relaunches() {
        assert_eq!(
            next_relaunch_generation(0, Duration::from_secs(5), true),
            Some(1)
        );
    }

    #[test]
    fn a_stable_gui_resets_the_relaunch_counter() {
        assert_eq!(next_relaunch_generation(4, GUI_STABLE_AFTER, true), Some(0));
    }

    #[test]
    fn normal_ticks_are_not_a_resume() {
        let now = SystemTime::now();
        assert_eq!(resume_gap(now - Duration::from_secs(1), now), None);
        assert_eq!(resume_gap(now - Duration::from_secs(5), now), None);
    }

    #[test]
    fn a_long_gap_is_a_resume() {
        let now = SystemTime::now();
        let slept = Duration::from_secs(3600);
        assert_eq!(resume_gap(now - slept, now), Some(slept));
        assert_eq!(
            resume_gap(now - RESUME_GAP, now),
            Some(RESUME_GAP),
            "the threshold itself counts"
        );
    }

    #[test]
    fn a_clock_set_backwards_is_not_a_resume() {
        let now = SystemTime::now();
        assert_eq!(resume_gap(now + Duration::from_secs(3600), now), None);
    }

    #[test]
    fn quick_crash_loops_stop_relaunching() {
        assert_eq!(
            next_relaunch_generation(MAX_QUICK_RELAUNCHES, Duration::from_secs(1), true),
            None
        );
    }
}
