use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use eframe::egui::{self, Align, Color32, CornerRadius, Layout, RichText, Stroke, Vec2};
use openmouse_bridge::{
    BRIDGE_PORT, BRIDGE_VERSION, api, config,
    service::{BridgeService, BridgeSnapshot},
};
#[cfg(target_os = "windows")]
use std::ptr::null_mut;
use tokio::{net::TcpListener, sync::oneshot};
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

enum DesktopEvent {
    Ready,
    Snapshot(BridgeSnapshot),
    ServerError(String),
}

struct BackgroundServer {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl BackgroundServer {
    fn start(events: Sender<DesktopEvent>) -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("openmouse-bridge-runtime".into())
            .spawn(move || run_server(events, shutdown_rx))
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
    let server = BackgroundServer::start(event_tx)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 390.0])
            .with_min_inner_size([420.0, 390.0])
            .with_max_inner_size([420.0, 390.0])
            .with_resizable(false),
        centered: true,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let ui_result = eframe::run_native(
        "OpenMouse Bridge",
        options,
        Box::new(move |context| Ok(Box::new(BridgeDesktop::new(context, event_rx)))),
    )
    .map_err(|error| anyhow!(error.to_string()));
    let server_result = server.stop();
    ui_result.and(server_result)
}

fn run_server(events: Sender<DesktopEvent>, shutdown: oneshot::Receiver<()>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not create the Bridge async runtime")?;
    runtime.block_on(async move {
        let (bridge_config, path) = config::load_or_create()?;
        let origins = bridge_config.allowed_origins.clone();
        let service = BridgeService::new(bridge_config, path.clone());
        service.start_game_monitor();

        let snapshot_service = service.clone();
        let snapshot_events = events.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                if snapshot_events
                    .send(DesktopEvent::Snapshot(snapshot_service.snapshot().await))
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
    snapshot: Option<BridgeSnapshot>,
    server_ready: bool,
    error: Option<String>,
}

impl BridgeDesktop {
    fn new(context: &eframe::CreationContext<'_>, events: Receiver<DesktopEvent>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = BACKGROUND;
        visuals.override_text_color = Some(TEXT);
        visuals.selection.bg_fill = ACCENT;
        visuals.selection.stroke = Stroke::new(1.0, ACCENT);
        context.egui_ctx.set_visuals(visuals);

        let mut style = (*context.egui_ctx.style_of(egui::Theme::Dark)).clone();
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(14.0, 9.0);
        context.egui_ctx.set_style_of(egui::Theme::Dark, style);

        Self {
            events,
            snapshot: None,
            server_ready: false,
            error: None,
        }
    }

    fn receive_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                DesktopEvent::Ready => self.server_ready = true,
                DesktopEvent::Snapshot(snapshot) => self.snapshot = Some(snapshot),
                DesktopEvent::ServerError(error) => self.error = Some(error),
            }
        }
    }

    fn status_card(&self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (color, label) = if self.error.is_some() {
                        (Color32::from_rgb(239, 112, 112), "NEEDS ATTENTION")
                    } else if self.server_ready {
                        (ACCENT, "RUNNING")
                    } else {
                        (Color32::from_rgb(232, 184, 93), "STARTING")
                    };
                    ui.label(RichText::new("●").color(color).size(15.0));
                    ui.label(RichText::new(label).color(color).strong().size(12.0));
                });
                ui.add_space(8.0);
                if let Some(error) = &self.error {
                    ui.label(RichText::new(error).color(TEXT).size(14.0));
                } else {
                    ui.label(
                        RichText::new("OpenMouse can connect to this computer.")
                            .color(TEXT)
                            .size(14.0),
                    );
                    ui.label(
                        RichText::new("Keep Bridge open for profiles and battery alerts.")
                            .color(MUTED)
                            .size(12.0),
                    );
                }
            });
    }

    fn details(&self, ui: &mut egui::Ui) {
        let (profiles, applications, active_game) =
            self.snapshot
                .as_ref()
                .map_or(("—".into(), "—".into(), "None".into()), |snapshot| {
                    (
                        snapshot.profile_count.to_string(),
                        snapshot
                            .foreground_application
                            .as_ref()
                            .map_or_else(|| "None".into(), |application| application.name.clone()),
                        snapshot
                            .active_games
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "None".into()),
                    )
                });
        detail_row(ui, "Version", BRIDGE_VERSION);
        detail_row(ui, "Application profiles", &profiles);
        detail_row(ui, "Foreground application", &applications);
        detail_row(ui, "Active game", &active_game);
    }
}

impl eframe::App for BridgeDesktop {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_events();
        ui.ctx().request_repaint_after(Duration::from_millis(500));

        egui::Frame::new()
            .fill(BACKGROUND)
            .inner_margin(24.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("OPENMOUSE").color(ACCENT).strong().size(11.0));
                        ui.label(RichText::new("Bridge").color(TEXT).strong().size(25.0));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("LOCAL COMPANION").color(MUTED).size(10.0));
                    });
                });
                ui.add_space(14.0);
                self.status_card(ui);
                ui.add_space(14.0);
                self.details(ui);
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let mut autostart = false;
                    ui.add_enabled(
                        false,
                        egui::Checkbox::new(&mut autostart, "Start with Windows"),
                    );
                    ui.label(
                        RichText::new("Available with tray controls")
                            .color(MUTED)
                            .size(11.0),
                    );
                });
                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.horizontal(|ui| {
                        let button = egui::Button::new(RichText::new("Open OpenMouse").strong())
                            .fill(ACCENT)
                            .stroke(Stroke::NONE)
                            .corner_radius(CornerRadius::same(7));
                        if ui.add(button).clicked()
                            && let Err(error) = open_openmouse()
                        {
                            self.error = Some(error.to_string());
                        }
                        ui.label(
                            RichText::new("Closing this window stops Bridge.")
                                .color(MUTED)
                                .size(11.0),
                        );
                    });
                });
            });
    }
}

fn detail_row(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(SURFACE_RAISED)
        .corner_radius(CornerRadius::same(7))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).color(MUTED).size(12.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(value).color(TEXT).strong().size(12.0))
                            .truncate(),
                    );
                });
            });
        });
}

#[cfg(target_os = "windows")]
fn open_openmouse() -> Result<()> {
    let operation = wide("open");
    let target = wide(OPENMOUSE_URL);
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
        return Err(anyhow!("Windows could not open OpenMouse (code {result})"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_openmouse() -> Result<()> {
    let status = std::process::Command::new("open")
        .arg(OPENMOUSE_URL)
        .status()
        .context("macOS could not open OpenMouse")?;
    if !status.success() {
        return Err(anyhow!("macOS could not open OpenMouse"));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
