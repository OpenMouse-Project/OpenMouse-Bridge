//! The tray icon for the GPUI shell: closing the panel hides it, a click on the
//! icon brings it back, and the menu offers Open / Restart / Quit.
//!
//! Linux uses StatusNotifierItem over D-Bus, without a GTK event loop.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[cfg(any(target_os = "macos", windows))]
use gpui_kit::AppContext as _;
use gpui_kit::{AnyWindowHandle, App, Window};

#[cfg(any(target_os = "macos", windows))]
use crate::tray_battery;
use crate::tray_battery::TrayBattery;

/// Where the tray icon was when it was clicked, so the panel opens beside it:
/// the cursor in logical points with a bottom-left origin on macOS (the click
/// is on the icon), the icon's rectangle in physical pixels with a top-left
/// origin on Windows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

/// The tray icon, kept by the poll loop so it can show the battery on it.
pub struct TrayHandle {
    #[cfg(target_os = "linux")]
    icon: linux::Handle,
    #[cfg(any(target_os = "macos", windows))]
    icon: tray_icon::TrayIcon,
    shown: Option<TrayBattery>,
}

impl TrayHandle {
    /// Shows `battery` on the icon, or the plain logo for `None`, when that
    /// differs from what is showing; calling this every poll is cheap.
    pub fn show_battery(&mut self, battery: Option<TrayBattery>) {
        #[cfg(target_os = "linux")]
        self.icon.check_health();
        if battery == self.shown {
            return;
        }
        #[cfg(target_os = "linux")]
        if let Err(error) = self.icon.show_battery(battery.as_ref()) {
            tracing::warn!(%error, "Could not update the tray icon");
            return;
        }
        #[cfg(any(target_os = "macos", windows))]
        {
            let outcome = match &battery {
                Some(battery) => tray_battery::icon(battery).and_then(|icon| {
                    self.icon.set_icon(Some(icon))?;
                    self.icon
                        .set_tooltip(Some(tray_battery::tooltip(battery)))?;
                    Ok(())
                }),
                None => crate::runtime::tray_icon().and_then(|icon| {
                    self.icon.set_icon(Some(icon))?;
                    self.icon.set_tooltip(Some("OpenMouse Bridge"))?;
                    Ok(())
                }),
            };
            if let Err(error) = outcome {
                tracing::warn!(%error, "Could not update the tray icon");
                return;
            }
        }
        self.shown = battery;
    }

    /// Windows can drop the notification-area icon across sleep or hibernate
    /// while the process keeps running; this adds a fresh one. The battery is
    /// drawn on it again at the next poll.
    pub fn recreate(&mut self) {
        #[cfg(any(target_os = "macos", windows))]
        match build_icon() {
            Some(icon) => {
                self.icon = icon;
                self.shown = None;
            }
            None => tracing::warn!("Could not recreate the tray icon after resume"),
        }
    }
}

#[derive(Debug)]
pub enum Command {
    /// Show the panel, beside the icon when the icon was clicked.
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
    Show(Option<Anchor>),
    // Only the Windows tray has a menu to send these from.
    #[cfg_attr(not(windows), allow(dead_code))]
    Restart,
    #[cfg_attr(not(windows), allow(dead_code))]
    Quit,
}

#[cfg(target_os = "macos")]
mod platform {
    use super::Anchor;
    use gpui_kit::Window;
    use objc2::{MainThreadMarker, rc::Retained};
    use objc2_app_kit::{NSEvent, NSScreen, NSView, NSWindow};
    use objc2_foundation::{NSPoint, NSRect};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// Distance kept from the screen's edges and below the menu bar, in points.
    const MARGIN: f64 = 8.0;

    /// The panel's own `NSWindow`. Hiding only this window, not the whole app,
    /// keeps the overlay's window from un-hiding the panel when it appears.
    fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
        let RawWindowHandle::AppKit(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
        else {
            return None;
        };
        // SAFETY: the view belongs to the live window this handle came from.
        let view = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }?;
        view.window()
    }

    pub fn hide_window(window: &Window) {
        if let Some(window) = ns_window(window) {
            window.orderOut(None);
        }
    }

    pub fn show_window(window: &mut Window) -> bool {
        if let Some(window) = ns_window(window) {
            window.makeKeyAndOrderFront(None);
        }
        false
    }

    /// The anchor for a click on the icon: the cursor, which is on it. Read
    /// when the click arrives, not when the panel is shown a poll later.
    pub fn anchor_of_click() -> Anchor {
        let point = NSEvent::mouseLocation();
        Anchor {
            left: point.x,
            top: point.y,
            right: point.x,
            bottom: point.y,
        }
    }

    fn contains(frame: NSRect, point: NSPoint) -> bool {
        point.x >= frame.origin.x
            && point.x < frame.origin.x + frame.size.width
            && point.y >= frame.origin.y
            && point.y < frame.origin.y + frame.size.height
    }

    /// Puts the panel just below the menu bar, centred under the icon and kept
    /// on its screen. With no click to go by it goes to the menu bar's
    /// right-hand end of the main screen, where status icons live.
    pub fn place_near_tray(window: &Window, anchor: Option<Anchor>) {
        let (Some(panel), Some(marker)) = (ns_window(window), MainThreadMarker::new()) else {
            return;
        };
        let point = anchor.map(|anchor| NSPoint::new(anchor.left, anchor.top));
        let screen = point
            .and_then(|point| {
                NSScreen::screens(marker)
                    .iter()
                    .find(|screen| contains(screen.frame(), point))
            })
            .or_else(|| NSScreen::mainScreen(marker));
        let Some(screen) = screen else {
            return;
        };
        let visible = screen.visibleFrame();
        let width = panel.frame().size.width;
        let left = visible.origin.x + MARGIN;
        let right = (visible.origin.x + visible.size.width - width - MARGIN).max(left);
        let x = match point {
            Some(point) => point.x - width / 2.0,
            None => right,
        };
        // The top edge of the visible frame is the bottom of the menu bar.
        let top = visible.origin.y + visible.size.height - MARGIN;
        panel.setFrameTopLeftPoint(NSPoint::new(x.clamp(left, right), top));
    }
}

#[cfg(windows)]
mod platform {
    use super::Anchor;
    use gpui_kit::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::{
        Foundation::{POINT, RECT},
        Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
            MonitorFromPoint, MonitorFromRect,
        },
        UI::WindowsAndMessaging::{
            SW_HIDE, SW_RESTORE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow,
            SetWindowPos, ShowWindow,
        },
    };

    /// Distance kept from the work area's edges and from the icon, in pixels.
    const MARGIN: f64 = 8.0;

    fn hwnd_of(window: &Window) -> Option<windows_sys::Win32::Foundation::HWND> {
        match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => Some(handle.hwnd.get() as _),
            _ => None,
        }
    }

    /// Hides the window and its taskbar button.
    pub fn hide_window(window: &Window) {
        if let Some(hwnd) = hwnd_of(window) {
            unsafe {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    pub fn show_window(window: &mut Window) -> bool {
        if let Some(hwnd) = hwnd_of(window) {
            unsafe {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
        }
        false
    }

    pub fn anchor_of_icon(rect: &tray_icon::Rect) -> Anchor {
        Anchor {
            left: rect.position.x,
            top: rect.position.y,
            right: rect.position.x + f64::from(rect.size.width),
            bottom: rect.position.y + f64::from(rect.size.height),
        }
    }

    /// Puts the panel's right edge on the icon's, above it when the icon is in
    /// the lower half of its screen (a bottom taskbar) and below it otherwise,
    /// kept inside the work area. With no click to go by it goes to the primary
    /// screen's bottom-right corner.
    pub fn place_near_tray(window: &Window, anchor: Option<Anchor>) {
        let Some(hwnd) = hwnd_of(window) else {
            return;
        };
        let scale = f64::from(window.scale_factor());
        let size = window.bounds().size;
        let (width, height) = (
            f64::from(size.width.as_f32()) * scale,
            f64::from(size.height.as_f32()) * scale,
        );
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let monitor = unsafe {
            match anchor {
                Some(anchor) => {
                    let rect = RECT {
                        left: anchor.left as i32,
                        top: anchor.top as i32,
                        right: anchor.right as i32,
                        bottom: anchor.bottom as i32,
                    };
                    MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST)
                }
                None => MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY),
            }
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
            return;
        }
        let work = info.rcWork;
        let (work_left, work_top) = (f64::from(work.left), f64::from(work.top));
        let (work_right, work_bottom) = (f64::from(work.right), f64::from(work.bottom));
        let icon = anchor.unwrap_or(Anchor {
            left: work_right,
            top: work_bottom,
            right: work_right,
            bottom: work_bottom,
        });
        let x = icon.right - width;
        let y = if (icon.top + icon.bottom) / 2.0 > (work_top + work_bottom) / 2.0 {
            icon.top - height - MARGIN
        } else {
            icon.bottom + MARGIN
        };
        let clamp = |value: f64, low: f64, high: f64| value.clamp(low, high.max(low));
        let x = clamp(x, work_left + MARGIN, work_right - width - MARGIN);
        let y = clamp(y, work_top + MARGIN, work_bottom - height - MARGIN);
        unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x as i32,
                y as i32,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[path = "tray_linux.rs"]
mod linux;

#[cfg(target_os = "linux")]
mod platform {
    use super::Anchor;
    use gpui_kit::Window;
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

    // GPUI exposes minimization on both X11 and Wayland. Keeping the taskbar
    // entry also provides recovery if the desktop loses its tray host.
    pub fn hide_window(window: &mut Window) {
        if super::layer_panel() {
            // A layer surface cannot be minimized; showing opens a new one.
            window.remove_window();
        } else {
            window.minimize_window();
        }
    }
    /// Returns whether the panel was activated with the tray host's token, so
    /// the caller skips GPUI's own (rejected on Wayland) activation request.
    pub fn show_window(window: &mut Window) -> bool {
        let Ok(handle) = HasWindowHandle::window_handle(window).map(|handle| handle.as_raw())
        else {
            return false;
        };
        if let Err(error) = super::linux::restore_x11_window(handle) {
            tracing::warn!(%error, "Could not restore the X11 panel");
        }
        let (Some(token), Ok(display)) = (
            super::linux::take_activation_token(),
            HasDisplayHandle::display_handle(window).map(|display| display.as_raw()),
        ) else {
            return false;
        };
        if !matches!(handle, raw_window_handle::RawWindowHandle::Wayland(_)) {
            return false;
        }
        match super::linux::activate_wayland_window(handle, display, token) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "Could not activate the Wayland panel");
                false
            }
        }
    }
    // Wayland compositors control top-level placement; do not interpret the
    // host's coordinate hints as portable window positions.
    pub fn place_near_tray(_window: &Window, _anchor: Option<Anchor>) {}
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod platform {
    use gpui_kit::Window;

    use super::Anchor;

    pub fn hide_window(_window: &Window) {}
    pub fn show_window(_window: &mut Window) -> bool {
        false
    }
    pub fn place_near_tray(_window: &Window, _anchor: Option<Anchor>) {}
}

/// Whether the panel is on screen. While it is hidden in the tray nothing
/// repaints it. Stays true where there is no tray to hide it to.
static VISIBLE: AtomicBool = AtomicBool::new(true);

pub fn is_visible() -> bool {
    // Linux's taskbar can restore a minimized window without a tray command;
    // a closed layer-shell panel cannot be restored that way.
    (cfg!(target_os = "linux") && !layer_panel()) || VISIBLE.load(Ordering::Relaxed)
}

/// Set on Wayland with a tray when the panel is a layer-shell surface pinned
/// to the tray's corner: hiding closes it and showing opens a new one, since
/// an ordinary Wayland window can neither pick its position nor be hidden.
static LAYER_PANEL: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "linux")]
pub(crate) use linux::{wayland_layer_shell_available, x11_work_area};

pub fn layer_panel() -> bool {
    LAYER_PANEL.load(Ordering::Relaxed)
}

pub fn set_layer_panel(enabled: bool) {
    LAYER_PANEL.store(enabled, Ordering::Relaxed);
}

/// Gap between the layer-shell panel and the screen edges. The compositor
/// already keeps it clear of the desktop panel's exclusive zone.
#[cfg(target_os = "linux")]
const LAYER_MARGIN: f32 = 8.0;

/// The screen holding the tray click, else the first one.
#[cfg(target_os = "linux")]
pub fn layer_screen(
    anchor: Option<Anchor>,
    screens: &[gpui_kit::Bounds<gpui_kit::Pixels>],
) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
    use gpui_kit::{point, px};
    anchor
        .and_then(|anchor| {
            let click = point(px(anchor.left as f32), px(anchor.top as f32));
            screens.iter().find(|screen| screen.contains(&click))
        })
        .or_else(|| screens.first())
        .copied()
}

/// The screen corner nearest the tray click, as layer-shell edges. Without a
/// click position (the menu, the API, a first run) the bottom-right corner
/// most desktops keep their tray in.
#[cfg(target_os = "linux")]
pub fn layer_corner(
    anchor: Option<Anchor>,
    screen: Option<gpui_kit::Bounds<gpui_kit::Pixels>>,
) -> gpui_kit::layer_shell::Anchor {
    use gpui_kit::{layer_shell::Anchor as Edge, px};

    let (Some(anchor), Some(screen)) = (anchor, screen) else {
        return Edge::BOTTOM | Edge::RIGHT;
    };
    let (x, y) = (anchor.left as f32, anchor.top as f32);
    let center = screen.center();
    let horizontal = if px(x) < center.x {
        Edge::LEFT
    } else {
        Edge::RIGHT
    };
    let vertical = if px(y) < center.y {
        Edge::TOP
    } else {
        Edge::BOTTOM
    };
    horizontal | vertical
}

/// Margins (top, right, bottom, left) that keep the panel clear of the
/// desktop's own panels: the gap between the screen and its work area on each
/// side, plus [`LAYER_MARGIN`]. KWin does not count Plasma's panels (which are
/// not layer surfaces) when placing layer surfaces, so this is done here.
#[cfg(target_os = "linux")]
pub fn layer_margins(
    screen: Option<gpui_kit::Bounds<gpui_kit::Pixels>>,
    work_area: Option<gpui_kit::Bounds<gpui_kit::Pixels>>,
) -> (
    gpui_kit::Pixels,
    gpui_kit::Pixels,
    gpui_kit::Pixels,
    gpui_kit::Pixels,
) {
    use gpui_kit::px;
    let gap = px(LAYER_MARGIN);
    let (Some(screen), Some(work)) = (screen, work_area) else {
        return (gap, gap, gap, gap);
    };
    // The work area may span several screens; clip it to this one.
    let work = work.intersect(&screen);
    let reserved = |outer: gpui_kit::Pixels, inner: gpui_kit::Pixels| (outer - inner).max(px(0.));
    (
        reserved(work.top(), screen.top()) + gap,
        reserved(screen.right(), work.right()) + gap,
        reserved(screen.bottom(), work.bottom()) + gap,
        reserved(work.left(), screen.left()) + gap,
    )
}

/// Layer-shell options for the panel pinned to `corner`.
#[cfg(target_os = "linux")]
pub fn layer_options(
    corner: gpui_kit::layer_shell::Anchor,
    margin: (
        gpui_kit::Pixels,
        gpui_kit::Pixels,
        gpui_kit::Pixels,
        gpui_kit::Pixels,
    ),
) -> gpui_kit::layer_shell::LayerShellOptions {
    use gpui_kit::layer_shell;
    layer_shell::LayerShellOptions {
        namespace: "openmouse-bridge-panel".into(),
        layer: layer_shell::Layer::Top,
        anchor: corner,
        margin: Some(margin),
        keyboard_interactivity: layer_shell::KeyboardInteractivity::OnDemand,
        ..Default::default()
    }
}

/// Whether closing the panel can hide it to a tray icon; where it cannot, the
/// close button quits.
pub fn can_hide() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::available()
    }
    #[cfg(not(target_os = "linux"))]
    cfg!(any(target_os = "macos", windows))
}

pub fn hide(window: &mut Window) {
    platform::hide_window(window);
    #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
    VISIBLE.store(false, Ordering::Relaxed);
}

/// Moves the panel beside the tray icon (see [`Anchor`]); without an anchor,
/// to the tray's usual corner of the main screen.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn place(window: &Window, anchor: Option<Anchor>) {
    platform::place_near_tray(window, anchor);
}

pub fn show(window: &mut Window, anchor: Option<Anchor>) {
    platform::place_near_tray(window, anchor);
    if !platform::show_window(window) {
        window.activate_window();
    }
    VISIBLE.store(true, Ordering::Relaxed);
}

/// Builds the tray icon: the logo, the tooltip, and on Windows the menu.
#[cfg(any(target_os = "macos", windows))]
fn build_icon() -> Option<tray_icon::TrayIcon> {
    use tray_icon::TrayIconBuilder;
    #[cfg(windows)]
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};

    let icon = crate::runtime::tray_icon().ok()?;
    let builder = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("OpenMouse Bridge")
        .with_menu_on_left_click(false);
    // On macOS an NSStatusItem with a menu attached opens that menu on any
    // click and the click handler never runs, so neither a click nor a
    // double-click could show the panel. The panel has Restart and Quit
    // buttons of its own, so macOS gets no tray menu; Windows keeps one.
    #[cfg(windows)]
    let builder = {
        let open = MenuItem::with_id("open", "Open OpenMouse Bridge", true, None);
        let restart = MenuItem::with_id("restart", "Restart", true, None);
        let quit = MenuItem::with_id("quit", "Quit Bridge", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &PredefinedMenuItem::separator(), &restart, &quit])
            .ok()?;
        builder.with_menu(Box::new(menu))
    };
    builder.build().ok()
}

/// Creates the tray icon and makes closing `window` hide it. Returns the
/// channel its clicks and menu items arrive on and the icon itself, or `None`
/// when there is no tray (then closing the window still quits).
#[cfg(any(target_os = "macos", windows))]
pub fn install(
    cx: &mut App,
    window: AnyWindowHandle,
) -> Option<(mpsc::Receiver<Command>, TrayHandle)> {
    #[cfg(windows)]
    use tray_icon::menu::MenuEvent;
    use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

    let tray = build_icon()?;

    // The handlers run on the main thread inside the platform's event
    // dispatch; they only queue a command for the poll loop in `run`.
    let (sender, receiver) = mpsc::channel();
    #[cfg(windows)]
    {
        let menu_sender = sender.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let command = match event.id.as_ref() {
                "open" => Command::Show(None),
                "restart" => Command::Restart,
                "quit" => Command::Quit,
                _ => return,
            };
            let _ = menu_sender.send(command);
        }));
    }
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if matches!(
            event,
            TrayIconEvent::Click { .. } | TrayIconEvent::DoubleClick { .. }
        ) {
            tracing::debug!(?event, "tray icon event");
        }
        // A left click, a double-click, and on macOS (no menu there) a right
        // click all show the panel, beside the icon.
        let clicked = matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            }
        ) || (cfg!(target_os = "macos")
            && matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Right,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ));
        if clicked {
            #[cfg(target_os = "macos")]
            let anchor = platform::anchor_of_click();
            #[cfg(windows)]
            let anchor = match &event {
                TrayIconEvent::Click { rect, .. } | TrayIconEvent::DoubleClick { rect, .. } => {
                    platform::anchor_of_icon(rect)
                }
                _ => return,
            };
            let _ = sender.send(Command::Show(Some(anchor)));
        }
    }));

    let _ = cx.update_window(window, |_, window, cx| {
        window.on_window_should_close(cx, |window, _| {
            hide(window);
            false
        });
    });
    Some((
        receiver,
        TrayHandle {
            icon: tray,
            shown: None,
        },
    ))
}

#[cfg(target_os = "linux")]
pub fn install(
    _cx: &mut App,
    _window: AnyWindowHandle,
) -> Option<(mpsc::Receiver<Command>, TrayHandle)> {
    let (commands, icon) = match linux::install() {
        Ok(tray) => tray,
        Err(error) => {
            tracing::warn!(%error, "Linux tray unavailable; keeping the panel as a normal window");
            return None;
        }
    };
    Some((commands, TrayHandle { icon, shown: None }))
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub fn install(
    _cx: &mut App,
    _window: AnyWindowHandle,
) -> Option<(mpsc::Receiver<Command>, TrayHandle)> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use gpui_kit::{Bounds, layer_shell::Anchor as Edge, point, px, size};

    use super::*;

    fn screen() -> Bounds<gpui_kit::Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1200.)))
    }

    fn click(x: f64, y: f64) -> Option<Anchor> {
        Some(Anchor {
            left: x,
            top: y,
            right: x,
            bottom: y,
        })
    }

    #[test]
    fn the_panel_opens_in_the_corner_nearest_the_tray_click() {
        let screens = [screen()];
        let corner = |x, y| layer_corner(click(x, y), layer_screen(click(x, y), &screens));
        assert_eq!(corner(1800., 1185.), Edge::BOTTOM | Edge::RIGHT);
        assert_eq!(corner(1800., 15.), Edge::TOP | Edge::RIGHT);
        assert_eq!(corner(20., 1100.), Edge::BOTTOM | Edge::LEFT);
        // No click position: the usual bottom-right tray corner.
        assert_eq!(
            layer_corner(None, Some(screen())),
            Edge::BOTTOM | Edge::RIGHT
        );
    }

    #[test]
    fn margins_clear_the_desktop_panel_reserved_from_the_work_area() {
        // A 30 px bottom panel, as KWin reports it in _NET_WORKAREA.
        let work = Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1170.)));
        assert_eq!(
            layer_margins(Some(screen()), Some(work)),
            (px(8.), px(8.), px(38.), px(8.))
        );
        // A 48 px left panel.
        let work = Bounds::new(point(px(48.), px(0.)), size(px(1872.), px(1200.)));
        assert_eq!(
            layer_margins(Some(screen()), Some(work)),
            (px(8.), px(8.), px(8.), px(56.))
        );
        // No work area: just the gap.
        assert_eq!(
            layer_margins(Some(screen()), None),
            (px(8.), px(8.), px(8.), px(8.))
        );
    }
}
