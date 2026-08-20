use std::{
    io::Cursor,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, RichText, Sense, Stroke, Vec2, ViewportCommand,
};
use openmouse_bridge::{
    BRIDGE_PORT, BRIDGE_VERSION, api, config, platform,
    service::{BridgeService, BridgeSnapshot, DeviceBattery},
};
#[cfg(target_os = "windows")]
use std::ptr::null_mut;
use tokio::{
    net::TcpListener,
    sync::{mpsc as tokio_mpsc, oneshot},
};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

const OPENMOUSE_URL: &str = "https://openmouse.app";
const BACKGROUND: Color32 = Color32::from_rgb(15, 16, 18);
const SURFACE: Color32 = Color32::from_rgb(24, 26, 29);
const SURFACE_RAISED: Color32 = Color32::from_rgb(31, 34, 38);
const BORDER: Color32 = Color32::from_rgb(55, 59, 65);
const TEXT: Color32 = Color32::from_rgb(239, 241, 243);
const MUTED: Color32 = Color32::from_rgb(151, 157, 166);
const ACCENT: Color32 = Color32::from_rgb(105, 210, 141);
const AMBER: Color32 = Color32::from_rgb(232, 184, 93);
const RED: Color32 = Color32::from_rgb(239, 112, 112);
const TRAY_SHOW: &str = "openmouse.show";
const TRAY_OPEN: &str = "openmouse.open";
const TRAY_QUIT: &str = "openmouse.quit";

enum DesktopEvent {
    Ready,
    Snapshot(Box<BridgeSnapshot>),
    ServerError(String),
    UpdateCheck(UpdateCheckOutcome),
}

/// Commands the (synchronous) UI sends to the async runtime that owns the
/// Bridge service.
enum ServerCommand {
    SetBatteryThreshold(u8),
    CheckForUpdate,
}

const RELEASES_URL: &str =
    "https://api.github.com/repos/OpenMouse-Project/OpenMouse-Bridge/releases/latest";
const RELEASES_PAGE_URL: &str =
    "https://github.com/OpenMouse-Project/OpenMouse-Bridge/releases/latest";

#[derive(Clone)]
enum UpdateCheckOutcome {
    UpToDate,
    Available { version: String },
    Failed(String),
}

#[derive(Clone, Default)]
enum UpdateCheckState {
    #[default]
    Idle,
    Checking,
    Done(UpdateCheckOutcome),
}

#[derive(serde::Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

/// Parses a `MAJOR.MINOR.PATCH` version (an optional leading `v` and any
/// `-`/`+` suffix are ignored), mirroring `openmouse/src/updates.ts`'s
/// `compareVersions` so Bridge's own update check agrees with the web app's.
fn parse_version(version: &str) -> Option<[u32; 3]> {
    let trimmed = version.trim().trim_start_matches('v');
    let core = trimmed.split(['-', '+']).next().unwrap_or(trimmed);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some([major, minor, patch])
}

async fn check_for_update() -> UpdateCheckOutcome {
    let request = reqwest::Client::new()
        .get(RELEASES_URL)
        .header(reqwest::header::USER_AGENT, "OpenMouse-Bridge")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await;
    let response = match request {
        Ok(response) => response,
        Err(error) => return UpdateCheckOutcome::Failed(error.to_string()),
    };
    if !response.status().is_success() {
        return UpdateCheckOutcome::Failed(format!("GitHub returned HTTP {}", response.status()));
    }
    let release = match response.json::<GitHubRelease>().await {
        Ok(release) => release,
        Err(error) => return UpdateCheckOutcome::Failed(error.to_string()),
    };
    let (Some(current), Some(latest)) = (
        parse_version(BRIDGE_VERSION),
        parse_version(&release.tag_name),
    ) else {
        return UpdateCheckOutcome::Failed(format!(
            "Could not compare versions ({BRIDGE_VERSION} vs {})",
            release.tag_name
        ));
    };
    if latest > current {
        UpdateCheckOutcome::Available {
            version: release.tag_name.trim_start_matches('v').to_owned(),
        }
    } else {
        UpdateCheckOutcome::UpToDate
    }
}

struct TrayState {
    _icon: TrayIcon,
}

impl TrayState {
    fn new(
        context: &egui::Context,
        quitting: Arc<AtomicBool>,
        window_active: Arc<AtomicBool>,
    ) -> Result<Self> {
        let show = MenuItem::with_id(TRAY_SHOW, "Show Bridge", true, None);
        let open = MenuItem::with_id(TRAY_OPEN, "Open OpenMouse", true, None);
        let separator = PredefinedMenuItem::separator();
        let quit = MenuItem::with_id(TRAY_QUIT, "Quit OpenMouse Bridge", true, None);
        let menu = Menu::with_items(&[&show, &open, &separator, &quit])
            .context("could not create the tray menu")?;
        let icon = tray_icon().context("could not create the tray icon image")?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("OpenMouse Bridge")
            .with_icon(icon)
            .build()
            .context("could not create the system tray icon")?;

        let tray_context = context.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == TRAY_SHOW {
                window_active.store(true, Ordering::Release);
                tray_context.send_viewport_cmd(ViewportCommand::Visible(true));
                tray_context.send_viewport_cmd(ViewportCommand::Minimized(false));
                tray_context.send_viewport_cmd(ViewportCommand::Focus);
            } else if event.id == TRAY_OPEN {
                if let Err(error) = open_openmouse() {
                    tracing::error!(%error, "Could not open OpenMouse from the tray");
                }
            } else if event.id == TRAY_QUIT {
                quitting.store(true, Ordering::Release);
                tray_context.send_viewport_cmd(ViewportCommand::Close);
            }
        }));

        Ok(Self { _icon: icon })
    }
}

#[derive(Clone, Copy, Default)]
enum DesktopPage {
    #[default]
    Home,
    Settings,
}

struct BackgroundServer {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl BackgroundServer {
    fn start(
        events: Sender<DesktopEvent>,
        commands: tokio_mpsc::UnboundedReceiver<ServerCommand>,
        window_active: Arc<AtomicBool>,
    ) -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("openmouse-bridge-runtime".into())
            .spawn(move || run_server(events, commands, shutdown_rx, window_active))
            .context("could not start the Bridge runtime")?;
        Ok(Self {
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        })
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
    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
    let window_active = Arc::new(AtomicBool::new(true));
    let server = BackgroundServer::start(event_tx, command_rx, Arc::clone(&window_active))?;
    let app_icon = Arc::new(openmouse_app_icon()?);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 276.0])
            .with_min_inner_size([360.0, 260.0])
            .with_resizable(true)
            .with_decorations(false)
            .with_icon(app_icon)
            .with_transparent(true),
        centered: true,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let ui_result = eframe::run_native(
        "OpenMouse Bridge",
        options,
        Box::new(move |context| {
            Ok(Box::new(BridgeDesktop::new(
                context,
                event_rx,
                command_tx,
                window_active,
            )))
        }),
    )
    .map_err(|error| anyhow!(error.to_string()));
    let server_result = server.stop();
    ui_result.and(server_result)
}

fn run_server(
    events: Sender<DesktopEvent>,
    mut commands: tokio_mpsc::UnboundedReceiver<ServerCommand>,
    shutdown: oneshot::Receiver<()>,
    window_active: Arc<AtomicBool>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not create the Bridge async runtime")?;
    runtime.block_on(async move {
        let (bridge_config, path) = config::load_or_create()?;
        let origins = bridge_config.allowed_origins.clone();
        let service = BridgeService::new(bridge_config, path.clone());
        service.start_game_monitor(Arc::clone(&window_active));

        let command_service = service.clone();
        let command_events = events.clone();
        tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                match command {
                    ServerCommand::SetBatteryThreshold(percent) => {
                        if let Err(error) = command_service.set_battery_threshold(percent).await {
                            tracing::error!(%error, "Could not update the battery threshold");
                        }
                    }
                    ServerCommand::CheckForUpdate => {
                        // Its own task: a slow or hung GitHub request should
                        // not delay other commands (e.g. the battery slider)
                        // queued behind it.
                        let events = command_events.clone();
                        tokio::spawn(async move {
                            let outcome = check_for_update().await;
                            let _ = events.send(DesktopEvent::UpdateCheck(outcome));
                        });
                    }
                }
            }
        });

        let snapshot_service = service.clone();
        let snapshot_events = events.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                // No one is watching while the window is hidden to the tray, so
                // skip building and sending snapshots until it is shown again.
                if !window_active.load(Ordering::Acquire) {
                    continue;
                }
                if snapshot_events
                    .send(DesktopEvent::Snapshot(Box::new(
                        snapshot_service.snapshot().await,
                    )))
                    .is_err()
                {
                    break;
                }
            }
        });

        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), BRIDGE_PORT);
        let listener = match TcpListener::bind(address).await {
            Ok(listener) => listener,
            Err(error) => {
                let message = format!("Could not start on {address}. Is Bridge already running?");
                let _ = events.send(DesktopEvent::ServerError(message));
                return Err(error).context("could not bind the Bridge listener");
            }
        };
        tracing::info!(%address, config = %path.display(), "OpenMouse Bridge is ready");
        let _ = events.send(DesktopEvent::Ready);
        axum::serve(listener, api::router(service, &origins))
            .with_graceful_shutdown(async {
                let _ = shutdown.await;
            })
            .await?;
        Ok(())
    })
}

struct BridgeDesktop {
    events: Receiver<DesktopEvent>,
    valorant_logo: egui::TextureHandle,
    page: DesktopPage,
    discord_rpc_enabled: bool,
    snapshot: Option<BridgeSnapshot>,
    server_ready: bool,
    error: Option<String>,
    _tray: Option<TrayState>,
    quitting: Arc<AtomicBool>,
    window_active: Arc<AtomicBool>,
    showing: bool,
    commands: tokio_mpsc::UnboundedSender<ServerCommand>,
    battery_threshold: Option<u8>,
    update_check: UpdateCheckState,
}

impl BridgeDesktop {
    fn new(
        context: &eframe::CreationContext<'_>,
        events: Receiver<DesktopEvent>,
        commands: tokio_mpsc::UnboundedSender<ServerCommand>,
        window_active: Arc<AtomicBool>,
    ) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "inter".into(),
            egui::FontData::from_static(include_bytes!("../assets/fonts/Inter-Variable.ttf"))
                .into(),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "inter".into());
        context.egui_ctx.set_fonts(fonts);

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = BACKGROUND;
        visuals.override_text_color = Some(TEXT);
        visuals.selection.bg_fill = ACCENT;
        visuals.selection.stroke = Stroke::new(1.0, ACCENT);
        context.egui_ctx.set_visuals(visuals);

        let mut style = (*context.egui_ctx.style_of(egui::Theme::Dark)).clone();
        style.spacing.item_spacing = Vec2::new(8.0, 5.0);
        style.spacing.button_padding = Vec2::new(14.0, 9.0);
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(12.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(11.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::new(10.0, egui::FontFamily::Proportional),
        );
        context.egui_ctx.set_style_of(egui::Theme::Dark, style);

        let valorant_logo = context.egui_ctx.load_texture(
            "valorant-logomark",
            decode_png(include_bytes!("../assets/valorant-logomark.png")),
            egui::TextureOptions::LINEAR,
        );

        let quitting = Arc::new(AtomicBool::new(false));
        let tray = match TrayState::new(
            &context.egui_ctx,
            Arc::clone(&quitting),
            Arc::clone(&window_active),
        ) {
            Ok(tray) => Some(tray),
            Err(error) => {
                tracing::error!(%error, "Could not initialize the system tray");
                None
            }
        };

        Self {
            events,
            valorant_logo,
            page: DesktopPage::default(),
            discord_rpc_enabled: false,
            snapshot: None,
            server_ready: false,
            error: None,
            _tray: tray,
            quitting,
            window_active,
            showing: true,
            commands,
            battery_threshold: None,
            update_check: UpdateCheckState::default(),
        }
    }

    fn receive_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                DesktopEvent::Ready => self.server_ready = true,
                DesktopEvent::Snapshot(snapshot) => self.snapshot = Some(*snapshot),
                DesktopEvent::ServerError(error) => self.error = Some(error),
                DesktopEvent::UpdateCheck(outcome) => {
                    self.update_check = UpdateCheckState::Done(outcome)
                }
            }
        }
    }

    /// Whether the OpenMouse web client has sent a heartbeat recently.
    fn client_connected(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.client_connected)
    }

    /// The battery to feature: the lowest fresh reading, else any reading.
    fn primary_battery(&self) -> Option<DeviceBattery> {
        self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .batteries
                .iter()
                .filter(|battery| !battery.stale)
                .min_by_key(|battery| battery.percent)
                .or_else(|| snapshot.batteries.first())
                .cloned()
        })
    }

    fn battery_hero(&self, ui: &mut egui::Ui) {
        let connected = self.client_connected();
        let battery = self.primary_battery();
        let threshold = self
            .snapshot
            .as_ref()
            .map_or(20, |snapshot| snapshot.battery_threshold_percent);
        let charging = battery.as_ref().is_some_and(|battery| battery.charging);

        let (headline, headline_color) = if self.error.is_some() {
            ("Bridge error", RED)
        } else if connected {
            ("Mouse connected", TEXT)
        } else if self.server_ready {
            ("Waiting for OpenMouse", TEXT)
        } else {
            ("Starting Bridge", TEXT)
        };
        let (pill_label, pill_color) = if self.error.is_some() {
            ("Error", RED)
        } else if connected {
            if charging {
                ("Charging", ACCENT)
            } else {
                ("Live", ACCENT)
            }
        } else if self.server_ready {
            ("Waiting", AMBER)
        } else {
            ("Starting", AMBER)
        };

        let width = ui.available_width();
        egui::Frame::new()
            .fill(SURFACE_RAISED)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_min_width(width - 24.0);
                ui.horizontal(|ui| {
                    let (avatar, _) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::hover());
                    draw_mouse(ui.painter(), avatar);
                    if let Some(battery) = &battery {
                        draw_battery_ring(ui.painter(), avatar, battery.percent, battery.charging);
                    }
                    ui.add_space(12.0);

                    let remaining = ui.available_width();
                    ui.vertical(|ui| {
                        ui.set_min_width(remaining);
                        ui.horizontal(|ui| {
                            ui.set_min_width(remaining);
                            ui.label(
                                RichText::new(headline)
                                    .color(headline_color)
                                    .strong()
                                    .size(13.0),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                pill(ui, pill_label, pill_color);
                            });
                        });
                        ui.add_space(2.0);
                        match &battery {
                            Some(battery) => {
                                battery_bar(ui, battery.percent);
                                ui.add_space(2.0);
                                ui.horizontal(|ui| {
                                    ui.set_min_width(remaining);
                                    let left = if battery.charging {
                                        format!("{} · charging", battery.device_name)
                                    } else {
                                        format!("{} · {}%", battery.device_name, battery.percent)
                                    };
                                    ui.label(RichText::new(left).color(MUTED).size(9.5));
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.label(
                                            RichText::new(format!("Alerts below {threshold}%"))
                                                .color(MUTED)
                                                .size(9.5),
                                        );
                                    });
                                });
                            }
                            None => {
                                let hint = self.error.clone().unwrap_or_else(|| {
                                    "Open OpenMouse to sync your mouse battery".to_owned()
                                });
                                ui.label(RichText::new(hint).color(MUTED).size(9.5));
                            }
                        }
                    });
                });
            });
    }

    fn activity(&self, ui: &mut egui::Ui) {
        let active_game = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.active_games.first());
        let active_profile = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.active_profile.as_ref());
        let subtitle = profile_subtitle(active_profile);
        let width = ui.available_width();
        egui::Frame::new()
            .fill(SURFACE_RAISED)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_min_width(width - 24.0);
                ui.horizontal(|ui| {
                    match active_game {
                        Some(game) if game == "Valorant" => {
                            valorant_icon(ui, &self.valorant_logo);
                        }
                        Some(game) => game_icon(ui, game),
                        None => idle_icon(ui),
                    }
                    ui.add_space(2.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(
                                active_game.map(String::as_str).unwrap_or("No game running"),
                            )
                            .color(if active_game.is_some() { TEXT } else { MUTED })
                            .strong()
                            .size(12.0),
                        );
                        ui.label(RichText::new(subtitle).color(MUTED).size(9.5));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if active_game.is_some() {
                            pill(ui, "RUNNING", ACCENT);
                        } else {
                            pill(ui, "IDLE", MUTED);
                        }
                    });
                });
            });
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if back_button(ui) {
                self.page = DesktopPage::Home;
            }
            ui.label(RichText::new("SETTINGS").color(TEXT).strong().size(12.0));
        });
        ui.add_space(10.0);

        let width = ui.available_width();
        let remaining = ui.available_height();
        let threshold = self
            .battery_threshold
            .or_else(|| {
                self.snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.battery_threshold_percent)
            })
            .unwrap_or(20);
        egui::ScrollArea::vertical()
            .max_height(remaining)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Frame::new()
                    .fill(SURFACE_RAISED)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(12.0)
                    .show(ui, |ui| {
                        ui.set_min_width(width - 24.0);
                        setting_label(
                            ui,
                            "Start Bridge at login",
                            "Keep OpenMouse ready after restart",
                            |ui| {
                                let mut enabled = self
                                    .snapshot
                                    .as_ref()
                                    .is_some_and(|snapshot| snapshot.autostart_enabled);
                                if toggle_switch(ui, &mut enabled, "Start Bridge at login") {
                                    match platform::set_autostart(enabled) {
                                        Ok(()) => {
                                            if let Some(snapshot) = &mut self.snapshot {
                                                snapshot.autostart_enabled = enabled;
                                            }
                                        }
                                        Err(error) => self.error = Some(error.to_string()),
                                    }
                                }
                            },
                        );
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(4.0);
                        setting_label(
                            ui,
                            "Low-battery alert",
                            "Warn when the mouse drops to this level",
                            |ui| {
                                if step_button(ui, "+") {
                                    self.adjust_threshold(threshold, 5);
                                }
                                ui.add_space(2.0);
                                ui.label(
                                    RichText::new(format!("{threshold}%"))
                                        .color(TEXT)
                                        .strong()
                                        .size(11.0),
                                );
                                ui.add_space(2.0);
                                if step_button(ui, "−") {
                                    self.adjust_threshold(threshold, -5);
                                }
                            },
                        );
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(4.0);
                        setting_label(
                            ui,
                            "Discord Rich Presence",
                            "Show your active game and profile",
                            |ui| {
                                toggle_switch(
                                    ui,
                                    &mut self.discord_rpc_enabled,
                                    "Discord Rich Presence",
                                );
                            },
                        );
                        ui.add_space(4.0);
                        ui.separator();
                        setting_row(ui, "Version", BRIDGE_VERSION);
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(4.0);
                        let (subtitle, action) = match &self.update_check {
                            UpdateCheckState::Idle => (
                                "Check GitHub for a newer release".to_owned(),
                                UpdateAction::Check,
                            ),
                            UpdateCheckState::Checking => {
                                ("Checking…".to_owned(), UpdateAction::Wait)
                            }
                            UpdateCheckState::Done(UpdateCheckOutcome::UpToDate) => {
                                ("You're up to date".to_owned(), UpdateAction::Check)
                            }
                            UpdateCheckState::Done(UpdateCheckOutcome::Available { version }) => {
                                (format!("v{version} is available"), UpdateAction::Download)
                            }
                            UpdateCheckState::Done(UpdateCheckOutcome::Failed(message)) => {
                                (format!("Could not check: {message}"), UpdateAction::Check)
                            }
                        };
                        setting_label(ui, "Updates", &subtitle, |ui| match action {
                            UpdateAction::Wait => {
                                ui.label(RichText::new("…").color(MUTED).size(10.0));
                            }
                            UpdateAction::Check => {
                                if link_button(ui, "Check for updates") {
                                    self.update_check = UpdateCheckState::Checking;
                                    let _ = self.commands.send(ServerCommand::CheckForUpdate);
                                }
                            }
                            UpdateAction::Download => {
                                if link_button(ui, "Download")
                                    && let Err(error) = open_url(RELEASES_PAGE_URL)
                                {
                                    self.error = Some(error.to_string());
                                }
                            }
                        });
                    });
            });
    }

    fn adjust_threshold(&mut self, current: u8, delta: i8) {
        let next = (i16::from(current) + i16::from(delta)).clamp(5, 50) as u8;
        if next != current {
            self.battery_threshold = Some(next);
            if let Some(snapshot) = &mut self.snapshot {
                snapshot.battery_threshold_percent = next;
            }
            let _ = self.commands.send(ServerCommand::SetBatteryThreshold(next));
        }
    }

    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        egui::Frame::new()
            .fill(SURFACE)
            .corner_radius(CornerRadius {
                nw: 10,
                ne: 10,
                sw: 0,
                se: 0,
            })
            .inner_margin(egui::Margin::symmetric(12, 6))
            .show(ui, |ui| {
                ui.set_min_width(width - 24.0);
                let (bar, drag) =
                    ui.allocate_exact_size(Vec2::new(width - 24.0, 26.0), Sense::drag());
                let dot_color = if self.error.is_some() {
                    RED
                } else if self.client_connected() {
                    ACCENT
                } else {
                    AMBER
                };
                ui.painter().circle_filled(
                    egui::pos2(bar.left() + 4.0, bar.center().y),
                    3.5,
                    dot_color,
                );
                let title_end = ui.painter().text(
                    egui::pos2(bar.left() + 14.0, bar.center().y),
                    egui::Align2::LEFT_CENTER,
                    "OpenMouse Bridge",
                    egui::FontId::proportional(11.5),
                    TEXT,
                );
                ui.painter().text(
                    egui::pos2(title_end.right() + 8.0, bar.center().y),
                    egui::Align2::LEFT_CENTER,
                    BRIDGE_VERSION,
                    egui::FontId::proportional(9.5),
                    MUTED,
                );
                if drag.drag_started() {
                    ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
                }

                let controls_rect = egui::Rect::from_min_max(
                    egui::pos2(bar.right() - 88.0, bar.top()),
                    bar.right_bottom(),
                );
                let mut controls = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(controls_rect)
                        .layout(Layout::right_to_left(Align::Center)),
                );
                let close = egui::Button::new(RichText::new("×").color(MUTED).size(16.0))
                    .frame(false)
                    .min_size(Vec2::new(24.0, 24.0));
                if controls.add(close).on_hover_text("Close Bridge").clicked() {
                    controls
                        .ctx()
                        .send_viewport_cmd(ViewportCommand::Visible(false));
                    self.showing = false;
                }
                let minimize = egui::Button::new(RichText::new("−").color(MUTED).size(16.0))
                    .frame(false)
                    .min_size(Vec2::new(24.0, 24.0));
                if controls.add(minimize).on_hover_text("Minimize").clicked() {
                    controls
                        .ctx()
                        .send_viewport_cmd(ViewportCommand::Minimized(true));
                    self.showing = false;
                }
                if settings_button(&mut controls) {
                    self.page = DesktopPage::Settings;
                }
            });
    }
}

impl eframe::App for BridgeDesktop {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Color32::TRANSPARENT.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_events();

        // Gaining focus (tray "Show", dock click, or a click on the window)
        // means the window is on screen again and should refresh live.
        if ui.ctx().input(|input| input.viewport().focused) == Some(true) {
            self.showing = true;
        }
        if ui.ctx().input(|input| input.viewport().close_requested())
            && !self.quitting.load(Ordering::Acquire)
        {
            ui.ctx().send_viewport_cmd(ViewportCommand::CancelClose);
            ui.ctx().send_viewport_cmd(ViewportCommand::Visible(false));
            self.showing = false;
        }

        egui::Frame::new()
            .fill(BACKGROUND)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(10))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                self.title_bar(ui);
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(18, 16))
                    .show(ui, |ui| match self.page {
                        DesktopPage::Home => {
                            self.battery_hero(ui);
                            ui.add_space(6.0);
                            self.activity(ui);
                            ui.add_space(8.0);
                            let button = egui::Button::new(
                                RichText::new("Open OpenMouse")
                                    .color(BACKGROUND)
                                    .strong()
                                    .size(12.0),
                            )
                            .fill(ACCENT)
                            .stroke(Stroke::NONE)
                            .corner_radius(CornerRadius::same(6));
                            if ui.add_sized([ui.available_width(), 34.0], button).clicked()
                                && let Err(error) = open_openmouse()
                            {
                                self.error = Some(error.to_string());
                            }
                        }
                        DesktopPage::Settings => self.settings(ui),
                    });
            });

        handle_edge_resize(ui.ctx());

        // Only keep the render loop alive while the window is actually on
        // screen. Once it is hidden to the tray we stop repainting entirely and
        // pause snapshot production, so an idle Bridge costs almost nothing.
        self.window_active.store(self.showing, Ordering::Release);
        if self.showing {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
    }
}

/// Detects the pointer sitting in a thin band along the window's edges and
/// starts an OS-native interactive resize on press, plus sets a matching
/// resize cursor on hover. Needed because `with_decorations(false)` (the
/// custom-drawn titlebar) means there is no native chrome to drag from —
/// checked last so it wins the cursor for the true edge pixels over
/// whatever other widgets requested that frame.
fn handle_edge_resize(ctx: &egui::Context) {
    const MARGIN: f32 = 6.0;

    let Some(pointer) = ctx.input(|input| input.pointer.hover_pos()) else {
        return;
    };
    let rect = ctx.input(|input| input.viewport_rect());
    let north = pointer.y <= rect.top() + MARGIN;
    let south = pointer.y >= rect.bottom() - MARGIN;
    let west = pointer.x <= rect.left() + MARGIN;
    let east = pointer.x >= rect.right() - MARGIN;

    use egui::{CursorIcon, viewport::ResizeDirection as Dir};
    let zone = match (north, south, west, east) {
        (true, _, true, _) => Some((CursorIcon::ResizeNwSe, Dir::NorthWest)),
        (true, _, _, true) => Some((CursorIcon::ResizeNeSw, Dir::NorthEast)),
        (_, true, true, _) => Some((CursorIcon::ResizeNeSw, Dir::SouthWest)),
        (_, true, _, true) => Some((CursorIcon::ResizeNwSe, Dir::SouthEast)),
        (true, false, false, false) => Some((CursorIcon::ResizeVertical, Dir::North)),
        (false, true, false, false) => Some((CursorIcon::ResizeVertical, Dir::South)),
        (false, false, true, false) => Some((CursorIcon::ResizeHorizontal, Dir::West)),
        (false, false, false, true) => Some((CursorIcon::ResizeHorizontal, Dir::East)),
        _ => None,
    };
    let Some((cursor, direction)) = zone else {
        return;
    };
    ctx.set_cursor_icon(cursor);
    if ctx.input(|input| input.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(direction));
    }
}

fn valorant_icon(ui: &mut egui::Ui, texture: &egui::TextureHandle) {
    ui.add(egui::Image::new(texture).fit_to_exact_size(Vec2::new(40.0, 24.0)));
}

fn game_icon(ui: &mut egui::Ui, game: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(40.0, 24.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 11.0, SURFACE);
    let initial = game.chars().next().unwrap_or('?').to_string();
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(11.0),
        TEXT,
    );
}

fn idle_icon(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(40.0, 24.0), Sense::hover());
    ui.painter()
        .circle_stroke(rect.center(), 10.0, Stroke::new(1.0, BORDER));
    ui.painter().circle_filled(rect.center(), 2.5, MUTED);
}

fn decode_png(bytes: &[u8]) -> egui::ColorImage {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .expect("embedded logo must be valid PNG");
    let mut pixels = vec![
        0;
        reader
            .output_buffer_size()
            .expect("logo is within PNG limits")
    ];
    let info = reader
        .next_frame(&mut pixels)
        .expect("embedded logo must decode");
    egui::ColorImage::from_rgba_unmultiplied(
        [info.width as usize, info.height as usize],
        &pixels[..info.buffer_size()],
    )
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

fn openmouse_app_icon() -> Result<egui::IconData> {
    let (rgba, width, height) =
        decode_icon_rgba(include_bytes!("../assets/openmouse-app-icon.png"))?;
    Ok(egui::IconData {
        rgba,
        width,
        height,
    })
}

fn decode_icon_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("could not decode the icon")?;
    let mut pixels = vec![
        0;
        reader
            .output_buffer_size()
            .expect("icon is within PNG limits")
    ];
    let info = reader
        .next_frame(&mut pixels)
        .context("could not read the icon")?;
    pixels.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => pixels,
        png::ColorType::Rgb => pixels
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        _ => return Err(anyhow!("the icon must decode as RGB or RGBA")),
    };
    Ok((rgba, info.width, info.height))
}

fn tray_icon() -> Result<Icon> {
    let (rgba, width, height) = openmouse_icon_rgba()?;
    Icon::from_rgba(rgba, width, height).context("could not create the OpenMouse tray icon")
}

fn settings_button(ui: &mut egui::Ui) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
    let color = if response.hovered() { TEXT } else { MUTED };
    let center = rect.center();
    let stroke = Stroke::new(1.4, color);
    ui.painter().circle_stroke(center, 6.0, stroke);
    ui.painter().circle_stroke(center, 2.0, stroke);
    for index in 0..8 {
        let angle = index as f32 * std::f32::consts::TAU / 8.0;
        let direction = egui::vec2(angle.cos(), angle.sin());
        ui.painter()
            .line_segment([center + direction * 7.0, center + direction * 9.0], stroke);
    }
    response.on_hover_text("Bridge settings").clicked()
}

fn back_button(ui: &mut egui::Ui) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(62.0, 28.0), Sense::click());
    let fill = if response.hovered() {
        SURFACE_RAISED
    } else {
        SURFACE
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(6),
        fill,
        Stroke::new(1.0, BORDER),
        egui::StrokeKind::Inside,
    );
    let color = if response.hovered() { TEXT } else { MUTED };
    let arrow_center = egui::pos2(rect.left() + 14.0, rect.center().y);
    let stroke = Stroke::new(1.5, color);
    ui.painter().line_segment(
        [
            arrow_center + egui::vec2(3.0, -4.0),
            arrow_center + egui::vec2(-1.0, 0.0),
        ],
        stroke,
    );
    ui.painter().line_segment(
        [
            arrow_center + egui::vec2(-1.0, 0.0),
            arrow_center + egui::vec2(3.0, 4.0),
        ],
        stroke,
    );
    ui.painter().text(
        egui::pos2(rect.left() + 25.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "Back",
        egui::FontId::proportional(10.0),
        color,
    );
    response.clicked()
}

fn toggle_switch(ui: &mut egui::Ui, enabled: &mut bool, tooltip: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(38.0, 22.0), Sense::click());
    let clicked = response.clicked();
    if clicked {
        *enabled = !*enabled;
    }
    let track = if *enabled { ACCENT } else { BORDER };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(11), track);
    let knob_x = if *enabled {
        rect.right() - 11.0
    } else {
        rect.left() + 11.0
    };
    ui.painter().circle_filled(
        egui::pos2(knob_x, rect.center().y),
        8.0,
        if *enabled { BACKGROUND } else { MUTED },
    );
    response.on_hover_text(tooltip);
    clicked
}

/// A settings row: a two-line title/subtitle on the left and a right-aligned
/// control rendered by `control`.
fn setting_label(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: &str,
    control: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).color(TEXT).size(11.0));
            ui.label(RichText::new(subtitle).color(MUTED).size(9.0));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
}

enum UpdateAction {
    /// A check is already in flight; nothing to click.
    Wait,
    Check,
    Download,
}

fn link_button(ui: &mut egui::Ui, label: &str) -> bool {
    let button =
        egui::Button::new(RichText::new(label).color(ACCENT).strong().size(10.0)).frame(false);
    ui.add(button).clicked()
}

fn step_button(ui: &mut egui::Ui, symbol: &str) -> bool {
    let button = egui::Button::new(RichText::new(symbol).color(TEXT).size(13.0))
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(5))
        .min_size(Vec2::new(22.0, 22.0));
    ui.add(button).clicked()
}

fn setting_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.set_min_height(26.0);
        ui.label(RichText::new(label).color(MUTED).size(10.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).color(TEXT).size(10.0));
        });
    });
}

fn profile_subtitle(profile: Option<&openmouse_bridge::config::ApplicationProfile>) -> String {
    match profile {
        Some(profile) => {
            let mut parts = vec![profile.application.name.clone()];
            if let Some(dpi) = profile.settings.dpi {
                parts.push(format!("{dpi} DPI"));
            }
            if let Some(rate) = profile.settings.polling_rate_hz {
                parts.push(format!("{rate} Hz"));
            }
            parts.join("  ·  ")
        }
        None => "No saved profile for the active app".to_owned(),
    }
}

fn battery_color(percent: u8) -> Color32 {
    if percent > 50 {
        ACCENT
    } else if percent > 20 {
        AMBER
    } else {
        RED
    }
}

/// Draw a small mouse silhouette inside `rect`.
fn draw_mouse(painter: &egui::Painter, rect: egui::Rect) {
    let body = egui::Rect::from_center_size(rect.center(), Vec2::new(24.0, 34.0));
    painter.rect_filled(body, CornerRadius::same(12), SURFACE);
    painter.rect_stroke(
        body,
        CornerRadius::same(12),
        Stroke::new(1.2, BORDER),
        egui::StrokeKind::Inside,
    );
    painter.line_segment(
        [
            egui::pos2(body.center().x, body.top() + 4.0),
            egui::pos2(body.center().x, body.center().y - 1.0),
        ],
        Stroke::new(1.0, BORDER),
    );
    painter.rect_filled(
        egui::Rect::from_center_size(
            egui::pos2(body.center().x, body.top() + 9.0),
            Vec2::new(2.5, 6.0),
        ),
        CornerRadius::same(2),
        MUTED,
    );
}

/// Overlay a battery percentage badge on the lower-right of the mouse body.
fn draw_battery_ring(painter: &egui::Painter, rect: egui::Rect, percent: u8, charging: bool) {
    let body = egui::Rect::from_center_size(rect.center(), Vec2::new(24.0, 34.0));
    let center = egui::pos2(body.right() - 1.0, body.bottom() - 2.0);
    let color = if charging {
        ACCENT
    } else {
        battery_color(percent)
    };
    painter.circle_filled(center, 10.0, BACKGROUND);
    painter.circle_stroke(center, 8.5, Stroke::new(2.0, color));
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        percent.to_string(),
        egui::FontId::proportional(if percent >= 100 { 7.0 } else { 8.5 }),
        color,
    );
}

fn battery_bar(ui: &mut egui::Ui, percent: u8) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 5.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(3), BORDER);
    let fill_width = (rect.width() * f32::from(percent) / 100.0).clamp(3.0, rect.width());
    let fill = egui::Rect::from_min_size(rect.min, Vec2::new(fill_width, rect.height()));
    painter.rect_filled(fill, CornerRadius::same(3), battery_color(percent));
}

/// A small rounded status pill with a leading dot.
fn pill(ui: &mut egui::Ui, label: &str, color: Color32) {
    let tint = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 28);
    egui::Frame::new()
        .fill(tint)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(6.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.0, color);
                ui.add_space(1.0);
                ui.label(RichText::new(label).color(color).strong().size(9.5));
            });
        });
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
