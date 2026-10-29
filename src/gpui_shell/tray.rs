//! The tray icon for the GPUI shell: closing the panel hides it, a click on the
//! icon brings it back, and the menu offers Open / Restart / Quit.
//!
//! macOS and Windows only for now. Linux needs the GTK main loop that
//! `tray-icon` wants and GPUI does not provide; there the panel is a plain
//! window and closing it quits.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use gpui_kit::{AnyWindowHandle, App, AppContext as _, Window};

#[derive(Debug)]
pub enum Command {
    Show,
    Restart,
    Quit,
}

#[cfg(target_os = "macos")]
mod platform {
    use gpui_kit::Window;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSView, NSWindow};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

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
}

#[cfg(windows)]
mod platform {
    use gpui_kit::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SW_HIDE, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };

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
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use gpui_kit::Window;

    pub fn hide_window(_window: &Window) {}
    pub fn show_window(_window: &mut Window) {}
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

pub fn show(window: &mut Window) {
    platform::show_window(window);
    window.activate_window();
    VISIBLE.store(true, Ordering::Relaxed);
}

/// Creates the tray icon and makes closing `window` hide it. Returns the
/// channel its clicks and menu items arrive on, or `None` when there is no tray
/// (then closing the window still quits).
#[cfg(any(target_os = "macos", windows))]
pub fn install(cx: &mut App, window: AnyWindowHandle) -> Option<mpsc::Receiver<Command>> {
    use tray_icon::{
        MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };

    let icon = crate::desktop::tray_icon().ok()?;
    let open = MenuItem::with_id("open", "Open OpenMouse Bridge", true, None);
    let restart = MenuItem::with_id("restart", "Restart", true, None);
    let quit = MenuItem::with_id("quit", "Quit Bridge", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &PredefinedMenuItem::separator(), &restart, &quit])
        .ok()?;
    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("OpenMouse Bridge")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
        .ok()?;

    // The handlers run on the main thread inside the platform's event
    // dispatch; they only queue a command for the poll loop in `run`.
    let (sender, receiver) = mpsc::channel();
    let menu_sender = sender.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let command = match event.id.as_ref() {
            "open" => Command::Show,
            "restart" => Command::Restart,
            "quit" => Command::Quit,
            _ => return,
        };
        let _ = menu_sender.send(command);
    }));
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
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
        );
        if clicked {
            let _ = sender.send(Command::Show);
        }
    }));

    // A task that never finishes owns the icon, so it lives as long as the app.
    cx.spawn(async move |_cx| {
        let _tray = tray;
        std::future::pending::<()>().await;
    })
    .detach();

    let _ = cx.update_window(window, |_, window, cx| {
        window.on_window_should_close(cx, |window, _| {
            hide(window);
            false
        });
    });
    Some(receiver)
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn install(_cx: &mut App, _window: AnyWindowHandle) -> Option<mpsc::Receiver<Command>> {
    None
}
