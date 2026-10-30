//! The tray icon for the GPUI shell: closing the panel hides it, a click on the
//! icon brings it back, and the menu offers Open / Restart / Quit.
//!
//! macOS and Windows only for now. Linux has no tray icon yet; there the panel
//! is a plain window and closing it quits.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use gpui_kit::{AnyWindowHandle, App, Window};
#[cfg(any(target_os = "macos", windows))]
use gpui_kit::AppContext as _;

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
    #[cfg(any(target_os = "macos", windows))]
    icon: tray_icon::TrayIcon,
    shown: Option<TrayBattery>,
}

impl TrayHandle {
    /// Shows `battery` on the icon, or the plain logo for `None`, when that
    /// differs from what is showing; calling this every poll is cheap.
    pub fn show_battery(&mut self, battery: Option<TrayBattery>) {
        if battery == self.shown {
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

    pub fn show_window(window: &mut Window) {
        if let Some(window) = ns_window(window) {
            window.makeKeyAndOrderFront(None);
        }
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

    pub fn show_window(window: &mut Window) {
        if let Some(hwnd) = hwnd_of(window) {
            unsafe {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
        }
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

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use gpui_kit::Window;

    use super::Anchor;

    pub fn hide_window(_window: &Window) {}
    pub fn show_window(_window: &mut Window) {}
    pub fn place_near_tray(_window: &Window, _anchor: Option<Anchor>) {}
}

/// Whether the panel is on screen. While it is hidden in the tray nothing
/// repaints it. Stays true where there is no tray to hide it to.
static VISIBLE: AtomicBool = AtomicBool::new(true);

pub fn is_visible() -> bool {
    VISIBLE.load(Ordering::Relaxed)
}

/// Whether closing the panel can hide it to a tray icon; where it cannot, the
/// close button quits.
pub fn can_hide() -> bool {
    cfg!(any(target_os = "macos", windows))
}

pub fn hide(window: &Window) {
    platform::hide_window(window);
    #[cfg(any(target_os = "macos", windows))]
    VISIBLE.store(false, Ordering::Relaxed);
}

/// Moves the panel beside the tray icon (see [`Anchor`]); without an anchor,
/// to the tray's usual corner of the main screen.
pub fn place(window: &Window, anchor: Option<Anchor>) {
    platform::place_near_tray(window, anchor);
}

pub fn show(window: &mut Window, anchor: Option<Anchor>) {
    platform::place_near_tray(window, anchor);
    platform::show_window(window);
    window.activate_window();
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

#[cfg(not(any(target_os = "macos", windows)))]
pub fn install(
    _cx: &mut App,
    _window: AnyWindowHandle,
) -> Option<(mpsc::Receiver<Command>, TrayHandle)> {
    None
}
