use std::{
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
use eframe::egui::{self, ViewportCommand};
use openmouse_bridge::{BRIDGE_PORT, api, config, platform, service::BridgeService};
#[cfg(target_os = "windows")]
use std::ptr::null_mut;
use tokio::{net::TcpListener, sync::oneshot};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem},
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

const OPENMOUSE_URL: &str = "https://openmouse.app";
const TRAY_AUTOSTART: &str = "openmouse.autostart";
const TRAY_OPEN: &str = "openmouse.open";
const TRAY_EXIT: &str = "openmouse.exit";

#[derive(Clone, Copy)]
enum TrayAction {
    ToggleAutostart,
    OpenOpenMouse,
    Exit,
}

struct TrayState {
    _icon: TrayIcon,
    autostart: CheckMenuItem,
    events: Receiver<TrayAction>,
}

impl TrayState {
    fn new(context: &egui::Context) -> Result<Self> {
        let autostart = CheckMenuItem::with_id(
            TRAY_AUTOSTART,
            "Run on Start",
            true,
            platform::autostart_enabled(),
            None,
        );
        let open = MenuItem::with_id(TRAY_OPEN, "Open OpenMouse", true, None);
        let exit = MenuItem::with_id(TRAY_EXIT, "Exit", true, None);
        let menu = Menu::with_items(&[&autostart, &open, &exit])
            .context("could not create the tray menu")?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("OpenMouse Bridge")
            .with_icon(tray_icon().context("could not create the tray icon image")?)
            .build()
            .context("could not create the system tray icon")?;

        let (event_tx, events) = mpsc::channel();
        let repaint = context.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = if event.id == TRAY_AUTOSTART {
                Some(TrayAction::ToggleAutostart)
            } else if event.id == TRAY_OPEN {
                Some(TrayAction::OpenOpenMouse)
            } else if event.id == TRAY_EXIT {
                Some(TrayAction::Exit)
            } else {
                None
            };
            if let Some(action) = action {
                let _ = event_tx.send(action);
                repaint.request_repaint();
            }
        }));

        Ok(Self {
            _icon: icon,
            autostart,
            events,
        })
    }

    fn process_events(&self, context: &egui::Context) {
        while let Ok(action) = self.events.try_recv() {
            match action {
                TrayAction::ToggleAutostart => {
                    let enabled = self.autostart.is_checked();
                    if let Err(error) = platform::set_autostart(enabled) {
                        self.autostart.set_checked(!enabled);
                        tracing::error!(%error, "Could not change OpenMouse Bridge startup");
                    }
                }
                TrayAction::OpenOpenMouse => {
                    if let Err(error) = open_openmouse() {
                        tracing::error!(%error, "Could not open OpenMouse from the tray");
                    }
                }
                TrayAction::Exit => context.send_viewport_cmd(ViewportCommand::Close),
            }
        }
    }
}

struct TrayApp {
    tray: TrayState,
}

impl TrayApp {
    fn new(context: &egui::Context) -> Result<Self> {
        configure_tray_only_application();
        Ok(Self {
            tray: TrayState::new(context)?,
        })
    }
}

impl eframe::App for TrayApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        context.send_viewport_cmd(ViewportCommand::Visible(false));
        self.tray.process_events(context);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().send_viewport_cmd(ViewportCommand::Visible(false));
        self.tray.process_events(ui.ctx());
    }
}

struct BackgroundServer {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl BackgroundServer {
    fn start() -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("openmouse-bridge-runtime".into())
            .spawn(move || {
                let outcome = run_server(shutdown_rx, ready_tx.clone());
                if let Err(error) = &outcome {
                    let _ = ready_tx.send(Err(format!("{error:#}")));
                }
                outcome
            })
            .context("could not start the Bridge runtime")?;

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self {
                shutdown: Some(shutdown_tx),
                thread: Some(thread),
            }),
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
    let server = BackgroundServer::start()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1.0, 1.0])
            .with_decorations(false)
            .with_visible(false),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let tray_result = eframe::run_native(
        "OpenMouse Bridge",
        options,
        Box::new(|context| Ok(Box::new(TrayApp::new(&context.egui_ctx)?))),
    )
    .map_err(|error| anyhow!(error.to_string()));
    let server_result = server.stop();
    tray_result.and(server_result)
}

fn run_server(
    shutdown: oneshot::Receiver<()>,
    ready: SyncSender<Result<(), String>>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not create the Bridge async runtime")?;
    runtime.block_on(async move {
        let (bridge_config, path) = config::load_or_create()?;
        let origins = bridge_config.allowed_origins.clone();
        let service = BridgeService::new(bridge_config, path.clone());
        // The web client still requests application icons. There is no status
        // window anymore, so keep extraction enabled independently of UI state.
        service.start_game_monitor(Arc::new(AtomicBool::new(true)));

        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), BRIDGE_PORT);
        let listener = TcpListener::bind(address).await.with_context(|| {
            format!("could not bind http://{address}; is Bridge already running?")
        })?;
        tracing::info!(%address, config = %path.display(), "OpenMouse Bridge is ready");
        let _ = ready.send(Ok(()));
        axum::serve(listener, api::router(service, &origins))
            .with_graceful_shutdown(async {
                let _ = shutdown.await;
            })
            .await?;
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
