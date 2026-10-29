//! The GPUI tray shell (work in progress, `--features gpui`).
//!
//! Runs the same core as the egui shell: `BackgroundServer` owns the service,
//! HTTP API and HID on its own thread and hands this module snapshot, update,
//! notice and panel-request channels, which are polled on a timer. It is the
//! shell of a `--features gpui` build; `OPENMOUSE_BRIDGE_SHELL=egui` runs the
//! egui one instead. See docs/gpui-migration.md.

mod ascii;
mod panel;
mod theme;
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
use gpui_kit::*;
use panel::{CoreLinks, PanelView};

use crate::desktop::{
    BackgroundServer, WINDOW_HEIGHT, WINDOW_WIDTH, current_relaunch_generation, restart_bridge,
};

/// How often the channels from the core are drained; a tray click waits at
/// most this long.
const POLL: Duration = Duration::from_millis(200);

pub fn run() -> Result<()> {
    tracing::info!("starting the GPUI shell");
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
        theme::apply(cx);
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
            cx.new(|cx| {
                PanelView::new(
                    CoreLinks {
                        snapshots,
                        commands,
                        updates,
                        notices,
                        panel_requests,
                    },
                    view_restart,
                    show_panel,
                    cx.focus_handle(),
                )
            })
        })
        .expect("failed to open the Bridge panel");

        // The panel opens at launch on a first run, and when asked to by the
        // environment, as in the egui shell.
        let show_panel = show_panel || std::env::var_os("OPENMOUSE_BRIDGE_SHOW_WINDOW").is_some();
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
            loop {
                cx.background_executor().timer(wait).await;
                let tray_requests: Vec<tray::Command> = tray_commands
                    .as_ref()
                    .map(|commands| commands.try_iter().collect())
                    .unwrap_or_default();
                let Ok((drained, tray_battery)) = view.update(cx, |view: &mut PanelView, cx| {
                    let drained = view.drain();
                    if drained.changed {
                        cx.notify();
                    }
                    (drained, view.tray_battery())
                }) else {
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
                    cx.update(|cx| cx.quit());
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
