//! The GPUI tray shell (work in progress, `--features gpui`).
//!
//! Runs the same core as the egui shell: `BackgroundServer` owns the service,
//! HTTP API and HID on its own thread and hands this module snapshot, update,
//! notice and panel-request channels, which are polled on a timer. Select it
//! with `OPENMOUSE_BRIDGE_SHELL=gpui`. See docs/gpui-migration.md.

mod tray;

use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::Result;
use gpui_kit::component::{ActiveTheme as _, button::Button};
use gpui_kit::*;
use openmouse_bridge::service::{BridgeSnapshot, Notice};
use tokio::sync::mpsc as tokio_mpsc;

use crate::desktop::{
    BackgroundServer, BridgeCommand, UpdateEvent, WINDOW_HEIGHT, WINDOW_WIDTH, battery_text,
    current_relaunch_generation, device_label, notify_natively, open_openmouse, profile_label,
    restart_bridge, service_label,
};

/// How often the channels from the core are drained.
const POLL: Duration = Duration::from_millis(100);

pub fn run() -> Result<()> {
    let (notice_tx, notices) = mpsc::channel();
    // The egui shell wakes its repaint loop through this; GPUI polls instead.
    let waker = Arc::new(OnceLock::new());
    let Some((server, snapshots, commands, updates, panel_requests, show_panel)) =
        BackgroundServer::start((notice_tx, waker))?
    else {
        return Ok(());
    };
    let restart = Arc::new(AtomicBool::new(false));
    let app_restart = Arc::clone(&restart);

    application().with_assets(assets::AllAssets).run(move |cx| {
        init(cx);
        crate::desktop::configure_tray_only_application();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
                cx,
            ))),
            titlebar: None,
            kind: WindowKind::PopUp,
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        };
        let view_restart = Arc::clone(&app_restart);
        let (handle, view) = open_window(options, cx, move |_, cx| {
            cx.new(|_| PanelView {
                snapshots,
                commands,
                updates,
                notices,
                panel_requests,
                restart: view_restart,
                snapshot: None,
                update_text: None,
            })
        })
        .expect("failed to open the Bridge panel");

        let tray_commands = tray::install(cx, handle);
        if !show_panel && tray_commands.is_some() {
            let _ = cx.update_window(handle, |_, window, _| tray::hide(window));
        } else {
            cx.activate(true);
        }

        let view = view.downgrade();
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let tray_requests: Vec<tray::Command> = tray_commands
                    .as_ref()
                    .map(|commands| commands.try_iter().collect())
                    .unwrap_or_default();
                let Ok(panel_wanted) = view.update(cx, |view: &mut PanelView, cx| {
                    let drained = view.drain();
                    if drained.changed {
                        cx.notify();
                    }
                    drained.panel_requested
                }) else {
                    break;
                };
                let show = panel_wanted
                    || tray_requests
                        .iter()
                        .any(|command| matches!(command, tray::Command::Show));
                if show {
                    let _ = cx.update_window(handle, |_, window, _| tray::show(window));
                }
                for command in tray_requests {
                    match command {
                        tray::Command::Show => {}
                        tray::Command::Restart => {
                            app_restart.store(true, Ordering::Relaxed);
                            cx.update(|cx| cx.quit());
                        }
                        tray::Command::Quit => {
                            cx.update(|cx| cx.quit());
                        }
                    }
                }
            }
        })
        .detach();
    });

    let stopped = server.stop();
    if restart.load(Ordering::Relaxed) {
        restart_bridge(current_relaunch_generation())?;
    }
    stopped
}

struct PanelView {
    snapshots: mpsc::Receiver<BridgeSnapshot>,
    #[allow(dead_code)] // Settings and update controls send these once ported.
    commands: tokio_mpsc::UnboundedSender<BridgeCommand>,
    updates: mpsc::Receiver<UpdateEvent>,
    notices: mpsc::Receiver<Notice>,
    panel_requests: mpsc::Receiver<()>,
    restart: Arc<AtomicBool>,
    snapshot: Option<BridgeSnapshot>,
    update_text: Option<String>,
}

struct Drained {
    changed: bool,
    panel_requested: bool,
}

impl PanelView {
    /// Takes everything the core has queued since the last poll. Notices become
    /// system notifications for now; the GPUI overlay replaces that later.
    fn drain(&mut self) -> Drained {
        let mut changed = false;
        while let Ok(snapshot) = self.snapshots.try_recv() {
            self.snapshot = Some(snapshot);
            changed = true;
        }
        while let Ok(event) = self.updates.try_recv() {
            self.update_text = match event {
                UpdateEvent::Checking => Some("Checking for updates…".into()),
                UpdateEvent::UpToDate => Some("Bridge is up to date".into()),
                UpdateEvent::Available(update) => {
                    Some(format!("Version {} is available", update.version))
                }
                UpdateEvent::Downloading(version) => Some(format!("Downloading {version}…")),
                UpdateEvent::Restarting(version) => Some(format!("Restarting into {version}…")),
                UpdateEvent::Failed(error) => Some(format!("Update failed: {error}")),
            };
            changed = true;
        }
        while let Ok(notice) = self.notices.try_recv() {
            notify_natively(&notice);
        }
        let mut panel_requested = false;
        while self.panel_requests.try_recv().is_ok() {
            panel_requested = true;
        }
        Drained {
            changed,
            panel_requested,
        }
    }
}

fn row(muted: Hsla, label: &'static str, value: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .gap_3()
        .text_sm()
        .child(div().text_color(muted).child(label))
        .child(div().truncate().child(value.into()))
}

impl Render for PanelView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let snapshot = self.snapshot.as_ref();
        let restart = Arc::clone(&self.restart);

        let mut panel = div()
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_2()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(div().text_base().child("OpenMouse Bridge"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
                    ),
            )
            .child(row(muted, "Service", service_label(snapshot)))
            .child(row(muted, "Profile", profile_label(snapshot)))
            .child(row(muted, "Mouse", device_label(snapshot)));

        if let Some(snapshot) = snapshot {
            for battery in &snapshot.batteries {
                panel = panel.child(div().text_sm().child(format!(
                    "{}  {}",
                    battery.device_name,
                    battery_text(battery)
                )));
            }
        }
        if let Some(text) = &self.update_text {
            panel = panel.child(div().text_xs().text_color(muted).child(text.clone()));
        }

        panel.child(div().flex_1()).child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    Button::new("open-openmouse")
                        .compact()
                        .label("Open OpenMouse")
                        .on_click(cx.listener(|_, _, _, _| {
                            let _ = open_openmouse();
                        })),
                )
                .child(
                    Button::new("restart")
                        .compact()
                        .label("Restart")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            restart.store(true, Ordering::Relaxed);
                            cx.quit();
                        })),
                )
                .child(
                    Button::new("quit")
                        .compact()
                        .label("Quit")
                        .on_click(cx.listener(|_, _, _, cx| cx.quit())),
                ),
        )
    }
}
