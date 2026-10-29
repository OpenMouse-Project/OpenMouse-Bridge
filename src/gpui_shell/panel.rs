//! The tray panel's views: Home, Settings and the Bridge info page.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _,
    button::{Button, ButtonVariants as _},
    switch::Switch,
};
use gpui_kit::*;
use openmouse_bridge::{
    BRIDGE_PORT, config, platform,
    service::{BridgeSnapshot, KnownDeviceStatus, Notice},
    updater::UpdateInfo,
};
use tokio::sync::mpsc as tokio_mpsc;

use super::ascii;
use crate::desktop::{
    BridgeCommand, UpdateEvent, WINDOW_HEIGHT, WINDOW_WIDTH, notify_natively, open_logs,
    open_openmouse, profile_label, service_label,
};
use crate::overlay::Overlay;

/// How many remembered mice Home lists before summarising the rest.
const HOME_DEVICE_ROWS: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    Settings,
    Info,
}

impl View {
    /// The panel's base height in logical points; Home matches the egui panel.
    fn base_height(self) -> f32 {
        match self {
            View::Home => WINDOW_HEIGHT,
            View::Settings => 436.0,
            View::Info => 380.0,
        }
    }
}

/// Extra height for one more line of content, in logical points.
const ROW_HEIGHT: f32 = 24.0;
const NOTE_HEIGHT: f32 = 22.0;

pub struct PanelView {
    snapshots: mpsc::Receiver<BridgeSnapshot>,
    commands: tokio_mpsc::UnboundedSender<BridgeCommand>,
    updates: mpsc::Receiver<UpdateEvent>,
    notices: mpsc::Receiver<Notice>,
    panel_requests: mpsc::Receiver<()>,
    restart: Arc<AtomicBool>,
    snapshot: Option<BridgeSnapshot>,
    view: View,
    autostart: bool,
    automatic_updates: bool,
    update_text: Option<String>,
    update_available: Option<UpdateInfo>,
    update_busy: bool,
    error: Option<String>,
    field: ascii::Field,
    was_visible: bool,
    /// The on-screen banner for low-battery and game-profile notices. Without
    /// it (no display for the native window), notices fall back to system
    /// notifications.
    overlay: Option<Overlay>,
}

pub struct Drained {
    pub changed: bool,
    pub panel_requested: bool,
    /// How soon the overlay needs the next tick while a banner is fading.
    pub overlay_delay: Option<Duration>,
}

impl PanelView {
    pub fn new(
        snapshots: mpsc::Receiver<BridgeSnapshot>,
        commands: tokio_mpsc::UnboundedSender<BridgeCommand>,
        updates: mpsc::Receiver<UpdateEvent>,
        notices: mpsc::Receiver<Notice>,
        panel_requests: mpsc::Receiver<()>,
        restart: Arc<AtomicBool>,
    ) -> Self {
        Self {
            snapshots,
            commands,
            updates,
            notices,
            panel_requests,
            restart,
            snapshot: None,
            view: View::Home,
            autostart: platform::autostart_enabled(),
            automatic_updates: false,
            update_text: None,
            update_available: None,
            update_busy: false,
            error: None,
            field: ascii::Field::default(),
            was_visible: true,
            overlay: Overlay::new()
                .inspect_err(|error| tracing::warn!(%error, "Could not create the overlay"))
                .ok(),
        }
    }

    /// Takes everything the core has queued since the last poll. Notices become
    /// system notifications for now; the GPUI overlay replaces that later.
    pub fn drain(&mut self) -> Drained {
        let mut changed = false;
        while let Ok(snapshot) = self.snapshots.try_recv() {
            self.autostart = snapshot.autostart_enabled;
            self.automatic_updates = snapshot.automatic_updates;
            self.snapshot = Some(snapshot);
            changed = true;
        }
        while let Ok(event) = self.updates.try_recv() {
            self.update_busy = false;
            self.update_text = Some(match event {
                UpdateEvent::Checking => {
                    self.update_busy = true;
                    "Checking for updates…".into()
                }
                UpdateEvent::UpToDate => {
                    self.update_available = None;
                    "Bridge is up to date".into()
                }
                UpdateEvent::Available(update) => {
                    let text = format!("Version {} is available", update.version);
                    self.update_available = Some(update);
                    text
                }
                UpdateEvent::Downloading(version) => {
                    self.update_busy = true;
                    format!("Downloading {version}…")
                }
                UpdateEvent::Restarting(version) => {
                    self.update_available = None;
                    self.update_busy = true;
                    format!("Restarting into {version}…")
                }
                UpdateEvent::Failed(error) => format!("Update failed: {error}"),
            });
            changed = true;
        }
        while let Ok(notice) = self.notices.try_recv() {
            self.present(&notice);
        }
        let mut panel_requested = false;
        while self.panel_requests.try_recv().is_ok() {
            panel_requested = true;
        }
        // Nothing repaints a panel that is hidden in the tray; the state above
        // keeps up regardless, and one repaint on showing it catches up.
        let visible = super::tray::is_visible();
        let became_visible = visible && !self.was_visible;
        self.was_visible = visible;
        Drained {
            changed: (changed && visible) || became_visible,
            panel_requested,
            overlay_delay: self.overlay.as_mut().and_then(Overlay::tick),
        }
    }

    /// Shows `notice` on the overlay with its chime, or as a system
    /// notification when there is no overlay. Low-battery alerts and the
    /// profile switch for a detected game both arrive here.
    fn present(&mut self, notice: &Notice) {
        let (sound, volume) = self.snapshot.as_ref().map_or((true, 60), |snapshot| {
            (snapshot.notification_sound, snapshot.notification_volume)
        });
        let Some(overlay) = self.overlay.as_mut() else {
            notify_natively(notice);
            return;
        };
        if let Err(error) = overlay.show(&notice.title, &notice.detail) {
            tracing::warn!(%error, "Could not show the overlay");
            notify_natively(notice);
            return;
        }
        tracing::info!(title = %notice.title, sound, volume, "Showed a notice on the overlay");
        if sound && let Err(error) = overlay.chime(volume) {
            tracing::warn!(%error, "Could not play the notification sound");
        }
    }

    fn show_view(&mut self, view: View, cx: &mut Context<Self>) {
        self.view = view;
        self.error = None;
        cx.notify();
    }

    /// The height the current view needs: the base height plus a line for each
    /// extra remembered mouse, the "more" note, and the update status text.
    fn desired_height(&self) -> f32 {
        let mut height = self.view.base_height();
        match self.view {
            View::Home => {
                let known = self
                    .snapshot
                    .as_ref()
                    .map_or(0, |snapshot| snapshot.known_devices.len());
                height += ROW_HEIGHT * known.min(HOME_DEVICE_ROWS).saturating_sub(1) as f32;
                if known > HOME_DEVICE_ROWS {
                    height += NOTE_HEIGHT;
                }
                if self.update_text.is_some() {
                    height += NOTE_HEIGHT;
                }
            }
            View::Settings => {
                if self.update_available.is_some() {
                    height += 40.0;
                }
                if self.error.is_some() {
                    height += NOTE_HEIGHT;
                }
            }
            View::Info => {}
        }
        height
    }

    fn set_autostart(&mut self, enabled: bool, cx: &mut Context<Self>) {
        match platform::set_autostart(enabled) {
            Ok(()) => {
                self.autostart = enabled;
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn set_automatic_updates(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.automatic_updates = enabled;
        let _ = self
            .commands
            .send(BridgeCommand::SetAutomaticUpdates(enabled));
        cx.notify();
    }

    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.update_busy = true;
        self.update_text = Some("Checking for updates…".into());
        let _ = self.commands.send(BridgeCommand::CheckForUpdates);
        cx.notify();
    }

    /// Sends a notice down the same path a low-battery alert or a detected
    /// game's profile switch takes, so it shows the overlay and plays the chime.
    fn test_notification(&mut self, cx: &mut Context<Self>) {
        self.present(&Notice {
            title: "Test notification".into(),
            detail: "This is how OpenMouse Bridge alerts look.".into(),
        });
        cx.notify();
    }

    fn open_logs(&mut self, cx: &mut Context<Self>) {
        self.error = open_logs().err().map(|error| error.to_string());
        cx.notify();
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

fn card(line: Hsla, fill: Hsla) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(line)
        .bg(fill)
}

fn format_age(seconds: u64) -> String {
    match seconds {
        0..=59 => "just now".into(),
        60..=3_599 => format!("{} min ago", seconds / 60),
        3_600..=86_399 => format!("{} h ago", seconds / 3_600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

fn format_uptime(seconds: u64) -> String {
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
    );
    if days > 0 {
        format!("{days} d {hours} h")
    } else if hours > 0 {
        format!("{hours} h {minutes} min")
    } else {
        format!("{minutes} min")
    }
}

/// What Home shows for a remembered mouse: its live reading while connected,
/// otherwise when it was last seen and the reading it had then.
fn known_device_text(device: &KnownDeviceStatus) -> String {
    let mut parts = Vec::new();
    if device.connected {
        match device.percent {
            Some(percent) if device.charging == Some(true) => {
                parts.push(format!("{percent}% · Charging"));
            }
            Some(percent) => parts.push(format!("{percent}%")),
            None => parts.push("Connected".into()),
        }
        if let Some(dpi) = device.dpi {
            parts.push(format!("{dpi} DPI"));
        }
    } else {
        parts.push(format!(
            "Last seen {}",
            format_age(device.last_seen_seconds_ago)
        ));
        if let Some(percent) = device.percent {
            parts.push(format!("{percent}%"));
        }
    }
    parts.join(" · ")
}

impl PanelView {
    fn back_header(
        &self,
        title: &'static str,
        accent: Hsla,
        muted: Hsla,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                Button::new("back")
                    .ghost()
                    .compact()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_view(View::Home, cx);
                    })),
            )
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(div().flex_1())
            .child(div().size(px(8.)).flex_none().rounded_full().bg(accent))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
            )
    }

    fn home(
        &self,
        muted: Hsla,
        accent: Hsla,
        line: Hsla,
        fill: Hsla,
        cx: &mut Context<Self>,
    ) -> Div {
        let snapshot = self.snapshot.as_ref();
        let mut details = card(line, fill)
            .child(row(muted, "Service", service_label(snapshot)))
            .child(row(muted, "Profile", profile_label(snapshot)));
        let known = snapshot
            .map(|snapshot| snapshot.known_devices.as_slice())
            .unwrap_or(&[]);
        if known.is_empty() {
            details = details.child(row(muted, "Mouse", "No mouse detected yet"));
        }
        for device in known.iter().take(HOME_DEVICE_ROWS) {
            details = details.child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap_3()
                    .text_sm()
                    .child(
                        div()
                            .truncate()
                            .text_color(if device.connected { accent } else { muted })
                            .child(device.name.clone()),
                    )
                    .child(div().flex_none().child(known_device_text(device))),
            );
        }
        if known.len() > HOME_DEVICE_ROWS {
            details = details.child(div().text_xs().text_color(muted).child(format!(
                "+{} more remembered",
                known.len() - HOME_DEVICE_ROWS
            )));
        }

        let restart = Arc::clone(&self.restart);
        let mut content = div()
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(8.)).flex_none().rounded_full().bg(accent))
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("OpenMouse Bridge"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
                            )
                            .child(
                                Button::new("settings")
                                    .ghost()
                                    .compact()
                                    .icon(IconName::Settings)
                                    .tooltip("Settings")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_view(View::Settings, cx);
                                    })),
                            ),
                    ),
            )
            .child(details);
        if let Some(text) = &self.update_text {
            content = content.child(div().text_xs().text_color(muted).child(text.clone()));
        }
        content.child(div().flex_1()).child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    Button::new("open-openmouse")
                        .primary()
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
                        .ghost()
                        .compact()
                        .label("Quit")
                        .on_click(cx.listener(|_, _, _, cx| cx.quit())),
                ),
        )
    }

    fn settings(
        &self,
        muted: Hsla,
        accent: Hsla,
        line: Hsla,
        fill: Hsla,
        danger: Hsla,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut content = div()
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_3()
            .child(self.back_header("Settings", accent, muted, cx))
            .child(
                card(line, fill).child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .items_center()
                        .text_sm()
                        .child("Run on start")
                        .child(Switch::new("autostart").checked(self.autostart).on_change(
                            cx.listener(|this, enabled: &bool, _, cx| {
                                this.set_autostart(*enabled, cx);
                            }),
                        )),
                ),
            );

        let mut updates = card(line, fill)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .text_sm()
                    .child("Update automatically")
                    .child(
                        Switch::new("automatic-updates")
                            .checked(self.automatic_updates)
                            .on_change(cx.listener(|this, enabled: &bool, _, cx| {
                                this.set_automatic_updates(*enabled, cx);
                            })),
                    ),
            )
            .child(
                div().flex().flex_row().items_center().gap_2().child(
                    Button::new("check-updates")
                        .compact()
                        .label("Check for updates")
                        .loading(self.update_busy)
                        .disabled(self.update_busy)
                        .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
                ),
            );
        if let Some(update) = self.update_available.clone() {
            updates = updates.child(
                Button::new("install-update")
                    .primary()
                    .compact()
                    .label(format!("Install {}", update.version))
                    .disabled(self.update_busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_busy = true;
                        let _ = this
                            .commands
                            .send(BridgeCommand::InstallUpdate(update.clone()));
                        cx.notify();
                    })),
            );
        }
        if let Some(text) = &self.update_text {
            updates = updates.child(div().text_xs().text_color(muted).child(text.clone()));
        }
        content = content.child(updates).child(
            card(line, fill)
                .child(
                    Button::new("test-notification")
                        .ghost()
                        .compact()
                        .icon(IconName::Bell)
                        .label("Test notification")
                        .on_click(cx.listener(|this, _, _, cx| this.test_notification(cx))),
                )
                .child(
                    Button::new("open-logs")
                        .ghost()
                        .compact()
                        .icon(IconName::FileText)
                        .label("Open logs")
                        .on_click(cx.listener(|this, _, _, cx| this.open_logs(cx))),
                )
                .child(
                    Button::new("info")
                        .ghost()
                        .compact()
                        .icon(IconName::Info)
                        .label("OpenMouse Bridge info")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_view(View::Info, cx);
                        })),
                ),
        );
        if let Some(error) = &self.error {
            content = content.child(div().text_xs().text_color(danger).child(error.clone()));
        }
        content
    }

    fn info(
        &self,
        muted: Hsla,
        accent: Hsla,
        line: Hsla,
        fill: Hsla,
        cx: &mut Context<Self>,
    ) -> Div {
        let snapshot = self.snapshot.as_ref();
        let platform_text = match snapshot.and_then(|snapshot| snapshot.linux_distribution.clone())
        {
            Some(distribution) => format!("{} · {distribution}", platform::platform_name()),
            None => platform::platform_name().to_owned(),
        };
        let path_text = |path: anyhow::Result<std::path::PathBuf>| {
            path.map(|path| path.display().to_string())
                .unwrap_or_else(|_| "unavailable".into())
        };
        let mut details = card(line, fill)
            .child(row(muted, "Version", env!("CARGO_PKG_VERSION")))
            .child(row(muted, "Platform", platform_text))
            .child(row(muted, "Address", format!("127.0.0.1:{BRIDGE_PORT}")));
        if let Some(snapshot) = snapshot {
            details = details
                .child(row(
                    muted,
                    "Running for",
                    format_uptime(snapshot.uptime_seconds),
                ))
                .child(row(
                    muted,
                    "Control panel",
                    if snapshot.client_connected {
                        "Connected"
                    } else {
                        "Not connected"
                    },
                ))
                .child(row(muted, "Device mode", snapshot.device_mode.clone()))
                .child(row(muted, "Profiles", snapshot.profile_count.to_string()))
                .child(row(
                    muted,
                    "Remembered mice",
                    snapshot.known_devices.len().to_string(),
                ));
        }
        details = details
            .child(row(muted, "Config", path_text(config::config_path())))
            .child(row(muted, "Logs", path_text(config::log_dir())));
        div()
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_3()
            .child(self.back_header("Bridge info", accent, muted, cx))
            .child(details)
    }
}

impl Render for PanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, accent, foreground, danger) = (
            theme.muted_foreground,
            theme.primary,
            theme.foreground,
            theme.danger,
        );
        let line = Hsla {
            a: 0.7,
            ..theme.border
        };
        let fill = Hsla {
            a: 0.35,
            ..theme.sidebar
        };
        // Keep the window as tall as its content so nothing is clipped.
        let desired = self.desired_height();
        if (window.viewport_size().height.as_f32() - desired).abs() > 1.0 {
            window.resize(size(px(WINDOW_WIDTH), px(desired)));
        }
        let field = self.field.element(window, cx);
        let content = match self.view {
            View::Home => self.home(muted, accent, line, fill, cx),
            View::Settings => self.settings(muted, accent, line, fill, danger, cx),
            View::Info => self.info(muted, accent, line, fill, cx),
        };
        // The ASCII field sits behind the content; the window's own Root paints
        // the background, so this shell stays clear.
        div()
            .relative()
            .size_full()
            .text_color(foreground)
            .child(field)
            .child(content)
    }
}
