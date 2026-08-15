use std::{
    io::Cursor,
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
    BRIDGE_PORT, BRIDGE_VERSION, api, config, platform,
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
            .with_inner_size([420.0, 330.0])
            .with_min_inner_size([420.0, 330.0])
            .with_max_inner_size([420.0, 330.0])
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
    valorant_logo: egui::TextureHandle,
    page: DesktopPage,
    discord_rpc_enabled: bool,
    snapshot: Option<BridgeSnapshot>,
    server_ready: bool,
    error: Option<String>,
}

impl BridgeDesktop {
    fn new(context: &eframe::CreationContext<'_>, events: Receiver<DesktopEvent>) -> Self {
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
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
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

        Self {
            events,
            valorant_logo,
            page: DesktopPage::default(),
            discord_rpc_enabled: false,
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
        let width = ui.available_width();
        egui::Frame::new()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_min_width(width - 24.0);
                ui.horizontal(|ui| {
                    let (color, label) = if self.error.is_some() {
                        (Color32::from_rgb(239, 112, 112), "BRIDGE ERROR")
                    } else if self.server_ready {
                        (ACCENT, "BRIDGE CONNECTED")
                    } else {
                        (Color32::from_rgb(232, 184, 93), "CONNECTING")
                    };
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, color);
                    ui.label(RichText::new(label).color(color).strong().size(11.0));
                });
                if let Some(error) = &self.error {
                    ui.add_space(4.0);
                    ui.label(RichText::new(error).color(TEXT).size(12.0));
                }
            });
    }

    fn activity(&self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        egui::Frame::new()
            .fill(SURFACE_RAISED)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_min_width(width - 24.0);
                ui.horizontal(|ui| {
                    valorant_icon(ui, &self.valorant_logo);
                    ui.label(RichText::new("Valorant").color(TEXT).strong().size(12.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("RUNNING").color(ACCENT).strong().size(10.0));
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 3.0, ACCENT);
                    });
                });
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.set_min_height(28.0);
                    ui.label(RichText::new("Active profile").color(MUTED).size(11.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("Valorant").color(TEXT).strong().size(11.0));
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
        egui::Frame::new()
            .fill(SURFACE_RAISED)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_min_width(width - 24.0);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Start Bridge at login")
                                .color(TEXT)
                                .size(11.0),
                        );
                        ui.label(
                            RichText::new("Keep OpenMouse ready after restart")
                                .color(MUTED)
                                .size(9.0),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
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
                    });
                });
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Discord Rich Presence")
                                .color(TEXT)
                                .size(11.0),
                        );
                        ui.label(
                            RichText::new("Show your active game and profile")
                                .color(MUTED)
                                .size(9.0),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        toggle_switch(ui, &mut self.discord_rpc_enabled, "Discord Rich Presence");
                    });
                });
                ui.add_space(4.0);
                ui.separator();
                setting_row(ui, "Version", BRIDGE_VERSION);
            });
    }

    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        egui::Frame::new()
            .fill(SURFACE)
            .inner_margin(egui::Margin::symmetric(12, 6))
            .show(ui, |ui| {
                ui.set_min_width(width - 24.0);
                let (bar, drag) =
                    ui.allocate_exact_size(Vec2::new(width - 24.0, 26.0), Sense::drag());
                ui.painter().text(
                    egui::pos2(bar.left(), bar.center().y),
                    egui::Align2::LEFT_CENTER,
                    format!("OPENMOUSE  /  BRIDGE  {BRIDGE_VERSION}"),
                    egui::FontId::proportional(11.0),
                    TEXT,
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
                    controls.ctx().send_viewport_cmd(ViewportCommand::Close);
                }
                let minimize = egui::Button::new(RichText::new("−").color(MUTED).size(16.0))
                    .frame(false)
                    .min_size(Vec2::new(24.0, 24.0));
                if controls.add(minimize).on_hover_text("Minimize").clicked() {
                    controls
                        .ctx()
                        .send_viewport_cmd(ViewportCommand::Minimized(true));
                }
                if settings_button(&mut controls) {
                    self.page = DesktopPage::Settings;
                }
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
            egui::Frame::new()
                .inner_margin(20.0)
                .show(ui, |ui| match self.page {
                    DesktopPage::Home => {
                        self.status_card(ui);
                        ui.add_space(10.0);
                        self.activity(ui);
                        ui.add_space(12.0);
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
    }
}

fn valorant_icon(ui: &mut egui::Ui, texture: &egui::TextureHandle) {
    ui.add(egui::Image::new(texture).fit_to_exact_size(Vec2::new(40.0, 24.0)));
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

fn setting_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.set_min_height(26.0);
        ui.label(RichText::new(label).color(MUTED).size(10.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).color(TEXT).size(10.0));
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
