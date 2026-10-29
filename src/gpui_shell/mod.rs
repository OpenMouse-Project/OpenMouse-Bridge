//! The window: a GPUI tray panel.
//!
//! `BackgroundServer` owns the service, its HTTP API and HID on its own thread
//! and hands this module snapshot, update, notice and panel-request channels,
//! which are polled on a timer. See docs/gpui-migration.md.

mod ascii;
mod panel;
mod theme;
mod tray;

use std::{
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::Result;
use gpui_kit::*;
use panel::{CoreLinks, PanelView};

use crate::runtime::{
    BackgroundServer, RELAUNCH_DELAY, WINDOW_HEIGHT, WINDOW_WIDTH, current_relaunch_generation,
    next_relaunch_generation, panic_message, restart_bridge, resume_gap,
};

/// How often the channels from the core are drained; a tray click waits at
/// most this long.
const POLL: Duration = Duration::from_millis(200);

pub fn run() -> Result<()> {
    tracing::info!("starting the GPUI shell");
    let (notice_tx, notices) = mpsc::channel();
    let Some((server, snapshots, commands, updates, panel_requests, show_panel)) =
        BackgroundServer::start(notice_tx)?
    else {
        return Ok(());
    };
    let exit = ExitFlags::default();
    let app_exit = exit.clone();
    let gui_started = Instant::now();

    // A panic in the window must not take the whole app down for good: it is
    // caught here so the relaunch below can run.
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        application().with_assets(assets::AllAssets).run(move |cx| {
            init(cx);
            theme::apply(cx);
            crate::runtime::configure_tray_only_application();

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
            let view_exit = app_exit.clone();
            let (handle, view) = open_window(options, cx, move |_, cx| {
                cx.new(|cx| {
                    PanelView::new(
                        CoreLinks {
                            snapshots,
                            commands,
                            updates,
                            notices,
                            panel_requests,
                        },
                        view_exit,
                        show_panel,
                        cx.focus_handle(),
                    )
                })
            })
            .expect("failed to open the Bridge panel");

            // The panel opens at launch on a first run, and when asked to by the
            // environment.
            let show_panel =
                show_panel || std::env::var_os("OPENMOUSE_BRIDGE_SHOW_WINDOW").is_some();
            let (tray_commands, mut tray_handle) = match tray::install(cx, handle) {
                Some((commands, tray)) => (Some(commands), Some(tray)),
                None => (None, None),
            };
            if !show_panel && tray_commands.is_some() {
                let _ = cx.update_window(handle, |_, window, _| tray::hide(window));
            } else {
                // Shown at launch (the first run): beside the tray, not mid-screen.
                if tray_commands.is_some() {
                    let _ = cx.update_window(handle, |_, window, _| tray::place(window, None));
                }
                cx.activate(true);
            }

            let view = view.downgrade();
            cx.spawn(async move |cx| {
                let mut wait = POLL;
                let mut last_anchor: Option<tray::Anchor> = None;
                let mut last_tick = SystemTime::now();
                loop {
                    cx.background_executor().timer(wait).await;
                    // This loop runs several times a second, so a long
                    // wall-clock gap means the PC slept or hibernated and the
                    // tray icon may be gone.
                    let now = SystemTime::now();
                    if let Some(gap) = resume_gap(std::mem::replace(&mut last_tick, now), now) {
                        tracing::info!(?gap, "Resumed from sleep; recreating the tray icon");
                        if let Some(tray) = tray_handle.as_mut() {
                            tray.recreate();
                        }
                    }
                    let tray_requests: Vec<tray::Command> = tray_commands
                        .as_ref()
                        .map(|commands| commands.try_iter().collect())
                        .unwrap_or_default();
                    let Ok((drained, tray_battery)) =
                        view.update(cx, |view: &mut PanelView, cx| {
                            let drained = view.drain();
                            if drained.changed {
                                cx.notify();
                            }
                            (drained, view.tray_battery())
                        })
                    else {
                        break;
                    };
                    let (panel_wanted, overlay_delay) =
                        (drained.panel_requested, drained.overlay_delay);
                    if let Some(tray) = tray_handle.as_mut() {
                        tray.show_battery(tray_battery);
                    }
                    if drained.quit_for_update {
                        // The updater has staged the new version and started its
                        // helper, which waits for this process to exit, installs,
                        // and relaunches. Exiting without a restart of our own.
                        tracing::info!("exiting so the staged update can be installed");
                        cx.update(|cx| app_exit.quit(cx));
                        break;
                    }
                    // A fading banner needs a frame every ~16 ms; otherwise idle.
                    wait = overlay_delay.map_or(POLL, |delay| delay.min(POLL));
                    // The newest click says where the icon is; a request with no
                    // click (the API, a second launch) reuses where it last was.
                    let clicked_at = tray_requests
                        .iter()
                        .rev()
                        .find_map(|command| match command {
                            tray::Command::Show(anchor) => Some(*anchor),
                            _ => None,
                        });
                    if let Some(Some(anchor)) = clicked_at {
                        last_anchor = Some(anchor);
                    }
                    let show = panel_wanted || clicked_at.is_some();
                    if show {
                        tracing::debug!(panel_wanted, "showing the panel");
                        let anchor = last_anchor;
                        let _ = cx.update_window(handle, |_, window, _| tray::show(window, anchor));
                    }
                    for command in tray_requests {
                        match command {
                            tray::Command::Show(_) => {}
                            tray::Command::Restart => {
                                cx.update(|cx| app_exit.restart(cx));
                            }
                            tray::Command::Quit => {
                                cx.update(|cx| app_exit.quit(cx));
                            }
                        }
                    }
                }
            })
            .detach();
        });
    }));

    let stopped = server.stop();
    if let Err(payload) = &outcome {
        tracing::error!(error = %panic_message(payload), "the GPUI shell panicked");
    }
    if exit.restart.load(Ordering::Relaxed) {
        restart_bridge(current_relaunch_generation())?;
        return stopped;
    }
    // The window ended and nobody asked it to (a quit, a restart or an update
    // all set `deliberate`): start Bridge again, unless it keeps dying at once.
    let deliberate = exit.deliberate.load(Ordering::Relaxed);
    if let Some(generation) = next_relaunch_generation(
        current_relaunch_generation(),
        gui_started.elapsed(),
        !deliberate,
    ) {
        if outcome.is_err() {
            thread::sleep(RELAUNCH_DELAY);
        }
        tracing::info!(
            generation,
            "relaunching Bridge after its window stopped unexpectedly"
        );
        restart_bridge(generation)?;
        return Ok(());
    }
    stopped
}

/// Why the app is ending. A window that ends without `deliberate` set ended
/// unexpectedly, and Bridge relaunches itself.
#[derive(Clone, Default)]
pub struct ExitFlags {
    pub restart: Arc<AtomicBool>,
    pub deliberate: Arc<AtomicBool>,
}

impl ExitFlags {
    /// Quits on purpose.
    pub fn quit(&self, cx: &mut App) {
        self.deliberate.store(true, Ordering::Relaxed);
        cx.quit();
    }

    /// Quits on purpose, and Bridge starts again once it has stopped.
    pub fn restart(&self, cx: &mut App) {
        self.restart.store(true, Ordering::Relaxed);
        self.quit(cx);
    }
}

/// An empty snapshot for tests of the shell's pieces.
#[cfg(test)]
pub(crate) fn test_snapshot() -> openmouse_bridge::service::BridgeSnapshot {
    openmouse_bridge::service::BridgeSnapshot {
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
        known_devices: vec![],
    }
}
