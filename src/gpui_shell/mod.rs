//! The GPUI tray shell (work in progress, `--features gpui`).
//!
//! Runs the same core as the egui shell: `BackgroundServer` owns the service,
//! HTTP API and HID on its own thread and hands this module snapshot, update,
//! notice and panel-request channels, which are polled on a timer. Select it
//! with `OPENMOUSE_BRIDGE_SHELL=gpui`. See docs/gpui-migration.md.

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
use panel::PanelView;

use crate::desktop::{
    BackgroundServer, WINDOW_HEIGHT, WINDOW_WIDTH, current_relaunch_generation, restart_bridge,
};

/// How often the channels from the core are drained; a tray click waits at
/// most this long.
const POLL: Duration = Duration::from_millis(200);

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
            cx.new(|_| {
                PanelView::new(
                    snapshots,
                    commands,
                    updates,
                    notices,
                    panel_requests,
                    view_restart,
                )
            })
        })
        .expect("failed to open the Bridge panel");

        let tray_commands = tray::install(cx, handle);
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
                let Ok((panel_wanted, overlay_delay)) =
                    view.update(cx, |view: &mut PanelView, cx| {
                        let drained = view.drain();
                        if drained.changed {
                            cx.notify();
                        }
                        (drained.panel_requested, drained.overlay_delay)
                    })
                else {
                    break;
                };
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
