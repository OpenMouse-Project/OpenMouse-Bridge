use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, RichText, Sense, Stroke, Vec2, ViewportCommand,
};
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
            .with_inner_size([420.0, 360.0])
            .with_min_inner_size([420.0, 360.0])
            .with_max_inner_size([420.0, 360.0])
            .with_resizable(false)
            .with_decorations(false),
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
            .corner_radius(CornerRadius::same(8))
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (color, label) = if self.error.is_some() {
                        (Color32::from_rgb(239, 112, 112), "NEEDS ATTENTION")
                    } else if self.server_ready {
                        (ACCENT, "RUNNING")
                    } else {
                        (Color32::from_rgb(232, 184, 93), "STARTING")
                    };
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, color);
                    ui.label(RichText::new(label).color(color).strong().size(11.0));
                });
                ui.add_space(4.0);
                if let Some(error) = &self.error {
                    ui.label(RichText::new(error).color(TEXT).size(12.0));
                } else {
                    ui.label(
                        RichText::new("Ready for profiles, game detection, and battery alerts.")
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
        egui::Frame::new()
            .fill(SURFACE_RAISED)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(12, 5))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                detail_row(ui, "Version", BRIDGE_VERSION);
                ui.separator();
                detail_row(ui, "Application profiles", &profiles);
                ui.separator();
                detail_row(ui, "Foreground application", &applications);
                ui.separator();
                detail_row(ui, "Active game", &active_game);
            });
    }

    fn title_bar(&self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(SURFACE)
            .inner_margin(egui::Margin::symmetric(12, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let drag_width = (ui.available_width() - 62.0).max(100.0);
                    let title = ui
                        .add_sized(
                            [drag_width, 26.0],
                            egui::Label::new(
                                RichText::new("OPENMOUSE  /  BRIDGE")
                                    .color(TEXT)
                                    .strong()
                                    .size(11.0),
                            )
                            .halign(Align::Min),
                        )
                        .interact(Sense::drag());
                    if title.drag_started() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
                    }

                    let minimize = egui::Button::new(RichText::new("−").color(MUTED).size(16.0))
                        .frame(false)
                        .min_size(Vec2::new(24.0, 24.0));
                    if ui.add(minimize).on_hover_text("Minimize").clicked() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
                    }

                    let close = egui::Button::new(RichText::new("×").color(MUTED).size(16.0))
                        .frame(false)
                        .min_size(Vec2::new(24.0, 24.0));
                    if ui.add(close).on_hover_text("Close Bridge").clicked() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                    }
                });
            });
    }
}

impl eframe::App for BridgeDesktop {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_events();
        ui.ctx().request_repaint_after(Duration::from_millis(500));

        egui::Frame::new().fill(BACKGROUND).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            self.title_bar(ui);
            egui::Frame::new().inner_margin(20.0).show(ui, |ui| {
                self.status_card(ui);
                ui.add_space(10.0);
                self.details(ui);
                ui.add_space(12.0);
                let button = egui::Button::new(
                    RichText::new("Open OpenMouse  →")
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
            });
        });
    }
}

fn detail_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.set_min_height(25.0);
        ui.label(RichText::new(label).color(MUTED).size(11.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(
                egui::Label::new(RichText::new(value).color(TEXT).strong().size(11.0)).truncate(),
            );
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
