use std::{
    env,
    io::Cursor,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use crate::overlay::Overlay;
use anyhow::{Context, Result, anyhow};
use eframe::egui::{
    self, Align, Button, Color32, Frame, Layout, Pos2, RichText, Sense, Stroke, Vec2,
    ViewportCommand,
};
use openmouse_bridge::{
    BRIDGE_PORT, api, config, platform,
    service::{BridgeService, BridgeSnapshot, DeviceBattery, Notice},
    updater::{self, UpdateInfo},
};
#[cfg(target_os = "windows")]
use std::ptr::null_mut;
use tokio::{
    net::TcpListener,
    sync::{mpsc as tokio_mpsc, oneshot},
};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, Rect as TrayRect, TrayIcon, TrayIconBuilder,
    TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem},
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
const OPENMOUSE_URL: &str = "https://control.openmouse.app";

/// Present when the user explicitly wants no tray panel (or when no display
/// exists): Bridge serves its HTTP API, HID, and game profiles without a
/// window. Checked first so servers and SSH sessions never touch winit.
fn headless_requested() -> bool {
    env::var_os("OPENMOUSE_BRIDGE_HEADLESS").is_some() || env::args().any(|arg| arg == "--headless")
}

/// Whether a display server is reachable for the tray panel.
fn gui_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        (env::var_os("WAYLAND_DISPLAY").is_some() || env::var_os("DISPLAY").is_some())
            && gtk_ready()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// The tray menu is built with GTK, which panics on first use unless it was
/// initialised on this thread. If it cannot start there is no tray, so Bridge
/// runs headless instead of crash-looping (issue #26).
#[cfg(target_os = "linux")]
fn gtk_ready() -> bool {
    match gtk::init() {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "GTK could not start");
            false
        }
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
    _waker: Arc<OnceLock<egui::Context>>,
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

#[cfg(any(target_os = "linux", test))]
fn drain_tray_events(mut pending: impl FnMut() -> bool, mut iterate: impl FnMut()) {
    for _ in 0..32 {
        if !pending() {
            break;
        }
        iterate();
    }
}

const WINDOW_WIDTH: f32 = 360.0;
const WINDOW_HEIGHT: f32 = 280.0;

use crate::native_ui::{ACCENT, BACKGROUND, DANGER, DIVIDER, MUTED, ROW_HEIGHT, TEXT, WARNING};
const BATTERY_THRESHOLDS: [u8; 7] = [10, 15, 20, 25, 30, 40, 50];
/// Glow/winit often tear down after Windows hibernate. If the tray loop ends
/// without an Exit or in-app update, spawn a replacement (issue #16).
const RELAUNCH_ENV: &str = "OPENMOUSE_BRIDGE_RELAUNCH";
const RESUME_GAP: Duration = Duration::from_secs(10);
const GUI_STABLE_AFTER: Duration = Duration::from_secs(30);
const MAX_QUICK_RELAUNCHES: u32 = 5;
const RELAUNCH_DELAY: Duration = Duration::from_secs(2);

fn clamp_panel(position: Pos2, size: Vec2, work_area: egui::Rect) -> Pos2 {
    let min = work_area.min + Vec2::splat(8.0);
    let max = (work_area.max - size - Vec2::splat(8.0)).max(min);
    Pos2::new(
        position.x.clamp(min.x, max.x),
        position.y.clamp(min.y, max.y),
    )
}

fn test_position() -> Option<Pos2> {
    env::var("OPENMOUSE_BRIDGE_TEST_POSITION")
        .ok()
        .and_then(|value| parse_position(&value))
}

fn parse_position(value: &str) -> Option<Pos2> {
    let (x, y) = value.split_once(',')?;
    let position = Pos2::new(x.trim().parse().ok()?, y.trim().parse().ok()?);
    position.is_finite().then_some(position)
}

#[cfg(target_os = "windows")]
fn tray_work_area(rect: TrayRect, scale: f64) -> Option<egui::Rect> {
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect},
    };
    let native_rect = RECT {
        left: rect.position.x as i32,
        top: rect.position.y as i32,
        right: (rect.position.x + f64::from(rect.size.width)) as i32,
        bottom: (rect.position.y + f64::from(rect.size.height)) as i32,
    };
    let monitor = unsafe { MonitorFromRect(&native_rect, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    let work = info.rcWork;
    Some(egui::Rect::from_min_max(
        Pos2::new(
            (f64::from(work.left) / scale) as f32,
            (f64::from(work.top) / scale) as f32,
        ),
        Pos2::new(
            (f64::from(work.right) / scale) as f32,
            (f64::from(work.bottom) / scale) as f32,
        ),
    ))
}

#[cfg(not(target_os = "windows"))]
fn tray_work_area(_rect: TrayRect, _scale: f64) -> Option<egui::Rect> {
    None
}

fn drag_header(ui: &mut egui::Ui, rect: egui::Rect, id: &'static str) {
    let response = ui
        .interact(rect, ui.id().with(id), Sense::drag())
        .on_hover_cursor(egui::CursorIcon::Grab)
        .on_hover_text("Drag to move");
    if response.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
}

fn divider(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, DIVIDER));
}

fn row(ui: &mut egui::Ui, label: &str, trailing: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), ROW_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.label(RichText::new(label).size(13.0).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), trailing);
        },
    );
}

fn service_label(snapshot: Option<&BridgeSnapshot>) -> &'static str {
    if snapshot.is_some() {
        "Bridge service ready"
    } else {
        "Starting Bridge…"
    }
}

fn profile_label(snapshot: Option<&BridgeSnapshot>) -> String {
    if let Some(snapshot) = snapshot {
        if snapshot.pending_native_restores > 0 {
            let mut label = format!(
                "Native restore pending ({})",
                snapshot.pending_native_restores
            );
            if let Some(error) = &snapshot.native_automation_error {
                label.push_str(": ");
                label.push_str(error);
            }
            return label;
        }
        if let Some(error) = &snapshot.native_automation_error {
            return format!("Native settings not applied: {error}");
        }
    }
    snapshot
        .and_then(|snapshot| {
            snapshot
                .active_profile
                .as_ref()
                .map(|profile| (snapshot, profile))
        })
        .map(|(snapshot, profile)| {
            if snapshot.active_profile_is_default {
                "Default profile selected".into()
            } else {
                format!("{} selected", profile.application.name)
            }
        })
        .unwrap_or_else(|| "No game profile active".into())
}

fn device_label(snapshot: Option<&BridgeSnapshot>) -> String {
    snapshot
        .and_then(|snapshot| snapshot.active_devices.first())
        .map(|device| device.name.clone())
        .unwrap_or_else(|| "No mouse session".into())
}

fn battery_text(battery: &openmouse_bridge::service::DeviceBattery) -> String {
    match (battery.stale, battery.charging) {
        (true, true) => format!("{}% · Last known (was charging)", battery.percent),
        (true, false) => format!("{}% · Last known", battery.percent),
        (false, true) => format!("{}% · Charging", battery.percent),
        (false, false) => format!("{}%", battery.percent),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    Settings,
}

fn forget_battery_button(ui: &mut egui::Ui, stale: bool) -> Option<egui::Response> {
    stale.then(|| {
        crate::native_ui::button(ui, "Remove", None, false, 60.0)
            .on_hover_text("Forget this stale battery report")
    })
}

enum BridgeCommand {
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

enum UpdateEvent {
    Checking,
    UpToDate,
    Available(UpdateInfo),
    Downloading(String),
    Restarting(String),
    Failed(String),
}

enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(UpdateInfo),
    Downloading(String),
    Restarting(String),
    Failed(String),
}

fn update_is_notice(state: &UpdateState) -> bool {
    matches!(
        state,
        UpdateState::Available(_)
            | UpdateState::Downloading(_)
            | UpdateState::Restarting(_)
            | UpdateState::Failed(_)
    )
}

/// Height added to the Home view while a device-summary row is showing
/// (the mouse's live onboard-memory settings under the profile panel).
const DEVICE_SUMMARY_HEIGHT: f32 = 22.0;

fn panel_height(
    view: View,
    details: bool,
    saved_values: bool,
    notice: bool,
    device_summary: bool,
) -> f32 {
    if view == View::Settings {
        return 420.0;
    }
    if details {
        return 340.0;
    }
    let base =
        (WINDOW_HEIGHT + if saved_values { 18.0 } else { 0.0 } + if notice { 40.0 } else { 0.0 })
            .min(340.0);
    base + if device_summary {
        DEVICE_SUMMARY_HEIGHT
    } else {
        0.0
    }
}

/// The battery entry whose onboard-memory settings the Home view renders
/// under the profile panel — the freshest non-stale one carrying any. Both
/// the renderer and the window sizing go through this so they stay in step.
fn device_summary_battery(snapshot: Option<&BridgeSnapshot>) -> Option<&DeviceBattery> {
    let snapshot = snapshot?;
    snapshot
        .batteries
        .iter()
        .find(|battery| !battery.stale)
        .filter(|battery| {
            battery.dpi.is_some()
                || battery.polling_rate_hz.is_some()
                || battery.switch_mode.is_some()
        })
}

#[derive(Clone, Copy)]
enum TrayAction {
    ShowWindow,
    ToggleWindow { rect: TrayRect },
    Restart,
    Exit,
}

/// The battery display currently swapped into the tray icon: `None` keeps
/// the static OpenMouse logo. Every field here feeds the drawing or the
/// tooltip, so comparing against the next snapshot redraws exactly when
/// the level, charge state, estimate, or size changed — and stops the
/// per-second updates from churning the icon when nothing did.
#[derive(Clone, PartialEq, Eq)]
struct TrayBattery {
    device_name: String,
    percent: u8,
    charging: bool,
    stale: bool,
    /// The Segoe glyph's ink, from `battery_ink` (green charged/charging,
    /// amber mid, alert red when low, gray when stale).
    ink: Color32,
    minutes_remaining: Option<u32>,
    /// The notification area's icon size in physical pixels at the current
    /// DPI; Windows reports it, so a display-scaling change redraws.
    size: u32,
    /// What fills the square: the battery glyph or the percentage number
    /// (the control panel's tray-icon style).
    style: platform::TrayBatteryStyle,
}

/// The battery glyph's ink, by charge state: green while charging or above
/// half, amber through the mid band, the alert red at or below the
/// low-battery threshold, and gray when the reading is stale.
fn battery_ink(percent: u8, charging: bool, stale: bool, threshold: u8) -> Color32 {
    use crate::native_ui::{ACCENT, DANGER, MUTED, WARNING};
    if stale {
        MUTED
    } else if charging {
        ACCENT
    } else if percent <= threshold {
        DANGER
    } else if percent <= 50 {
        WARNING
    } else {
        ACCENT
    }
}

/// Renders the live tray icon from a cached battery display: either the
/// Segoe Fluent battery glyph (the charging variant with the bolt when
/// plugged in) or the percentage number, per the panel's tray-icon style,
/// tinted by charge state — green charged/charging, amber mid, red low,
/// gray stale.
fn battery_tray_icon(battery: &TrayBattery) -> Result<Icon> {
    let (rgba, width, height) = platform::battery_icon_rgba(
        battery.percent,
        battery.charging,
        battery.style,
        battery.size,
        (battery.ink.r(), battery.ink.g(), battery.ink.b()),
    )?;
    Icon::from_rgba(rgba, width, height).context("could not create the battery tray icon")
}

/// Formats a rough duration for the tooltip: "45 m", "3 h 20 m", "1 d 4 h".
fn remaining_text(minutes: u32) -> String {
    let days = minutes / (24 * 60);
    let hours = minutes % (24 * 60) / 60;
    let minutes = minutes % 60;
    if days > 0 {
        if hours > 0 {
            format!("{days} d {hours} h")
        } else {
            format!("{days} d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours} h {minutes} m")
        } else {
            format!("{hours} h")
        }
    } else {
        format!("{minutes} m")
    }
}

fn tray_menu_action(open: bool, restart: bool, quit: bool) -> Option<TrayAction> {
    if open {
        Some(TrayAction::ShowWindow)
    } else if restart {
        Some(TrayAction::Restart)
    } else if quit {
        Some(TrayAction::Exit)
    } else {
        None
    }
}

struct TrayState {
    icon: TrayIcon,
    menu: Menu,
    events: Receiver<TrayAction>,
}

fn build_tray_icon(menu: &Menu) -> Result<TrayIcon> {
    TrayIconBuilder::new()
        .with_tooltip("OpenMouse Bridge")
        .with_icon(tray_icon().context("could not create the tray icon image")?)
        .with_menu(Box::new(menu.clone()))
        .with_menu_on_left_click(false)
        .build()
        .context("could not create the system tray icon")
}

impl TrayState {
    fn new(context: &egui::Context) -> Result<Self> {
        let menu = Menu::new();
        let open_item = MenuItem::new("Open OpenMouse Bridge", true, None);
        menu.append(&open_item)
            .context("could not add Open to the tray menu")?;
        let open_id = open_item.id().clone();
        let restart_item = MenuItem::new("Restart", true, None);
        let exit_item = MenuItem::new("Quit Bridge", true, None);
        menu.append(&restart_item)
            .context("could not add Restart to the tray menu")?;
        menu.append(&exit_item)
            .context("could not add Exit to the tray menu")?;
        let restart_id = restart_item.id().clone();
        let exit_id = exit_item.id().clone();

        let icon = build_tray_icon(&menu)?;

        let (event_tx, events) = mpsc::channel();
        let tray_event_tx = event_tx.clone();
        let tray_repaint = context.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            tracing::debug!(?event, "received tray event");
            if let TrayIconEvent::Click {
                rect,
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = tray_event_tx.send(TrayAction::ToggleWindow { rect });
                tray_repaint.request_repaint();
            }
        }));

        let menu_repaint = context.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            tracing::debug!(?event, "received tray menu event");
            let Some(action) = tray_menu_action(
                event.id == open_id,
                event.id == restart_id,
                event.id == exit_id,
            ) else {
                return;
            };
            let _ = event_tx.send(action);
            menu_repaint.request_repaint();
        }));

        Ok(Self { icon, menu, events })
    }

    /// Windows can drop the notification-area icon across sleep/hibernate
    /// while the process keeps running; add a fresh one.
    fn recreate_icon(&mut self) -> Result<()> {
        self.icon = build_tray_icon(&self.menu)?;
        Ok(())
    }

    /// Swaps the tray icon for the live battery display: the battery glyph
    /// drawn from `battery`'s level, theme, and state colors, with a tooltip
    /// that spells the percentage out on hover. The previous icon's HICON is
    /// destroyed by the tray backend when the new one replaces it, so
    /// redrawing never leaks GDI handles.
    fn set_battery_display(&mut self, battery: &TrayBattery, tooltip: &str) -> Result<()> {
        let icon = battery_tray_icon(battery)?;
        self.icon
            .set_icon(Some(icon))
            .context("could not swap in the battery tray icon")?;
        self.icon
            .set_tooltip(Some(tooltip.to_owned()))
            .context("could not update the battery tooltip")?;
        Ok(())
    }

    /// Restores the static OpenMouse logo and its plain tooltip.
    fn set_default_display(&mut self) -> Result<()> {
        self.icon
            .set_icon(Some(tray_icon()?))
            .context("could not restore the OpenMouse tray icon")?;
        self.icon
            .set_tooltip(Some("OpenMouse Bridge"))
            .context("could not restore the tray tooltip")?;
        Ok(())
    }

    fn next_action(&self) -> Option<TrayAction> {
        self.events.try_recv().ok()
    }
}

struct TrayApp {
    tray: TrayState,
    logo: egui::TextureHandle,
    snapshots: Receiver<BridgeSnapshot>,
    commands: tokio_mpsc::UnboundedSender<BridgeCommand>,
    updates: Receiver<UpdateEvent>,
    notices: Receiver<Notice>,
    overlay: Option<Overlay>,
    snapshot: Option<BridgeSnapshot>,
    view: View,
    details_open: bool,
    requested_height: f32,
    visible: bool,
    visibility_initialized: bool,
    panel_had_focus: bool,
    exit_requested: bool,
    keep_running: Arc<AtomicBool>,
    autostart: bool,
    battery_threshold: u8,
    tray_battery: Option<TrayBattery>,
    tray_style: platform::TrayBatteryStyle,
    hybrid_mode: bool,
    automatic_updates: bool,
    notification_sound: bool,
    notification_volume: u8,
    // While the volume slider is dragged, snapshots would snap it back to the
    // saved value; it is saved once released.
    adjusting_volume: bool,
    last_tick: SystemTime,
    panel_requests: Receiver<()>,
    update_state: UpdateState,
    last_error: Option<String>,
}

impl TrayApp {
    // Seven tray/runtime handles plus the persist flag: a genuine constructor,
    // not a helper worth splitting into a params struct for one call site.
    #[allow(clippy::too_many_arguments)]
    fn new(
        context: &egui::Context,
        snapshots: Receiver<BridgeSnapshot>,
        commands: tokio_mpsc::UnboundedSender<BridgeCommand>,
        updates: Receiver<UpdateEvent>,
        notices: Receiver<Notice>,
        panel_requests: Receiver<()>,
        waker: &OnceLock<egui::Context>,
        keep_running: Arc<AtomicBool>,
        visible: bool,
        acknowledge_tray_hint: bool,
    ) -> Result<Self> {
        configure_tray_only_application();
        let _ = waker.set(context.clone());
        if acknowledge_tray_hint {
            let _ = commands.send(BridgeCommand::AcknowledgeTrayHint);
        }
        // Without the overlay, notices fall back to system notifications.
        let overlay = Overlay::new()
            .inspect_err(|error| tracing::warn!(%error, "Could not create the overlay"))
            .ok();
        crate::native_ui::install(context);
        let (logo_pixels, logo_width, logo_height) = openmouse_icon_rgba()?;
        let logo = context.load_texture(
            "openmouse-logo",
            egui::ColorImage::from_rgba_unmultiplied(
                [logo_width as usize, logo_height as usize],
                &logo_pixels,
            ),
            egui::TextureOptions::LINEAR,
        );
        Ok(Self {
            tray: TrayState::new(context)?,
            logo,
            snapshots,
            commands,
            updates,
            notices,
            overlay,
            snapshot: None,
            view: View::Home,
            details_open: false,
            requested_height: WINDOW_HEIGHT,
            visible,
            visibility_initialized: false,
            panel_had_focus: false,
            exit_requested: false,
            keep_running,
            autostart: platform::autostart_enabled(),
            battery_threshold: 20,
            tray_battery: None,
            tray_style: platform::TrayBatteryStyle::Glyph,
            hybrid_mode: true,
            automatic_updates: false,
            notification_sound: true,
            notification_volume: 60,
            adjusting_volume: false,
            last_tick: SystemTime::now(),
            panel_requests,
            update_state: UpdateState::Idle,
            last_error: None,
        })
    }

    fn set_visible(&mut self, context: &egui::Context, visible: bool) {
        self.visible = visible;
        self.panel_had_focus = false;
        context.send_viewport_cmd(ViewportCommand::Visible(visible));
        if visible {
            self.show_view(context, View::Home);
            if test_position().is_none() {
                context.send_viewport_cmd(ViewportCommand::Focus);
            }
        }
    }

    fn request_exit(&mut self, context: &egui::Context) {
        self.exit_requested = true;
        context.send_viewport_cmd(ViewportCommand::Close);
    }

    fn show_near_tray(&mut self, context: &egui::Context, rect: TrayRect) {
        self.set_visible(context, true);
        let height = self.requested_height;
        if let Some(position) = test_position() {
            context.send_viewport_cmd(ViewportCommand::OuterPosition(position));
            return;
        }
        let scale = f64::from(context.pixels_per_point());
        let x = (rect.position.x + f64::from(rect.size.width)) / scale - f64::from(WINDOW_WIDTH);
        #[cfg(target_os = "macos")]
        let y = (rect.position.y + f64::from(rect.size.height)) / scale + 8.0;
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        let y = rect.position.y / scale - f64::from(height) - 8.0;
        let position = Pos2::new(x as f32, y as f32);
        let position = tray_work_area(rect, scale)
            .map(|work| clamp_panel(position, Vec2::new(WINDOW_WIDTH, height), work))
            .unwrap_or(position);
        context.send_viewport_cmd(ViewportCommand::OuterPosition(position));
    }

    fn show_view(&mut self, _context: &egui::Context, view: View) {
        self.view = view;
        self.resize_panel(_context);
    }

    fn refresh_snapshot(&mut self) {
        while let Ok(snapshot) = self.snapshots.try_recv() {
            self.autostart = snapshot.autostart_enabled;
            self.battery_threshold = snapshot.battery_threshold_percent;
            self.tray_style =
                platform::TrayBatteryStyle::from_config_str(&snapshot.tray_battery_style);
            self.hybrid_mode = snapshot.hybrid_mode;
            self.automatic_updates = snapshot.automatic_updates;
            self.notification_sound = snapshot.notification_sound;
            if !self.adjusting_volume {
                self.notification_volume = snapshot.notification_volume;
            }
            self.snapshot = Some(snapshot);
        }
        self.refresh_tray_battery_display(false);
    }

    /// Refreshes the tray icon: a live battery glyph showing the mouse's
    /// charge (or the percentage number, per the panel's tray-icon style)
    /// while any battery is known, the static OpenMouse logo otherwise.
    /// Redraws only when the display would change unless `force` — used
    /// after icon recreation, when the fresh icon is the plain logo again.
    fn refresh_tray_battery_display(&mut self, force: bool) {
        let battery = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .batteries
                .iter()
                .find(|battery| !battery.stale)
                .or_else(|| snapshot.batteries.first())
        });
        let shown = battery.map(|battery| TrayBattery {
            device_name: battery.device_name.clone(),
            percent: battery.percent.min(100),
            charging: battery.charging,
            stale: battery.stale,
            ink: battery_ink(
                battery.percent.min(100),
                battery.charging,
                battery.stale,
                self.battery_threshold,
            ),
            minutes_remaining: battery.minutes_remaining,
            size: platform::tray_icon_size(),
            style: self.tray_style,
        });
        if !force && shown == self.tray_battery {
            return;
        }
        let outcome = match &shown {
            Some(battery) => {
                let status = match (battery.charging, battery.stale) {
                    (true, _) => " · Charging",
                    (false, true) => " · Not updating",
                    (false, false) => "",
                };
                // Windows' tooltip buffer tops out at 128 UTF-16 units, and
                // device names can be long.
                let eta = battery.minutes_remaining.map(|minutes| {
                    if battery.charging {
                        format!("full in {}", remaining_text(minutes))
                    } else {
                        format!("{} left", remaining_text(minutes))
                    }
                });
                let tooltip = match eta {
                    Some(eta) => {
                        format!(
                            "{} — {}%{status} · {eta}",
                            battery.device_name, battery.percent
                        )
                    }
                    None => format!("{} — {}%{status}", battery.device_name, battery.percent),
                };
                self.tray
                    .set_battery_display(battery, &tooltip.chars().take(120).collect::<String>())
            }
            None => self.tray.set_default_display(),
        };
        match outcome {
            Ok(()) => self.tray_battery = shown,
            Err(error) => tracing::warn!(%error, "Could not update the tray battery display"),
        }
    }

    /// Shows pending notices and advances the banner's fade.
    /// Returns how soon the banner needs the next frame.
    fn refresh_overlay(&mut self) -> Option<Duration> {
        while let Ok(notice) = self.notices.try_recv() {
            self.present(&notice);
        }
        self.overlay.as_mut()?.tick()
    }

    /// Shows `notice` on the overlay with its chime, or as a system
    /// notification (with the system's own sound) when there is no overlay.
    fn present(&mut self, notice: &Notice) {
        let Some(overlay) = self.overlay.as_mut() else {
            notify_natively(notice);
            return;
        };
        if let Err(error) = overlay.show(&notice.title, &notice.detail) {
            tracing::warn!(%error, "Could not show the overlay");
            notify_natively(notice);
            return;
        }
        tracing::info!(
            title = %notice.title,
            sound = self.notification_sound,
            volume = self.notification_volume,
            "Showed a notice on the overlay"
        );
        if self.notification_sound
            && let Err(error) = overlay.chime(self.notification_volume)
        {
            tracing::warn!(%error, "Could not play the notification sound");
        }
    }

    fn refresh_updates(&mut self) -> bool {
        let mut restart = false;
        while let Ok(event) = self.updates.try_recv() {
            self.update_state = match event {
                UpdateEvent::Checking => UpdateState::Checking,
                UpdateEvent::UpToDate => UpdateState::UpToDate,
                UpdateEvent::Available(update) => UpdateState::Available(update),
                UpdateEvent::Downloading(version) => UpdateState::Downloading(version),
                UpdateEvent::Restarting(version) => {
                    restart = true;
                    UpdateState::Restarting(version)
                }
                UpdateEvent::Failed(error) => UpdateState::Failed(error),
            };
        }
        restart
    }
}

fn should_hide_on_escape(visible: bool, context: &egui::Context) -> bool {
    visible
        && context.input(|input| input.key_pressed(egui::Key::Escape))
        && !egui::Popup::is_any_open(context)
}

impl eframe::App for TrayApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "linux")]
        drain_tray_events(gtk::events_pending, || {
            gtk::main_iteration_do(false);
        });
        self.restore_tray_after_resume();
        self.refresh_snapshot();
        self.resize_panel(context);
        if self.refresh_updates() {
            // The updater has already spawned its own post-exit relaunch helper.
            self.keep_running.store(false, Ordering::Relaxed);
            self.request_exit(context);
            return;
        }
        if !self.visibility_initialized {
            self.visibility_initialized = true;
            context.send_viewport_cmd(ViewportCommand::Visible(self.visible));
            if self.visible && test_position().is_none() {
                context.send_viewport_cmd(ViewportCommand::Focus);
            }
        }
        while self.panel_requests.try_recv().is_ok() {
            tracing::debug!("opening the tray panel for another Bridge launch");
            self.set_visible(context, true);
        }
        while let Some(action) = self.tray.next_action() {
            match action {
                TrayAction::ShowWindow => self.set_visible(context, true),
                TrayAction::ToggleWindow { rect } => {
                    tracing::debug!(?rect, visible = self.visible, "handling tray toggle");
                    if self.visible {
                        self.set_visible(context, false);
                    } else {
                        self.show_near_tray(context, rect);
                    }
                }
                TrayAction::Restart => {
                    self.request_exit(context);
                    break;
                }
                TrayAction::Exit => {
                    self.keep_running.store(false, Ordering::Relaxed);
                    self.request_exit(context);
                    break;
                }
            }
        }
        if should_hide_on_escape(self.visible, context) {
            self.set_visible(context, false);
        }
        if self.visible {
            match context.input(|input| input.viewport().focused) {
                Some(true) => self.panel_had_focus = true,
                Some(false) if self.panel_had_focus => self.set_visible(context, false),
                _ => {}
            }
        }
        if context.input(|input| input.viewport().close_requested()) && !self.exit_requested {
            context.send_viewport_cmd(ViewportCommand::CancelClose);
            self.set_visible(context, false);
        }
        let mut next = Duration::from_secs(1);
        if let Some(fade) = self.refresh_overlay() {
            next = next.min(fade);
        }
        context.request_repaint_after(next);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.spacing_mut().item_spacing.y = 0.0;
        Frame::NONE
            .fill(BACKGROUND)
            .stroke(Stroke::new(1.0, DIVIDER))
            .show(ui, |ui| {
                self.titlebar(ui);
                Frame::NONE.inner_margin(12).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    match self.view {
                        View::Home => self.home_view(ui),
                        View::Settings => self.settings_view(ui),
                    }
                });
            });
    }
}

impl TrayApp {
    /// `logic` runs at least once a second, so a long wall-clock gap means the
    /// PC slept or hibernated; the tray icon may be gone after that.
    fn restore_tray_after_resume(&mut self) {
        let now = SystemTime::now();
        let previous = std::mem::replace(&mut self.last_tick, now);
        let Some(gap) = resume_gap(previous, now) else {
            return;
        };
        tracing::info!(?gap, "Resumed from sleep; recreating the tray icon");
        if let Err(error) = self.tray.recreate_icon() {
            tracing::error!(%error, "could not recreate the tray icon after resume");
        }
        // The fresh icon is the plain logo; put the battery display back.
        self.refresh_tray_battery_display(true);
    }

    fn titlebar(&mut self, ui: &mut egui::Ui) {
        use crate::native_ui as n;
        let mut controls = Vec::new();
        let header = Frame::NONE
            .fill(n::CHROME)
            .inner_margin(egui::Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(3.0, 0.0);
                ui.horizontal(|ui| {
                    ui.set_height(28.0);
                    if self.view == View::Settings {
                        let back = n::icon_button(ui, n::Icon::Back, "Back to Home");
                        controls.push(back.rect);
                        if back.clicked() {
                            self.show_view(ui.ctx(), View::Home);
                        }
                        ui.label(RichText::new("Settings").size(13.0).strong());
                    } else {
                        ui.add(egui::Image::new((self.logo.id(), Vec2::new(12.0, 18.0))));
                        ui.add_space(4.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new("OpenMouse Bridge").size(12.5).strong());
                            ui.label(
                                RichText::new(format!("v{}", openmouse_bridge::BRIDGE_VERSION))
                                    .size(10.0)
                                    .color(n::QUIET),
                            );
                        });
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let close = n::icon_button(
                            ui,
                            n::Icon::Close,
                            "Close to tray: Bridge keeps running",
                        );
                        controls.push(close.rect);
                        if close.clicked() {
                            self.set_visible(ui.ctx(), false);
                        }
                        let minimize = n::icon_button(
                            ui,
                            n::Icon::Minimize,
                            "Minimize to tray: Bridge keeps running",
                        );
                        controls.push(minimize.rect);
                        if minimize.clicked() {
                            self.set_visible(ui.ctx(), false);
                        }
                        if self.view == View::Home {
                            let settings = n::icon_button(ui, n::Icon::Gear, "Settings");
                            controls.push(settings.rect);
                            if settings.clicked() {
                                self.show_view(ui.ctx(), View::Settings);
                            }
                        }
                    });
                });
            });
        controls.sort_by(|a, b| a.left().total_cmp(&b.left()));
        let mut left = header.response.rect.left();
        for (index, control) in controls.iter().enumerate() {
            let rect = egui::Rect::from_min_max(
                Pos2::new(left, header.response.rect.top()),
                Pos2::new(control.left(), header.response.rect.bottom()),
            );
            if rect.width() > 1.0 {
                ui.push_id(index, |ui| drag_header(ui, rect, "titlebar-drag"));
            }
            left = control.right();
        }
        if left < header.response.rect.right() {
            let rect = egui::Rect::from_min_max(
                Pos2::new(left, header.response.rect.top()),
                header.response.rect.max,
            );
            ui.push_id("titlebar-end", |ui| drag_header(ui, rect, "titlebar-drag"));
        }
        divider(ui);
    }

    fn home_view(&mut self, ui: &mut egui::Ui) {
        use crate::native_ui as n;
        let starting = self.snapshot.is_none();
        let connected = self.snapshot.as_ref().is_some_and(|s| s.client_connected);
        let device = self
            .snapshot
            .as_ref()
            .and_then(|s| s.active_devices.first())
            .cloned();
        let device_count = self.snapshot.as_ref().map_or(0, |s| s.active_devices.len());
        let profile = self
            .snapshot
            .as_ref()
            .and_then(|s| s.active_profile.clone());
        let is_default = self
            .snapshot
            .as_ref()
            .is_some_and(|s| s.active_profile_is_default);
        let profile_count = self.snapshot.as_ref().map_or(0, |s| s.profile_count);
        egui::ScrollArea::vertical().id_salt("home-content").auto_shrink([false, false])
            .max_height((ui.available_height() - 46.0).max(80.0)).show(ui, |ui| {
                if let Some(error) = &self.last_error { ui.label(RichText::new(error).size(12.0).color(DANGER)); }
                n::panel(ui, true, 12, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
                        ui.painter().rect_filled(rect, 8, n::CHROME);
                        ui.painter().rect_stroke(rect, 8, Stroke::new(1.0, DIVIDER), egui::StrokeKind::Inside);
                        n::icon(ui.painter(), rect, n::Icon::Mouse, if device.is_some() { ACCENT } else { MUTED });
                        ui.add_space(4.0);
                        ui.vertical(|ui| {
                            ui.add(egui::Label::new(RichText::new(if starting { "Starting Bridge…" } else { device.as_ref().map_or("Ready to connect", |d| d.name.as_str()) }).size(17.0).strong()).truncate());
                            ui.label(RichText::new(if starting { "Waiting for service" } else if device.is_some() { "Mouse session open" } else { "Connect a mouse in OpenMouse." }).size(12.0).color(MUTED));
                        });
                    });
                    if device_count > 1 {
                        ui.label(RichText::new(format!("+{} other open sessions", device_count - 1)).size(11.0).color(MUTED));
                    }
                });
                ui.add_space(4.0);
                n::panel(ui, false, 5, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 2.5, if starting { MUTED } else { ACCENT });
                        ui.label(RichText::new(if starting { "Starting" } else { "Bridge ready" }).size(11.5).color(MUTED));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if n::icon_button(ui, n::Icon::Info, "Connection details").clicked() { self.details_open = !self.details_open; }
                            ui.label(RichText::new(if connected { "Panel connected" } else { "Panel offline" }).size(11.5).color(if connected { TEXT } else { MUTED }));
                        });
                    });
                });
                ui.add_space(4.0);
                n::panel(ui, false, 10, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (icon, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                        n::icon(ui.painter(), icon, n::Icon::Game, MUTED);
                        ui.label(RichText::new("Mouse profile").size(12.0).color(MUTED));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(profile.as_ref().map_or("None selected", |p| if is_default { "Default profile" } else { p.application.name.as_str() })).size(12.5).strong()).truncate())
                                .on_hover_text(profile.as_ref().map_or_else(|| format!("{profile_count} saved profiles. Profiles are configured in OpenMouse."), |p| format!("Selected for {}. Hardware application is not confirmed here.", p.device.name)));
                        });
                    });
                    if let Some(profile) = &profile {
                        let mut values = Vec::new();
                        if let Some(dpi) = profile.settings.dpi { values.push(format!("{dpi} DPI")); }
                        if let Some(rate) = profile.settings.polling_rate_hz { values.push(format!("{rate} Hz")); }
                        if !values.is_empty() { ui.label(RichText::new(format!("Saved: {}", values.join(" · "))).size(12.0).color(MUTED)); }
                    }
                    // The mouse's own onboard-memory settings, read natively
                    // wherever its driver can see them — the details someone
                    // changing DPI or polling rate actually needs, profile or
                    // no profile.
                    if let Some(device) = device_summary_battery(self.snapshot.as_ref()) {
                        let mut values = Vec::new();
                        if let Some(dpi) = device.dpi { values.push(format!("{dpi} DPI")); }
                        if let Some(rate) = device.polling_rate_hz { values.push(format!("{rate} Hz")); }
                        if let Some(mode) = &device.switch_mode { values.push(mode.clone()); }
                        if !values.is_empty() {
                            ui.label(RichText::new(format!("Onboard: {}", values.join(" · "))).size(12.0).color(MUTED));
                        }
                    }
                    if self.snapshot.as_ref().is_some_and(|snapshot| snapshot.native_automation_error.is_some() || snapshot.pending_native_restores > 0) {
                        ui.label(RichText::new(profile_label(self.snapshot.as_ref())).size(12.0).color(DANGER));
                    }
                });
                if update_is_notice(&self.update_state) { ui.add_space(4.0); self.update_row(ui); }
                if self.details_open { self.diagnostics(ui); }
            });
        ui.add_space(8.0);
        if n::button(
            ui,
            "Open control panel",
            Some(n::Icon::External),
            true,
            ui.available_width(),
        )
        .clicked()
        {
            self.last_error = open_openmouse().err().map(|error| error.to_string());
        }
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        use crate::native_ui as n;
        egui::ScrollArea::vertical()
            .id_salt("settings-content")
            .auto_shrink([false, false])
            .max_height((ui.available_height() - 36.0).max(80.0))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if let Some(error) = &self.last_error {
                    ui.label(RichText::new(error).size(12.0).color(DANGER));
                }
                n::panel(ui, false, 8, |ui| {
                    ui.set_width(ui.available_width());
                    let mut enabled = self.autostart;
                    if n::switch_row(ui, "Launch at login", &mut enabled)
                        .on_hover_text("Keep Bridge in the tray when you sign in.")
                        .changed()
                    {
                        match platform::set_autostart(enabled) {
                            Ok(()) => {
                                self.autostart = enabled;
                                self.last_error = None;
                            }
                            Err(error) => self.last_error = Some(error.to_string()),
                        }
                    }
                });
                ui.add_space(6.0);
                n::panel(ui, false, 8, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new("Device mode")
                            .size(12.0)
                            .strong()
                            .color(MUTED),
                    );
                    let mut enabled = self.hybrid_mode;
                    if n::switch_row(ui, "Hybrid", &mut enabled)
                        .on_hover_text(
                            "While the web panel is closed, Bridge applies the mouse's stored settings and reads the battery itself.",
                        )
                        .changed()
                        && self.send_command(
                            BridgeCommand::SetHybridMode(enabled),
                            "Bridge settings service is unavailable",
                        )
                    {
                        self.hybrid_mode = enabled;
                    }
                    ui.label(
                        RichText::new(if self.hybrid_mode {
                            "While the web panel is closed, Bridge applies the mouse's \
                             stored settings and reads the battery itself"
                        } else {
                            "Only the web panel changes the mouse"
                        })
                        .size(11.0)
                        .color(MUTED),
                    );
                    divider(ui);
                    let mut selected_style = self.tray_style;
                    row(ui, "Battery display", |ui| {
                        let choice = egui::ComboBox::from_id_salt("tray-battery-style")
                            .width(110.0)
                            .selected_text(
                                RichText::new(match self.tray_style {
                                    platform::TrayBatteryStyle::Glyph => "Battery glyph",
                                    platform::TrayBatteryStyle::Percentage => "Percentage",
                                })
                                .size(12.0),
                            )
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut selected_style,
                                    platform::TrayBatteryStyle::Glyph,
                                    "Battery glyph",
                                );
                                ui.selectable_value(
                                    &mut selected_style,
                                    platform::TrayBatteryStyle::Percentage,
                                    "Percentage",
                                );
                            });
                        choice.response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::ComboBox,
                                ui.is_enabled(),
                                "Battery display",
                            )
                        });
                    });
                    if selected_style != self.tray_style
                        && self.send_command(
                            BridgeCommand::SetTrayBatteryStyle(selected_style),
                            "Bridge settings service is unavailable",
                        )
                    {
                        self.tray_style = selected_style;
                        // Redraw the tray at once; the service's next snapshot
                        // only confirms what is already on screen.
                        self.refresh_tray_battery_display(true);
                    }
                });
                ui.add_space(6.0);
                n::panel(ui, false, 8, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new("Notifications")
                            .size(12.0)
                            .strong()
                            .color(MUTED),
                    );
                    let mut threshold = self.battery_threshold;
                    row(ui, "Low battery alert", |ui| {
                        let choice = egui::ComboBox::from_id_salt("battery-threshold")
                            .width(62.0)
                            .selected_text(format!("{threshold}%"))
                            .show_ui(ui, |ui| {
                                for percent in BATTERY_THRESHOLDS {
                                    if ui
                                        .selectable_value(
                                            &mut threshold,
                                            percent,
                                            format!("{percent}%"),
                                        )
                                        .clicked()
                                    {
                                        ui.close();
                                    }
                                }
                            });
                        choice.response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::ComboBox,
                                ui.is_enabled(),
                                "Low battery alert",
                            )
                        });
                        choice
                            .response
                            .on_hover_text("Notify at or below this reported charge.");
                    });
                    if threshold != self.battery_threshold
                        && self.send_command(
                            BridgeCommand::SetBatteryThreshold(threshold),
                            "Battery alert setting could not be sent",
                        )
                    {
                        self.battery_threshold = threshold;
                    }
                    divider(ui);
                    let mut sound = self.notification_sound;
                    if n::switch_row(ui, "Notification sound", &mut sound)
                        .on_hover_text("Play a chime with profile and battery notices.")
                        .changed()
                        && self.send_command(
                            BridgeCommand::SetNotificationSound(sound),
                            "Sound setting could not be sent",
                        )
                    {
                        self.notification_sound = sound;
                    }
                    divider(ui);
                    let mut released = false;
                    row(ui, "Volume", |ui| {
                        ui.add_enabled_ui(self.notification_sound, |ui| {
                            ui.label(
                                RichText::new(format!("{}%", self.notification_volume))
                                    .size(12.0)
                                    .color(MUTED),
                            );
                            ui.visuals_mut().selection.bg_fill = ACCENT;
                            ui.spacing_mut().slider_width = 108.0;
                            let response = ui.add(
                                egui::Slider::new(&mut self.notification_volume, 0..=100)
                                    .show_value(false)
                                    .trailing_fill(true),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Slider,
                                    ui.is_enabled(),
                                    "Notification volume",
                                )
                            });
                            self.adjusting_volume = response.is_pointer_button_down_on();
                            released = response.drag_stopped()
                                || (response.changed() && !self.adjusting_volume);
                        });
                    });
                    if released {
                        self.send_command(
                            BridgeCommand::SetNotificationVolume(self.notification_volume),
                            "Volume setting could not be sent",
                        );
                    }
                    divider(ui);
                    let mut test = false;
                    row(ui, "Test notification", |ui| {
                        test = n::button(ui, "Test", None, false, 48.0).clicked();
                    });
                    if test {
                        self.present(&Notice {
                            title: "OpenMouse Bridge".into(),
                            detail: "This is how notifications look".into(),
                        });
                        ui.ctx().request_repaint();
                    }
                });
                ui.add_space(6.0);
                n::panel(ui, false, 8, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Updates").size(12.0).strong().color(MUTED));
                    let mut automatic = self.automatic_updates;
                    if n::switch_row(ui, "Automatic updates", &mut automatic)
                        .on_hover_text(
                            "Download, install and restart Bridge when an update is found.",
                        )
                        .changed()
                        && self.send_command(
                            BridgeCommand::SetAutomaticUpdates(automatic),
                            "Update preference could not be sent",
                        )
                    {
                        self.automatic_updates = automatic;
                    }
                    divider(ui);
                    self.update_row(ui);
                });
                if self.details_open {
                    self.diagnostics(ui);
                }
            });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if n::button(ui, "Open logs", Some(n::Icon::Logs), false, 94.0).clicked() {
                self.last_error = open_logs().err().map(|e| e.to_string());
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        Button::new(RichText::new("Quit Bridge").size(12.0).color(DANGER))
                            .min_size(Vec2::new(84.0, 26.0)),
                    )
                    .clicked()
                {
                    self.keep_running.store(false, Ordering::Relaxed);
                    self.request_exit(ui.ctx());
                }
                if n::icon_button(ui, n::Icon::Info, "Connection details").clicked() {
                    self.details_open = !self.details_open;
                }
            });
        });
    }

    fn update_row(&mut self, ui: &mut egui::Ui) {
        use crate::native_ui as n;
        let (label, action): (String, Option<&str>) = match &self.update_state {
            UpdateState::Idle => ("Not checked yet".into(), Some("Check")),
            UpdateState::Checking => ("Checking…".into(), None),
            UpdateState::UpToDate => ("Up to date".into(), Some("Check")),
            UpdateState::Available(update) => {
                (format!("{} available", update.version), Some("Install"))
            }
            UpdateState::Downloading(version) => (format!("Downloading {version}…"), None),
            UpdateState::Restarting(version) => (format!("Restarting {version}…"), None),
            UpdateState::Failed(_) => ("Update check failed".into(), Some("Retry")),
        };
        let mut clicked = false;
        row(ui, &label, |ui| {
            if let Some(action) = action {
                clicked = n::button(ui, action, None, false, 60.0).clicked();
            }
        });
        if let UpdateState::Failed(error) = &self.update_state {
            ui.add(egui::Label::new(RichText::new(error).size(11.0).color(DANGER)).truncate())
                .on_hover_text(error);
        }
        if clicked {
            let command = match &self.update_state {
                UpdateState::Available(update) => BridgeCommand::InstallUpdate(update.clone()),
                _ => BridgeCommand::CheckForUpdates,
            };
            self.send_command(command, "Bridge update service is unavailable");
        }
    }

    fn diagnostics(&self, ui: &mut egui::Ui) {
        use crate::native_ui as n;
        ui.add_space(6.0);
        n::panel(ui, false, 8, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Connection details").size(12.0).strong());
            ui.label(
                RichText::new(service_label(self.snapshot.as_ref()))
                    .size(11.5)
                    .color(MUTED),
            );
            ui.label(
                RichText::new(format!(
                    "{} · 127.0.0.1:{BRIDGE_PORT}",
                    platform::platform_name()
                ))
                .size(11.0)
                .color(MUTED),
            );
            if let Some(snapshot) = &self.snapshot {
                ui.label(
                    RichText::new(device_label(Some(snapshot)))
                        .size(11.5)
                        .color(MUTED),
                );
                ui.label(
                    RichText::new(profile_label(Some(snapshot)))
                        .size(11.5)
                        .color(MUTED),
                );
                for device in &snapshot.active_devices {
                    ui.label(
                        RichText::new(format!(
                            "{} · {:04x}:{:04x}",
                            device.name, device.vendor_id, device.product_id
                        ))
                        .size(11.0)
                        .color(MUTED),
                    );
                }
                for battery in &snapshot.batteries {
                    let color = if !battery.stale
                        && !battery.charging
                        && battery.percent <= self.battery_threshold
                    {
                        WARNING
                    } else {
                        MUTED
                    };
                    ui.horizontal_wrapped(|ui| {
                        if forget_battery_button(ui, battery.stale)
                            .is_some_and(|response| response.clicked())
                        {
                            let _ = self
                                .commands
                                .send(BridgeCommand::ForgetBattery(battery.device_id.clone()));
                        }
                        ui.label(
                            RichText::new(format!(
                                "{}: {}",
                                battery.device_name,
                                battery_text(battery)
                            ))
                            .size(11.5)
                            .color(color),
                        );
                    });
                }
                if !snapshot.batteries.is_empty() {
                    ui.label(
                        RichText::new("Battery reports are not proof of an open device session.")
                            .size(11.0)
                            .color(MUTED),
                    );
                }
            }
        });
    }

    fn resize_panel(&mut self, context: &egui::Context) {
        let has_values = self
            .snapshot
            .as_ref()
            .and_then(|s| s.active_profile.as_ref())
            .is_some_and(|p| p.settings.dpi.is_some() || p.settings.polling_rate_hz.is_some());
        let has_summary = device_summary_battery(self.snapshot.as_ref()).is_some();
        let height = panel_height(
            self.view,
            self.details_open,
            has_values,
            update_is_notice(&self.update_state),
            has_summary,
        );
        if height != self.requested_height {
            let origin = context
                .input(|i| i.viewport().outer_rect.map(|rect| rect.min))
                .or_else(test_position);
            context.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(WINDOW_WIDTH, height)));
            if let Some(origin) = origin {
                let scale = f64::from(context.pixels_per_point());
                let anchor = TrayRect {
                    position: (f64::from(origin.x) * scale, f64::from(origin.y) * scale).into(),
                    size: (1, 1).into(),
                };
                let position = tray_work_area(anchor, scale).map_or(origin, |work| {
                    clamp_panel(origin, Vec2::new(WINDOW_WIDTH, height), work)
                });
                context.send_viewport_cmd(ViewportCommand::OuterPosition(position));
            }
            self.requested_height = height;
        }
    }

    fn send_command(&mut self, command: BridgeCommand, unavailable: &str) -> bool {
        if self.commands.send(command).is_ok() {
            self.last_error = None;
            true
        } else {
            self.last_error = Some(unavailable.into());
            false
        }
    }
}

struct BackgroundServer {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

/// Where the runtime sends notices, and the tray app's context to
/// wake so it shows them even while its panel is hidden.
type NoticeSink = (Sender<Notice>, Arc<OnceLock<egui::Context>>);

type BackgroundRuntime = (
    BackgroundServer,
    Receiver<BridgeSnapshot>,
    tokio_mpsc::UnboundedSender<BridgeCommand>,
    Receiver<UpdateEvent>,
    Receiver<()>,
    bool,
);

impl BackgroundServer {
    fn start(notices: NoticeSink) -> Result<Option<BackgroundRuntime>> {
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

    fn stop(mut self) -> Result<()> {
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

pub fn run() -> Result<()> {
    let (notice_tx, notices) = mpsc::channel();
    let waker = Arc::new(OnceLock::new());
    if headless_requested() || !gui_available() {
        if !headless_requested() {
            tracing::warn!(
                "no usable display for the tray panel (set WAYLAND_DISPLAY or DISPLAY, and GTK must start); running headless: HTTP API, HID, and game profiles still work"
            );
        }
        let Some((server, snapshots, commands, updates, _panel_requests, _show_panel)) =
            BackgroundServer::start((notice_tx, Arc::clone(&waker)))?
        else {
            return Ok(());
        };
        return run_headless(server, snapshots, commands, updates, notices, waker);
    }
    // The GUI closure below moves the channels in, so a GL failure cannot
    // fall back to headless afterwards: start headless up front when no
    // display is reachable (checked above), and treat a run_native error
    // as fatal here. (A broken-GL fallback would need the channels split
    // before the closure; revisit if Wayland-compositor reports require it.)
    let Some((server, snapshots, commands, updates, panel_requests, show_panel)) =
        BackgroundServer::start((notice_tx, Arc::clone(&waker)))?
    else {
        return Ok(());
    };
    let keep_running = Arc::new(AtomicBool::new(true));
    let app_keep_running = Arc::clone(&keep_running);
    // The panel stays hidden after the first launch. Until then, and whenever
    // OPENMOUSE_BRIDGE_SHOW_WINDOW is set, open it: a tray-only start looks
    // like nothing happened, and an elevated process's tray icon does not
    // show in the normal notification area.
    let visible = show_panel || env::var_os("OPENMOUSE_BRIDGE_SHOW_WINDOW").is_some();
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
            .with_decorations(false)
            .with_resizable(false)
            .with_taskbar(show_panel)
            .with_always_on_top()
            .with_visible(visible),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    if let Some(position) = test_position() {
        options.viewport = options.viewport.with_position(position).with_active(false);
    }
    let gui_started = Instant::now();
    let tray_result = match panic::catch_unwind(AssertUnwindSafe(|| {
        eframe::run_native(
            "OpenMouse Bridge",
            options,
            Box::new(move |context| {
                Ok(Box::new(TrayApp::new(
                    &context.egui_ctx,
                    snapshots,
                    commands,
                    updates,
                    notices,
                    panel_requests,
                    &waker,
                    app_keep_running,
                    visible,
                    show_panel,
                )?))
            }),
        )
        .map_err(|error| error.to_string())
    })) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow!(error)),
        Err(payload) => {
            let error = panic_message(&payload);
            tracing::error!(%error, "OpenMouse Bridge tray panel panicked");
            Err(anyhow!("tray panel panicked: {error}"))
        }
    };
    let server_result = server.stop();
    if let Err(error) = &tray_result {
        tracing::error!(%error, "OpenMouse Bridge tray panel stopped");
    }
    if let Err(error) = &server_result {
        tracing::error!(%error, "OpenMouse Bridge runtime stopped");
    }
    if let Some(generation) = next_relaunch_generation(
        current_relaunch_generation(),
        gui_started.elapsed(),
        keep_running.load(Ordering::Relaxed),
    ) {
        if tray_result.is_err() {
            thread::sleep(RELAUNCH_DELAY);
        }
        tracing::info!(
            generation,
            "Relaunching OpenMouse Bridge after the tray panel stopped"
        );
        restart_bridge(generation)?;
        return Ok(());
    }
    tray_result.and(server_result)?;
    Ok(())
}

/// The time since `previous` when it is long enough that the PC must have
/// slept; a backwards clock change is not a resume.
fn resume_gap(previous: SystemTime, now: SystemTime) -> Option<Duration> {
    now.duration_since(previous)
        .ok()
        .filter(|gap| *gap >= RESUME_GAP)
}

fn current_relaunch_generation() -> u32 {
    env::var(RELAUNCH_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// After Glow/winit die (common on Windows hibernate), replace this process
/// unless the user quit or an in-app update is already relaunching. Quick
/// crash loops stop after `MAX_QUICK_RELAUNCHES`; a panel that stayed up
/// for `GUI_STABLE_AFTER` resets the counter.
fn next_relaunch_generation(current: u32, gui_lived: Duration, keep_running: bool) -> Option<u32> {
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

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
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

fn restart_bridge(generation: u32) -> Result<()> {
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
    (notices, waker): NoticeSink,
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
        let panel_waker = Arc::clone(&waker);
        service.on_notice(move |notice| {
            let sent = notices
                .lock()
                .map(|sender| sender.send(notice).is_ok())
                .unwrap_or(false);
            if sent && let Some(context) = waker.get() {
                context.request_repaint();
            }
        });
        service.on_show_panel(move || {
            let _ = panel_requests.send(());
            if let Some(context) = panel_waker.get() {
                context.request_repaint();
            }
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
            if automatic_updates {
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
fn configure_tray_only_application() {
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
fn configure_tray_only_application() {}

fn notify_natively(notice: &Notice) {
    if let Err(error) = platform::notify(&notice.title, &notice.detail) {
        tracing::warn!(%error, "Could not show the notification");
    }
}

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

fn tray_icon() -> Result<Icon> {
    let (rgba, width, height) = openmouse_icon_rgba()?;
    Icon::from_rgba(rgba, width, height).context("could not create the OpenMouse tray icon")
}

fn open_logs() -> Result<()> {
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

fn open_openmouse() -> Result<()> {
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
    fn escape_keeps_panel_open_until_popup_closes() {
        let context = egui::Context::default();
        context
            .run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key: egui::Key::Escape,
                        physical_key: Some(egui::Key::Escape),
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..Default::default()
                },
                |ui| {
                    let popup = egui::Id::new("escape-review-popup");
                    egui::Popup::open_id(ui.ctx(), popup);
                    assert!(egui::Popup::is_any_open(ui.ctx()));
                    assert!(!should_hide_on_escape(true, ui.ctx()));
                    assert!(
                        egui::Popup::new(popup, ui.ctx().clone(), egui::Pos2::ZERO, ui.layer_id(),)
                            .open_memory(None)
                            .show(|ui| ui.label("Popup"))
                            .is_some()
                    );
                    assert!(!egui::Popup::is_any_open(ui.ctx()));
                },
            )
            .drop_without_applying_deltas();
    }

    #[test]
    fn escape_requires_visible_panel_and_keypress() {
        for (visible, pressed, expected) in [
            (true, true, true),
            (false, true, false),
            (true, false, false),
            (false, false, false),
        ] {
            egui::Context::default()
                .run_ui(
                    egui::RawInput {
                        events: if pressed {
                            vec![egui::Event::Key {
                                key: egui::Key::Escape,
                                physical_key: Some(egui::Key::Escape),
                                pressed: true,
                                repeat: false,
                                modifiers: egui::Modifiers::NONE,
                            }]
                        } else {
                            vec![]
                        },
                        ..Default::default()
                    },
                    |ui| assert_eq!(should_hide_on_escape(visible, ui.ctx()), expected),
                )
                .drop_without_applying_deltas();
        }
    }

    fn simulated_header_drag(start: Pos2, end: Pos2) -> bool {
        let context = egui::Context::default();
        let rect = egui::Rect::from_min_max(Pos2::new(20.0, 20.0), Pos2::new(260.0, 60.0));
        let frames = [
            vec![],
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            vec![egui::Event::PointerMoved(end)],
        ];
        let mut started = false;
        for events in frames {
            let output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        Pos2::ZERO,
                        Vec2::new(372.0, 484.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| drag_header(ui, rect, "test-drag"),
            );
            started |= output.viewport_output.values().any(|viewport| {
                viewport
                    .commands
                    .iter()
                    .any(|command| matches!(command, ViewportCommand::StartDrag))
            });
            output.drop_without_applying_deltas();
        }
        started
    }

    #[test]
    fn tray_menu_can_recover_the_window_without_click_coordinates() {
        assert!(matches!(
            tray_menu_action(true, false, false),
            Some(TrayAction::ShowWindow)
        ));
        assert!(tray_menu_action(false, false, false).is_none());
    }

    #[test]
    fn tray_event_pump_is_bounded_and_drains_pending_events() {
        let remaining = std::cell::Cell::new(40);
        drain_tray_events(
            || remaining.get() > 0,
            || remaining.set(remaining.get() - 1),
        );
        assert_eq!(remaining.get(), 8);
        drain_tray_events(
            || remaining.get() > 0,
            || remaining.set(remaining.get() - 1),
        );
        assert_eq!(remaining.get(), 0);
    }

    #[test]
    fn header_drag_requests_a_native_window_move() {
        assert!(simulated_header_drag(
            Pos2::new(80.0, 35.0),
            Pos2::new(140.0, 75.0)
        ));
    }

    #[test]
    fn dragging_outside_header_leaves_window_in_place() {
        assert!(!simulated_header_drag(
            Pos2::new(300.0, 35.0),
            Pos2::new(340.0, 75.0)
        ));
    }

    #[test]
    fn content_growth_stays_bounded_and_settings_has_its_own_budget() {
        assert_eq!(panel_height(View::Home, false, false, false, false), 280.0);
        assert_eq!(panel_height(View::Home, false, true, false, false), 298.0);
        assert_eq!(panel_height(View::Home, false, true, true, false), 338.0);
        assert_eq!(panel_height(View::Home, true, false, false, false), 340.0);
        assert_eq!(panel_height(View::Settings, true, true, true, false), 420.0);
    }

    #[test]
    fn device_summary_rows_grow_the_home_view() {
        assert_eq!(panel_height(View::Home, false, false, false, true), 302.0);
        assert_eq!(panel_height(View::Home, false, true, false, true), 320.0);
        // The details view scrolls, so its budget stays fixed.
        assert_eq!(panel_height(View::Home, true, false, false, true), 340.0);
    }

    #[test]
    fn idle_window_is_a_compact_companion_not_a_settings_sheet() {
        assert_eq!(WINDOW_WIDTH, 360.0);
        assert!(panel_height(View::Home, false, false, false, false) <= 340.0);
    }

    #[test]
    fn missing_snapshot_is_starting_not_ready() {
        assert_eq!(service_label(None), "Starting Bridge…");
    }

    #[test]
    fn native_failure_and_pending_restore_are_visible_without_an_applied_claim() {
        let mut snapshot = empty_snapshot();
        snapshot.native_automation_error =
            Some("manual intent superseded automatic settings".into());
        assert_eq!(
            profile_label(Some(&snapshot)),
            "Native settings not applied: manual intent superseded automatic settings"
        );
        snapshot.pending_native_restores = 1;
        assert_eq!(
            profile_label(Some(&snapshot)),
            "Native restore pending (1): manual intent superseded automatic settings"
        );
        snapshot.native_automation_error = None;
        assert_eq!(profile_label(Some(&snapshot)), "Native restore pending (1)");
    }

    #[test]
    fn missing_profile_is_not_an_invented_default() {
        assert_eq!(profile_label(None), "No game profile active");
    }

    fn empty_snapshot() -> BridgeSnapshot {
        BridgeSnapshot {
            version: "test",
            platform: "test",
            linux_distribution: None,
            uptime_seconds: 0,
            active_games: vec![],
            games: vec![],
            tracked_game_count: 0,
            battery_threshold_percent: 20,
            tray_battery_style: "glyph".to_owned(),
            hybrid_mode: true,
            device_mode: "onboard".to_owned(),
            autostart_enabled: false,
            automatic_updates: false,
            notification_sound: true,
            notification_volume: 60,
            foreground_application: None,
            active_profile: None,
            active_profile_is_default: false,
            native_automation_error: None,
            pending_native_restores: 0,
            visible_application_count: 0,
            profile_count: 0,
            client_connected: false,
            active_devices: vec![],
            batteries: vec![],
        }
    }

    #[test]
    fn remembered_battery_does_not_become_a_live_mouse() {
        let mut snapshot = empty_snapshot();
        snapshot
            .batteries
            .push(openmouse_bridge::service::DeviceBattery {
                device_id: "old".into(),
                device_name: "Remembered mouse".into(),
                percent: 60,
                charging: false,
                stale: true,
                minutes_remaining: None,
                dpi: None,
                polling_rate_hz: None,
                switch_mode: None,
            });
        assert_eq!(device_label(Some(&snapshot)), "No mouse session");
    }

    #[test]
    fn stale_battery_removal_remains_available_in_compact_details() {
        let context = egui::Context::default();
        let mut shown = Vec::new();
        for stale in [false, true] {
            context
                .run_ui(egui::RawInput::default(), |ui| {
                    shown.push(forget_battery_button(ui, stale).is_some());
                })
                .drop_without_applying_deltas();
        }
        assert_eq!(shown, vec![false, true]);
    }

    #[test]
    fn stale_battery_explicitly_says_last_known_without_current_charging_claim() {
        let battery = openmouse_bridge::service::DeviceBattery {
            device_id: "old".into(),
            device_name: "Remembered mouse".into(),
            percent: 60,
            charging: true,
            stale: true,
            minutes_remaining: None,
            dpi: None,
            polling_rate_hz: None,
            switch_mode: None,
        };
        assert_eq!(battery_text(&battery), "60% · Last known (was charging)");
    }

    #[test]
    fn panel_anchor_respects_negative_monitor_work_area() {
        let work = egui::Rect::from_min_max(Pos2::new(-1920.0, -1080.0), Pos2::new(0.0, -40.0));
        assert_eq!(
            clamp_panel(Pos2::new(-2000.0, -1200.0), Vec2::new(372.0, 484.0), work),
            Pos2::new(-1912.0, -1072.0)
        );
        assert_eq!(
            clamp_panel(Pos2::new(100.0, 100.0), Vec2::new(372.0, 484.0), work),
            Pos2::new(-380.0, -532.0)
        );
    }

    #[test]
    fn explicit_test_position_accepts_secondary_monitor_coordinates() {
        assert_eq!(parse_position("-1500,40"), Some(Pos2::new(-1500.0, 40.0)));
        for value in ["", "1", "1,2,3", "NaN,2", "inf,0"] {
            assert_eq!(parse_position(value), None);
        }
    }

    #[test]
    fn ready_service_does_not_claim_panel_connection_or_a_default() {
        let mut snapshot = empty_snapshot();
        assert_eq!(service_label(Some(&snapshot)), "Bridge service ready");
        assert_eq!(profile_label(Some(&snapshot)), "No game profile active");
        snapshot.client_connected = true;
        assert_eq!(service_label(Some(&snapshot)), "Bridge service ready");
        assert_eq!(device_label(Some(&snapshot)), "No mouse session");
    }

    #[test]
    fn profile_target_is_remembered_and_selected_not_live_or_applied() {
        let mut snapshot = empty_snapshot();
        snapshot.active_profile = Some(config::ApplicationProfile {
            application: config::ProfileApplication {
                name: "Game".into(),
                executable: "game.exe".into(),
                path: String::new(),
                local_id: None,
                activation: None,
                bundle_path: None,
                bundle_identifier: None,
            },
            device: config::ProfileDevice {
                id: "saved".into(),
                name: "Saved mouse".into(),
            },
            settings: config::ProfileSettings {
                dpi: Some(800),
                polling_rate_hz: Some(1000),
                snapshot: None,
            },
            enabled: true,
        });
        assert_eq!(profile_label(Some(&snapshot)), "Game selected");
        assert_eq!(device_label(Some(&snapshot)), "No mouse session");
        snapshot.active_profile_is_default = true;
        assert_eq!(profile_label(Some(&snapshot)), "Default profile selected");
        snapshot
            .active_devices
            .push(openmouse_bridge::service::ActiveDevice {
                key: "live".into(),
                name: "Wired mouse".into(),
                vendor_id: 1,
                product_id: 2,
            });
        assert_eq!(device_label(Some(&snapshot)), "Wired mouse");
        assert!(snapshot.batteries.is_empty());
    }

    #[test]
    fn fresh_battery_keeps_reported_charge_and_charging_state() {
        let mut battery = openmouse_bridge::service::DeviceBattery {
            device_id: "live".into(),
            device_name: "Mouse".into(),
            percent: 42,
            charging: false,
            stale: false,
            minutes_remaining: None,
            dpi: None,
            polling_rate_hz: None,
            switch_mode: None,
        };
        assert_eq!(battery_text(&battery), "42%");
        battery.charging = true;
        assert_eq!(battery_text(&battery), "42% · Charging");
        battery.stale = true;
        battery.charging = false;
        assert_eq!(battery_text(&battery), "42% · Last known");
    }

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
