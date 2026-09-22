use std::{
    env,
    io::Cursor,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc, OnceLock,
        atomic::AtomicBool,
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread,
    time::Duration,
};

use crate::overlay::Overlay;
use anyhow::{Context, Result, anyhow};
use eframe::egui::{
    self, Align, Button, Color32, Frame, Layout, Pos2, RichText, Sense, Stroke, Vec2,
    ViewportCommand,
};
use openmouse_bridge::{
    BRIDGE_PORT, api, config, platform,
    service::{BridgeService, BridgeSnapshot, ProfileSwitch},
    updater::{self, UpdateInfo},
};
#[cfg(target_os = "windows")]
use std::ptr::null_mut;
use tokio::{
    net::TcpListener,
    sync::{mpsc as tokio_mpsc, oneshot},
};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, Rect as TrayRect, TrayIcon, TrayIconBuilder, TrayIconEvent,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

const OPENMOUSE_URL: &str = "https://control.openmouse.app";
const WINDOW_WIDTH: f32 = 320.0;
const WINDOW_HEIGHT: f32 = 306.0;
const BACKGROUND: Color32 = Color32::from_rgb(16, 17, 19);
const SURFACE: Color32 = Color32::from_rgb(26, 28, 31);
const SURFACE_HOVER: Color32 = Color32::from_rgb(36, 39, 43);
const DIVIDER: Color32 = Color32::from_rgb(34, 36, 40);
const TEXT: Color32 = Color32::from_rgb(236, 238, 240);
const MUTED: Color32 = Color32::from_rgb(132, 138, 145);
const ACCENT: Color32 = Color32::from_rgb(93, 222, 137);
const DANGER: Color32 = Color32::from_rgb(248, 113, 113);
const ROW_HEIGHT: f32 = 34.0;
const MAX_BATTERY_ROWS: usize = 3;
const BATTERY_THRESHOLDS: [u8; 7] = [10, 15, 20, 25, 30, 40, 50];

fn toggle_control(ui: &mut egui::Ui, enabled: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(28.0, 16.0), Sense::click());
    let track = if enabled { ACCENT } else { SURFACE_HOVER };
    let knob = if enabled { BACKGROUND } else { MUTED };
    ui.painter().rect_filled(rect, 8.0, track);
    let knob_x = if enabled {
        rect.right() - 8.0
    } else {
        rect.left() + 8.0
    };
    ui.painter()
        .circle_filled(Pos2::new(knob_x, rect.center().y), 5.0, knob);
    response.clicked()
}

#[derive(Clone, Copy)]
enum Glyph {
    Gear,
    Back,
}

fn icon_button(ui: &mut egui::Ui, glyph: Glyph) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, 6.0, SURFACE_HOVER);
    }
    let color = if response.hovered() { TEXT } else { MUTED };
    let center = rect.center();
    match glyph {
        Glyph::Gear => {
            painter.circle_stroke(center, 3.5, Stroke::new(1.5, color));
            for tooth in 0..8 {
                let angle = tooth as f32 * std::f32::consts::FRAC_PI_4;
                let direction = Vec2::angled(angle);
                painter.line_segment(
                    [center + direction * 5.5, center + direction * 7.5],
                    Stroke::new(2.0, color),
                );
            }
        }
        Glyph::Back => {
            painter.line(
                vec![
                    center + Vec2::new(2.5, -5.0),
                    center + Vec2::new(-2.5, 0.0),
                    center + Vec2::new(2.5, 5.0),
                ],
                Stroke::new(1.5, color),
            );
        }
    }
    response
}

fn divider(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, DIVIDER));
}

fn section_label(ui: &mut egui::Ui, label: &str) {
    ui.add_space(14.0);
    ui.label(
        RichText::new(label.to_uppercase())
            .size(10.0)
            .strong()
            .color(MUTED),
    );
    ui.add_space(2.0);
}

fn row(ui: &mut egui::Ui, label: &str, trailing: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), ROW_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.label(RichText::new(label).size(12.0).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), trailing);
        },
    );
}

fn value_row(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    row(ui, label, |ui| {
        ui.add(egui::Label::new(RichText::new(value).size(12.0).color(color)).truncate());
    });
}

fn link_button(ui: &mut egui::Ui, label: &str, color: Color32) -> bool {
    ui.add(Button::new(RichText::new(label).size(12.0).strong().color(color)).frame(false))
        .clicked()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    Settings,
}

enum BridgeCommand {
    SetBatteryThreshold(u8),
    SetAutomaticUpdates(bool),
    CheckForUpdates,
    InstallUpdate(UpdateInfo),
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

enum UpdateUiAction {
    Check,
    Install(UpdateInfo),
}

#[derive(Clone, Copy)]
enum TrayAction {
    ToggleWindow { rect: TrayRect },
}

struct TrayState {
    _icon: TrayIcon,
    events: Receiver<TrayAction>,
}

impl TrayState {
    fn new(context: &egui::Context) -> Result<Self> {
        let icon = TrayIconBuilder::new()
            .with_tooltip("OpenMouse Bridge")
            .with_icon(tray_icon().context("could not create the tray icon image")?)
            .build()
            .context("could not create the system tray icon")?;

        let (event_tx, events) = mpsc::channel();
        let repaint = context.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            tracing::debug!(?event, "received tray event");
            if let TrayIconEvent::Click {
                rect,
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = event_tx.send(TrayAction::ToggleWindow { rect });
                repaint.request_repaint();
            }
        }));

        Ok(Self {
            _icon: icon,
            events,
        })
    }

    fn toggle_requested(&self) -> Option<TrayRect> {
        let mut request = None;
        while let Ok(TrayAction::ToggleWindow { rect }) = self.events.try_recv() {
            request = if request.is_some() { None } else { Some(rect) };
        }
        request
    }
}

struct TrayApp {
    tray: TrayState,
    logo: egui::TextureHandle,
    snapshots: Receiver<BridgeSnapshot>,
    commands: tokio_mpsc::UnboundedSender<BridgeCommand>,
    updates: Receiver<UpdateEvent>,
    switches: Receiver<ProfileSwitch>,
    overlay: Option<Overlay>,
    snapshot: Option<BridgeSnapshot>,
    view: View,
    visible: bool,
    visibility_initialized: bool,
    panel_had_focus: bool,
    exit_requested: bool,
    autostart: bool,
    battery_threshold: u8,
    automatic_updates: bool,
    update_state: UpdateState,
    last_error: Option<String>,
}

impl TrayApp {
    fn new(
        context: &egui::Context,
        snapshots: Receiver<BridgeSnapshot>,
        commands: tokio_mpsc::UnboundedSender<BridgeCommand>,
        updates: Receiver<UpdateEvent>,
        switches: Receiver<ProfileSwitch>,
        waker: &OnceLock<egui::Context>,
        visible: bool,
    ) -> Result<Self> {
        configure_tray_only_application();
        let _ = waker.set(context.clone());
        // Without the overlay, switches fall back to system notifications.
        let overlay = Overlay::new()
            .inspect_err(|error| tracing::warn!(%error, "Could not create the profile overlay"))
            .ok();
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = BACKGROUND;
        visuals.override_text_color = Some(TEXT);
        visuals.window_stroke = Stroke::NONE;
        visuals.window_shadow = egui::Shadow::NONE;
        visuals.popup_shadow = egui::Shadow::NONE;
        visuals.window_corner_radius = 8.into();
        visuals.menu_corner_radius = 8.into();
        visuals.selection.bg_fill = SURFACE_HOVER;
        visuals.selection.stroke = Stroke::new(1.0, TEXT);
        for (widget, fill) in [
            (&mut visuals.widgets.noninteractive, BACKGROUND),
            (&mut visuals.widgets.inactive, SURFACE),
            (&mut visuals.widgets.hovered, SURFACE_HOVER),
            (&mut visuals.widgets.active, SURFACE_HOVER),
            (&mut visuals.widgets.open, SURFACE_HOVER),
        ] {
            widget.bg_fill = fill;
            widget.weak_bg_fill = fill;
            widget.bg_stroke = Stroke::NONE;
            widget.corner_radius = 6.into();
            widget.expansion = 0.0;
        }
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, DIVIDER);
        context.set_visuals(visuals);
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
            switches,
            overlay,
            snapshot: None,
            view: View::Home,
            visible,
            visibility_initialized: false,
            panel_had_focus: false,
            exit_requested: false,
            autostart: platform::autostart_enabled(),
            battery_threshold: 20,
            automatic_updates: false,
            update_state: UpdateState::Idle,
            last_error: None,
        })
    }

    fn set_visible(&mut self, context: &egui::Context, visible: bool) {
        self.visible = visible;
        self.panel_had_focus = false;
        context.send_viewport_cmd(ViewportCommand::Visible(visible));
        if visible {
            self.view = View::Home;
            context.send_viewport_cmd(ViewportCommand::Focus);
        }
    }

    fn show_near_tray(&mut self, context: &egui::Context, rect: TrayRect) {
        let scale = f64::from(context.pixels_per_point());
        let x = (rect.position.x + f64::from(rect.size.width)) / scale - f64::from(WINDOW_WIDTH);
        #[cfg(target_os = "macos")]
        let y = (rect.position.y + f64::from(rect.size.height)) / scale + 8.0;
        #[cfg(target_os = "windows")]
        let y = rect.position.y / scale - f64::from(WINDOW_HEIGHT) - 8.0;
        context.send_viewport_cmd(ViewportCommand::OuterPosition(Pos2::new(
            x as f32,
            y.max(8.0) as f32,
        )));
        self.set_visible(context, true);
    }

    fn refresh_snapshot(&mut self) {
        while let Ok(snapshot) = self.snapshots.try_recv() {
            self.autostart = snapshot.autostart_enabled;
            self.battery_threshold = snapshot.battery_threshold_percent;
            self.automatic_updates = snapshot.automatic_updates;
            self.snapshot = Some(snapshot);
        }
    }

    /// Shows pending profile switches and advances the banner's fade.
    /// Returns how soon the banner needs the next frame.
    fn refresh_overlay(&mut self) -> Option<Duration> {
        while let Ok(switch) = self.switches.try_recv() {
            let shown = self
                .overlay
                .as_mut()
                .map(|overlay| overlay.show(&switch.title, &switch.detail));
            match shown {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    tracing::warn!(%error, "Could not show the profile overlay");
                    notify_switch(&switch);
                }
                None => notify_switch(&switch),
            }
        }
        self.overlay.as_mut()?.tick()
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

impl eframe::App for TrayApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.refresh_snapshot();
        if self.refresh_updates() {
            self.exit_requested = true;
            context.send_viewport_cmd(ViewportCommand::Close);
        }
        if !self.visibility_initialized {
            self.visibility_initialized = true;
            context.send_viewport_cmd(ViewportCommand::Visible(self.visible));
            if self.visible {
                context.send_viewport_cmd(ViewportCommand::Focus);
            }
        }
        if let Some(rect) = self.tray.toggle_requested() {
            tracing::debug!(?rect, visible = self.visible, "handling tray toggle");
            if self.visible {
                self.set_visible(context, false);
            } else {
                self.show_near_tray(context, rect);
            }
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
        Frame::NONE
            .fill(BACKGROUND)
            .inner_margin(egui::Margin::symmetric(18, 16))
            .show(ui, |ui| match self.view {
                View::Home => self.home_view(ui),
                View::Settings => self.settings_view(ui),
            });
    }
}

impl TrayApp {
    fn home_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(24.0);
            let (logo_rect, _) = ui.allocate_exact_size(Vec2::new(14.0, 20.0), Sense::hover());
            ui.put(
                logo_rect,
                egui::Image::new((self.logo.id(), Vec2::new(13.0, 19.0))),
            );
            ui.add_space(2.0);
            ui.label(RichText::new("OpenMouse Bridge").size(13.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, Glyph::Gear)
                    .on_hover_text("Settings")
                    .clicked()
                {
                    self.view = View::Settings;
                }
            });
        });

        let snapshot = self.snapshot.as_ref();
        let connected = snapshot.is_some_and(|snapshot| snapshot.client_connected);
        ui.add_space(22.0);
        ui.horizontal(|ui| {
            let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
            ui.painter().circle_filled(
                dot_rect.center(),
                4.0,
                if connected { ACCENT } else { MUTED },
            );
            ui.label(
                RichText::new(if connected { "Connected" } else { "Ready" })
                    .size(20.0)
                    .strong(),
            );
        });
        let device_name = snapshot.and_then(|snapshot| {
            snapshot
                .batteries
                .first()
                .map(|battery| battery.device_name.clone())
                .or_else(|| {
                    snapshot
                        .active_profile
                        .as_ref()
                        .map(|profile| profile.device.name.clone())
                })
        });
        let subtitle = device_name.unwrap_or_else(|| {
            if connected {
                "The control panel is using Bridge".into()
            } else {
                "Open the control panel to connect".into()
            }
        });
        ui.add(egui::Label::new(RichText::new(subtitle).size(12.0).color(MUTED)).truncate());

        ui.add_space(18.0);
        // Name the profile that is actually applied: a game's while it runs,
        // an application's while it is in front, otherwise Default.
        let profile = snapshot
            .filter(|snapshot| !snapshot.active_profile_is_default)
            .and_then(|snapshot| snapshot.active_profile.as_ref())
            .map(|profile| profile.application.name.clone())
            .unwrap_or_else(|| "Default".into());
        divider(ui);
        value_row(ui, "Profile", &profile, MUTED);
        divider(ui);
        match snapshot.map(|snapshot| snapshot.batteries.as_slice()) {
            Some(batteries) if !batteries.is_empty() => {
                let single = batteries.len() == 1;
                for battery in batteries.iter().take(MAX_BATTERY_ROWS) {
                    let value = if battery.charging {
                        format!("{}% · Charging", battery.percent)
                    } else {
                        format!("{}%", battery.percent)
                    };
                    let color = if battery.stale {
                        MUTED
                    } else if battery.percent <= self.battery_threshold && !battery.charging {
                        DANGER
                    } else {
                        TEXT
                    };
                    let label = if single {
                        "Battery"
                    } else {
                        battery.device_name.as_str()
                    };
                    value_row(ui, label, &value, color);
                    divider(ui);
                }
            }
            _ => {
                value_row(ui, "Battery", "Unknown", MUTED);
                divider(ui);
            }
        }

        let mut install = None;
        let notice = match &self.update_state {
            UpdateState::Available(update) => Some(format!("Update {} available", update.version)),
            UpdateState::Downloading(version) => Some(format!("Downloading {version}…")),
            UpdateState::Restarting(version) => Some(format!("Restarting into {version}…")),
            _ => None,
        };
        if let Some(notice) = notice {
            ui.add_space(12.0);
            Frame::NONE
                .fill(SURFACE)
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(notice).size(12.0));
                        if let UpdateState::Available(update) = &self.update_state {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if link_button(ui, "Install", ACCENT) {
                                    install = Some(update.clone());
                                }
                            });
                        }
                    });
                });
        }
        if let Some(update) = install {
            self.send_command(
                BridgeCommand::InstallUpdate(update),
                "Bridge update service is unavailable",
            );
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            if ui
                .add_sized(
                    [ui.available_width(), 36.0],
                    Button::new(
                        RichText::new("Open control panel")
                            .size(12.0)
                            .strong()
                            .color(BACKGROUND),
                    )
                    .fill(ACCENT)
                    .stroke(Stroke::NONE)
                    .corner_radius(8.0),
                )
                .clicked()
                && let Err(error) = open_openmouse()
            {
                self.last_error = Some(error.to_string());
            }
            if let Some(error) = &self.last_error {
                ui.add_space(6.0);
                ui.label(RichText::new(error).size(11.0).color(DANGER));
            }
        });
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(24.0);
            if icon_button(ui, Glyph::Back).on_hover_text("Back").clicked() {
                self.view = View::Home;
            }
            ui.label(RichText::new("Settings").size(13.0).strong());
        });

        section_label(ui, "General");
        let mut toggle_autostart = false;
        row(ui, "Launch at login", |ui| {
            toggle_autostart = toggle_control(ui, self.autostart);
        });
        if toggle_autostart {
            let enabled = !self.autostart;
            match platform::set_autostart(enabled) {
                Ok(()) => {
                    self.autostart = enabled;
                    self.last_error = None;
                }
                Err(error) => self.last_error = Some(error.to_string()),
            }
        }
        divider(ui);
        let mut selected_threshold = self.battery_threshold;
        row(ui, "Low battery alert", |ui| {
            egui::ComboBox::from_id_salt("battery-threshold")
                .width(64.0)
                .selected_text(RichText::new(format!("{}%", self.battery_threshold)).size(12.0))
                .show_ui(ui, |ui| {
                    for percent in BATTERY_THRESHOLDS {
                        ui.selectable_value(
                            &mut selected_threshold,
                            percent,
                            format!("{percent}%"),
                        );
                    }
                });
        });
        if selected_threshold != self.battery_threshold
            && self.send_command(
                BridgeCommand::SetBatteryThreshold(selected_threshold),
                "Bridge settings service is unavailable",
            )
        {
            self.battery_threshold = selected_threshold;
        }

        section_label(ui, "Updates");
        let mut toggle_automatic_updates = false;
        row(ui, "Automatic updates", |ui| {
            toggle_automatic_updates = toggle_control(ui, self.automatic_updates);
        });
        if toggle_automatic_updates {
            let enabled = !self.automatic_updates;
            if self.send_command(
                BridgeCommand::SetAutomaticUpdates(enabled),
                "Bridge settings service is unavailable",
            ) {
                self.automatic_updates = enabled;
            }
        }
        divider(ui);
        let (status, color, action_label) = match &self.update_state {
            UpdateState::Idle => ("Not checked yet".to_owned(), MUTED, Some("Check")),
            UpdateState::Checking => ("Checking…".to_owned(), MUTED, None),
            UpdateState::UpToDate => ("Up to date".to_owned(), MUTED, Some("Check")),
            UpdateState::Available(update) => (
                format!("{} available", update.version),
                ACCENT,
                Some("Install"),
            ),
            UpdateState::Downloading(version) => (format!("Downloading {version}…"), MUTED, None),
            UpdateState::Restarting(version) => {
                (format!("Restarting into {version}…"), ACCENT, None)
            }
            UpdateState::Failed(_) => ("Check failed".to_owned(), DANGER, Some("Retry")),
        };
        let mut update_action = None;
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), ROW_HEIGHT),
            Layout::left_to_right(Align::Center),
            |ui| {
                let response = ui.label(RichText::new(status).size(12.0).color(color));
                if let UpdateState::Failed(error) = &self.update_state {
                    response.on_hover_text(error);
                }
                if let Some(action_label) = action_label {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if link_button(ui, action_label, ACCENT) {
                            update_action = Some(match &self.update_state {
                                UpdateState::Available(update) => {
                                    UpdateUiAction::Install(update.clone())
                                }
                                _ => UpdateUiAction::Check,
                            });
                        }
                    });
                }
            },
        );
        if let Some(action) = update_action {
            let command = match action {
                UpdateUiAction::Check => BridgeCommand::CheckForUpdates,
                UpdateUiAction::Install(update) => BridgeCommand::InstallUpdate(update),
            };
            self.send_command(command, "Bridge update service is unavailable");
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "v{}  ·  {}  ·  127.0.0.1:{BRIDGE_PORT}",
                        openmouse_bridge::BRIDGE_VERSION,
                        platform::platform_name()
                    ))
                    .size(10.0)
                    .color(MUTED),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if link_button(ui, "Quit", DANGER) {
                        self.exit_requested = true;
                        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                    }
                });
            });
            if let Some(error) = &self.last_error {
                ui.add_space(6.0);
                ui.label(RichText::new(error).size(11.0).color(DANGER));
            }
        });
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

/// Where the runtime sends profile switches, and the tray app's context to
/// wake so it shows them even while its panel is hidden.
type SwitchSink = (Sender<ProfileSwitch>, Arc<OnceLock<egui::Context>>);

type BackgroundRuntime = (
    BackgroundServer,
    Receiver<BridgeSnapshot>,
    tokio_mpsc::UnboundedSender<BridgeCommand>,
    Receiver<UpdateEvent>,
);

impl BackgroundServer {
    fn start(switches: SwitchSink) -> Result<BackgroundRuntime> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (snapshot_tx, snapshots) = mpsc::sync_channel(1);
        let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
        let (update_tx, updates) = mpsc::sync_channel(8);
        let thread = thread::Builder::new()
            .name("openmouse-bridge-runtime".into())
            .spawn(move || {
                let outcome = run_server(
                    shutdown_rx,
                    ready_tx.clone(),
                    snapshot_tx,
                    command_rx,
                    update_tx,
                    switches,
                );
                if let Err(error) = &outcome {
                    let _ = ready_tx.send(Err(format!("{error:#}")));
                }
                outcome
            })
            .context("could not start the Bridge runtime")?;

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok((
                Self {
                    shutdown: Some(shutdown_tx),
                    thread: Some(thread),
                },
                snapshots,
                command_tx,
                updates,
            )),
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
    let (switch_tx, switches) = mpsc::channel();
    let waker = Arc::new(OnceLock::new());
    let (server, snapshots, commands, updates) =
        BackgroundServer::start((switch_tx, Arc::clone(&waker)))?;
    let visible = env::var_os("OPENMOUSE_BRIDGE_SHOW_WINDOW").is_some();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
            .with_decorations(false)
            .with_resizable(false)
            .with_taskbar(false)
            .with_always_on_top()
            .with_visible(visible),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let tray_result = eframe::run_native(
        "OpenMouse Bridge",
        options,
        Box::new(move |context| {
            Ok(Box::new(TrayApp::new(
                &context.egui_ctx,
                snapshots,
                commands,
                updates,
                switches,
                &waker,
                visible,
            )?))
        }),
    )
    .map_err(|error| anyhow!(error.to_string()));
    let server_result = server.stop();
    tray_result.and(server_result)
}

fn run_server(
    shutdown: oneshot::Receiver<()>,
    ready: SyncSender<Result<(), String>>,
    snapshots: SyncSender<BridgeSnapshot>,
    mut commands: tokio_mpsc::UnboundedReceiver<BridgeCommand>,
    update_events: SyncSender<UpdateEvent>,
    (switches, waker): SwitchSink,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not create the Bridge async runtime")?;
    runtime.block_on(async move {
        let (bridge_config, path) = config::load_with_catalog().await?;
        let automatic_updates = bridge_config.automatic_updates;
        let origins = bridge_config.allowed_origins.clone();
        let service = BridgeService::new(bridge_config, path.clone());
        let switches = std::sync::Mutex::new(switches);
        service.on_profile_switch(move |switch| {
            let sent = switches
                .lock()
                .map(|sender| sender.send(switch).is_ok())
                .unwrap_or(false);
            if sent && let Some(context) = waker.get() {
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
                    BridgeCommand::SetAutomaticUpdates(enabled) => {
                        match command_service.set_automatic_updates(enabled).await {
                            Ok(()) if enabled => check_for_updates(true, &update_events).await,
                            Ok(()) => {}
                            Err(error) => {
                                tracing::error!(%error, "could not save Bridge settings");
                            }
                        }
                    }
                    BridgeCommand::CheckForUpdates => {
                        check_for_updates(false, &update_events).await;
                    }
                    BridgeCommand::InstallUpdate(update) => {
                        install_update(update, &update_events).await;
                    }
                }
            }
        });

        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), BRIDGE_PORT);
        let listener = TcpListener::bind(address).await.with_context(|| {
            format!("could not bind http://{address}; is Bridge already running?")
        })?;
        tracing::info!(%address, config = %path.display(), "OpenMouse Bridge is ready");
        let _ = ready.send(Ok(()));
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

#[cfg(target_os = "windows")]
fn configure_tray_only_application() {}

fn notify_switch(switch: &ProfileSwitch) {
    if let Err(error) = platform::notify(&switch.title, &switch.detail) {
        tracing::warn!(%error, "Could not show the profile notification");
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

#[cfg(target_os = "windows")]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
