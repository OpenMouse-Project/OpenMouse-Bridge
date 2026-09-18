use std::{
    env,
    io::Cursor,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::AtomicBool,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use eframe::egui::{
    self, Align, Button, Color32, Frame, Layout, Pos2, RichText, Sense, Stroke, Vec2,
    ViewportCommand,
};
use openmouse_bridge::{
    BRIDGE_PORT, api, config, platform,
    service::{BridgeService, BridgeSnapshot},
};
#[cfg(target_os = "windows")]
use std::ptr::null_mut;
use tokio::{net::TcpListener, sync::oneshot};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, Rect as TrayRect, TrayIcon, TrayIconBuilder, TrayIconEvent,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

const OPENMOUSE_URL: &str = "https://control.openmouse.app";
const WINDOW_WIDTH: f32 = 360.0;
const WINDOW_HEIGHT: f32 = 460.0;
const BACKGROUND: Color32 = Color32::from_rgb(12, 14, 16);
const SURFACE: Color32 = Color32::from_rgb(24, 27, 30);
const SURFACE_HOVER: Color32 = Color32::from_rgb(31, 35, 39);
const TEXT: Color32 = Color32::from_rgb(239, 243, 241);
const MUTED: Color32 = Color32::from_rgb(151, 160, 158);
const ACCENT: Color32 = Color32::from_rgb(93, 222, 137);
const DANGER: Color32 = Color32::from_rgb(248, 113, 113);

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
    snapshot: Option<BridgeSnapshot>,
    visible: bool,
    visibility_initialized: bool,
    exit_requested: bool,
    autostart: bool,
    last_error: Option<String>,
}

impl TrayApp {
    fn new(
        context: &egui::Context,
        snapshots: Receiver<BridgeSnapshot>,
        visible: bool,
    ) -> Result<Self> {
        configure_tray_only_application();
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = BACKGROUND;
        visuals.override_text_color = Some(TEXT);
        visuals.widgets.inactive.bg_fill = SURFACE;
        visuals.widgets.hovered.bg_fill = SURFACE_HOVER;
        visuals.widgets.active.bg_fill = SURFACE_HOVER;
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
            snapshot: None,
            visible,
            visibility_initialized: false,
            exit_requested: false,
            autostart: platform::autostart_enabled(),
            last_error: None,
        })
    }

    fn set_visible(&mut self, context: &egui::Context, visible: bool) {
        self.visible = visible;
        context.send_viewport_cmd(ViewportCommand::Visible(visible));
        if visible {
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
            self.snapshot = Some(snapshot);
        }
    }
}

impl eframe::App for TrayApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.refresh_snapshot();
        if !self.visibility_initialized {
            self.visibility_initialized = true;
            context.send_viewport_cmd(ViewportCommand::Visible(self.visible));
        }
        if let Some(rect) = self.tray.toggle_requested() {
            tracing::debug!(?rect, visible = self.visible, "handling tray toggle");
            if self.visible {
                self.set_visible(context, false);
            } else {
                self.show_near_tray(context, rect);
            }
        }
        if context.input(|input| input.viewport().close_requested()) && !self.exit_requested {
            context.send_viewport_cmd(ViewportCommand::CancelClose);
            self.set_visible(context, false);
        }
        context.request_repaint_after(Duration::from_secs(1));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        Frame::NONE
            .fill(BACKGROUND)
            .inner_margin(20.0)
            .show(ui, |ui| {
                let header = ui.horizontal(|ui| {
                    let (logo_rect, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
                    ui.put(
                        egui::Rect::from_center_size(logo_rect.center(), Vec2::new(24.0, 35.5)),
                        egui::Image::new((self.logo.id(), Vec2::new(24.0, 35.5))),
                    );
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("OpenMouse Bridge")
                                .size(17.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.label(
                            RichText::new(format!(
                                "Native companion · v{}",
                                openmouse_bridge::BRIDGE_VERSION
                            ))
                            .size(11.0)
                            .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                Button::new(RichText::new("×").size(20.0).color(MUTED))
                                    .frame(false),
                            )
                            .clicked()
                        {
                            self.set_visible(ui.ctx(), false);
                        }
                    });
                });
                if header.response.drag_started() {
                    ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
                }

                ui.add_space(18.0);
                Frame::NONE
                    .fill(SURFACE)
                    .corner_radius(14.0)
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        let connected = self
                            .snapshot
                            .as_ref()
                            .is_some_and(|snapshot| snapshot.client_connected);
                        ui.horizontal(|ui| {
                            let (dot_rect, _) =
                                ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                            ui.painter().circle_filled(dot_rect.center(), 4.0, ACCENT);
                            ui.label(RichText::new("Bridge is running").strong().color(TEXT));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("127.0.0.1:{BRIDGE_PORT}"))
                                        .monospace()
                                        .size(10.0)
                                        .color(MUTED),
                                );
                            });
                        });
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(if connected {
                                "Control panel connected"
                            } else {
                                "Waiting for the control panel"
                            })
                            .size(12.0)
                            .color(MUTED),
                        );
                        if let Some(snapshot) = &self.snapshot {
                            ui.add_space(12.0);
                            ui.separator();
                            ui.add_space(10.0);
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new("PROFILES").size(9.0).color(MUTED).strong(),
                                    );
                                    ui.label(
                                        RichText::new(snapshot.profile_count.to_string())
                                            .size(18.0)
                                            .strong(),
                                    );
                                });
                                ui.add_space(28.0);
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new("TRACKED GAMES")
                                            .size(9.0)
                                            .color(MUTED)
                                            .strong(),
                                    );
                                    ui.label(
                                        RichText::new(snapshot.tracked_game_count.to_string())
                                            .size(18.0)
                                            .strong(),
                                    );
                                });
                            });
                        }
                    });

                ui.add_space(12.0);
                Frame::NONE
                    .fill(SURFACE)
                    .corner_radius(14.0)
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new("Run on startup").strong().color(TEXT));
                                ui.label(
                                    RichText::new("Start Bridge when you sign in")
                                        .size(11.0)
                                        .color(MUTED),
                                );
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let label = if self.autostart { "ON" } else { "OFF" };
                                let fill = if self.autostart {
                                    ACCENT
                                } else {
                                    SURFACE_HOVER
                                };
                                let text = if self.autostart { BACKGROUND } else { MUTED };
                                if ui
                                    .add(
                                        Button::new(
                                            RichText::new(label).size(10.0).strong().color(text),
                                        )
                                        .fill(fill)
                                        .stroke(Stroke::NONE)
                                        .corner_radius(999.0)
                                        .min_size(Vec2::new(48.0, 26.0)),
                                    )
                                    .clicked()
                                {
                                    let enabled = !self.autostart;
                                    match platform::set_autostart(enabled) {
                                        Ok(()) => {
                                            self.autostart = enabled;
                                            self.last_error = None;
                                        }
                                        Err(error) => self.last_error = Some(error.to_string()),
                                    }
                                }
                            });
                        });
                    });

                if let Some(error) = &self.last_error {
                    ui.add_space(8.0);
                    ui.label(RichText::new(error).size(11.0).color(DANGER));
                }

                ui.add_space(16.0);
                if ui
                    .add_sized(
                        [ui.available_width(), 42.0],
                        Button::new(
                            RichText::new("Open control panel")
                                .strong()
                                .color(BACKGROUND),
                        )
                        .fill(ACCENT)
                        .stroke(Stroke::NONE)
                        .corner_radius(10.0),
                    )
                    .clicked()
                    && let Err(error) = open_openmouse()
                {
                    self.last_error = Some(error.to_string());
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(platform::platform_name())
                            .size(10.0)
                            .color(MUTED),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                Button::new(RichText::new("Quit Bridge").color(DANGER))
                                    .frame(false),
                            )
                            .clicked()
                        {
                            self.exit_requested = true;
                            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                        }
                    });
                });
            });
    }
}

struct BackgroundServer {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl BackgroundServer {
    fn start() -> Result<(Self, Receiver<BridgeSnapshot>)> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (snapshot_tx, snapshots) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("openmouse-bridge-runtime".into())
            .spawn(move || {
                let outcome = run_server(shutdown_rx, ready_tx.clone(), snapshot_tx);
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
    let (server, snapshots) = BackgroundServer::start()?;
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
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not create the Bridge async runtime")?;
    runtime.block_on(async move {
        let (bridge_config, path) = config::load_or_create()?;
        let origins = bridge_config.allowed_origins.clone();
        let service = BridgeService::new(bridge_config, path.clone());
        service.start_game_monitor(Arc::new(AtomicBool::new(true)));

        let snapshot_service = service.clone();
        let snapshot_publisher = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let _ = snapshots.try_send(snapshot_service.snapshot().await);
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
        outcome?;
        Ok(())
    })
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
