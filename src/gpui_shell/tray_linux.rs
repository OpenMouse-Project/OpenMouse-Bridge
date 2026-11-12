//! StatusNotifierItem and DBusMenu on the session bus. ksni owns its worker
//! thread; callbacks only queue commands for GPUI's existing poll loop.

use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use ksni::{Tray, blocking::TrayMethods as _};
use openmouse_bridge::platform;

use super::Command;
use crate::tray_battery::{self, TrayBattery};

static AVAILABLE: AtomicBool = AtomicBool::new(false);

pub(super) fn available() -> bool {
    AVAILABLE.load(Ordering::Relaxed)
}

/// ICCCM Iconic -> Normal requires mapping the client window. GPUI's X11
/// activate only requests focus through _NET_ACTIVE_WINDOW, which the WM may
/// reject. Map first so restoration does not depend on that focus request.
pub(super) fn restore_x11_window(handle: raw_window_handle::RawWindowHandle) -> Result<()> {
    use raw_window_handle::RawWindowHandle;
    use x11rb::protocol::xproto::ConnectionExt as _;

    // GPUI's X11 backend exposes Xcb handles. Native Wayland windows must not
    // open an X connection, even when DISPLAY is also set for XWayland.
    let RawWindowHandle::Xcb(handle) = handle else {
        return Ok(());
    };
    let (connection, _) = x11rb::connect(None).context("could not open the X display")?;
    connection
        .map_window(handle.window.get())
        .context("could not request X11 panel restoration")?
        .check()
        .context("could not map the X11 panel")?;
    Ok(())
}

/// The XDG activation token the tray host sent with the last click, kept until
/// the panel is shown. On Wayland only a token from the user's click lets the
/// compositor raise (and un-minimize) the panel.
static ACTIVATION_TOKEN: Mutex<Option<String>> = Mutex::new(None);

/// Keeps the host's token for the click being handled. KDE can deliver it just
/// after the click event, so wait briefly (ksni's own grace is 20 ms).
fn capture_activation_token() {
    let deadline = Instant::now() + Duration::from_millis(25);
    let token = loop {
        if let Some(token) = ksni::current_xdg_activation_token() {
            break Some(token);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    *ACTIVATION_TOKEN.lock().unwrap_or_else(|e| e.into_inner()) = token;
}

pub(super) fn take_activation_token() -> Option<String> {
    ACTIVATION_TOKEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// Raises GPUI's Wayland surface with the tray host's activation token.
///
/// GPUI's `activate_window` requests its own token with a stale serial, which
/// KWin and Mutter reject, so the minimized panel would only flash its taskbar
/// entry. This opens a second queue on GPUI's libwayland display and activates
/// the surface with the token the host issued for the click. X11 handles are
/// ignored; `restore_x11_window` covers them.
#[cfg(target_os = "linux")]
pub(super) fn activate_wayland_window(
    window: raw_window_handle::RawWindowHandle,
    display: raw_window_handle::RawDisplayHandle,
    token: String,
) -> Result<()> {
    use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
    use wayland_client::{
        Connection, Dispatch, Proxy, QueueHandle,
        backend::{Backend, ObjectId},
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_registry, wl_surface::WlSurface},
    };
    use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

    struct State;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<XdgActivationV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &XdgActivationV1,
            _: <XdgActivationV1 as Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    let (RawWindowHandle::Wayland(window), RawDisplayHandle::Wayland(display)) = (window, display)
    else {
        return Ok(());
    };
    // SAFETY: both pointers come from GPUI's live window, which outlives this
    // call, and GPUI's Wayland client uses libwayland (client_system).
    let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
    let connection = Connection::from_backend(backend);
    let surface_id =
        unsafe { ObjectId::from_ptr(WlSurface::interface(), window.surface.as_ptr().cast()) }
            .context("could not wrap the panel's Wayland surface")?;
    let surface = WlSurface::from_id(&connection, surface_id)
        .context("could not use the panel's Wayland surface")?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&connection).context("could not read Wayland globals")?;
    let activation: XdgActivationV1 = globals
        .bind(&queue.handle(), 1..=1, ())
        .context("the compositor has no xdg_activation_v1")?;
    activation.activate(token, &surface);
    activation.destroy();
    queue
        .roundtrip(&mut State)
        .context("could not send the panel activation")?;
    Ok(())
}

/// The desktop's work area (the screen minus its panels) from the X root's
/// `_NET_WORKAREA`, which KWin and most window managers publish, through
/// XWayland under Wayland. GPUI's displays report the whole screen.
pub(crate) fn x11_work_area() -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
    use x11rb::{
        connection::Connection as _,
        protocol::xproto::{AtomEnum, ConnectionExt as _},
    };
    let (connection, screen) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    let atom = connection
        .intern_atom(true, b"_NET_WORKAREA")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let reply = connection
        .get_property(false, root, atom, AtomEnum::CARDINAL, 0, 4)
        .ok()?
        .reply()
        .ok()?;
    let [x, y, width, height] = reply.value32()?.collect::<Vec<_>>()[..] else {
        return None;
    };
    use gpui_kit::{point, px, size};
    Some(gpui_kit::Bounds::new(
        point(px(x as f32), px(y as f32)),
        size(px(width as f32), px(height as f32)),
    ))
}

/// Whether GPUI's Wayland display offers wlr-layer-shell (KWin, wlroots and
/// most others do; GNOME's Mutter does not). Not Wayland: false.
#[cfg(target_os = "linux")]
pub(crate) fn wayland_layer_shell_available(display: raw_window_handle::RawDisplayHandle) -> bool {
    use raw_window_handle::RawDisplayHandle;
    use wayland_client::{
        Connection, Dispatch, QueueHandle,
        backend::Backend,
        globals::{GlobalListContents, registry_queue_init},
        protocol::wl_registry,
    };

    struct State;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    let RawDisplayHandle::Wayland(display) = display else {
        return false;
    };
    // SAFETY: the pointer is GPUI's live libwayland display (client_system).
    let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
    let connection = Connection::from_backend(backend);
    registry_queue_init::<State>(&connection).is_ok_and(|(globals, _)| {
        globals.contents().with_list(|list| {
            list.iter()
                .any(|global| global.interface == "zwlr_layer_shell_v1")
        })
    })
}

#[cfg(not(target_os = "linux"))]
pub(super) fn wayland_layer_shell_available(_display: raw_window_handle::RawDisplayHandle) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub(super) fn activate_wayland_window(
    _window: raw_window_handle::RawWindowHandle,
    _display: raw_window_handle::RawDisplayHandle,
    _token: String,
) -> Result<()> {
    Ok(())
}

pub(super) struct Handle {
    service: ksni::blocking::Handle<LinuxTray>,
    sender: mpsc::Sender<Command>,
}

impl Handle {
    pub(super) fn check_health(&self) {
        if self.service.is_closed() && AVAILABLE.swap(false, Ordering::Relaxed) {
            let _ = self.sender.send(Command::Show(None));
        }
    }

    pub(super) fn show_battery(&self, battery: Option<&TrayBattery>) -> Result<()> {
        let (icon, tooltip) = presentation(battery)?;
        self.service
            .update(|tray| {
                tray.icon = icon;
                tray.tooltip = tooltip;
            })
            .context("Linux tray service stopped")
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        AVAILABLE.store(false, Ordering::Relaxed);
        self.service.shutdown();
    }
}

struct LinuxTray {
    sender: mpsc::Sender<Command>,
    icon: ksni::Icon,
    tooltip: String,
}

impl Tray for LinuxTray {
    fn id(&self) -> String {
        "io.openmouse.bridge".into()
    }
    fn title(&self) -> String {
        "OpenMouse Bridge".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        // The title is plain text (unlike the description, which allows markup).
        ksni::ToolTip {
            title: self.tooltip.clone(),
            ..Default::default()
        }
    }
    fn activate(&mut self, x: i32, y: i32) {
        capture_activation_token();
        // The host passes the click's global position; (0, 0) means unknown.
        let anchor = (x != 0 || y != 0).then(|| super::Anchor {
            left: f64::from(x),
            top: f64::from(y),
            right: f64::from(x),
            bottom: f64::from(y),
        });
        if let Some(command) = super::click_command(anchor) {
            let _ = self.sender.send(command);
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Open OpenMouse Bridge".into(),
                activate: Box::new(|tray: &mut Self| {
                    capture_activation_token();
                    let _ = tray.sender.send(Command::Show(None));
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Restart".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.sender.send(Command::Restart);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit Bridge".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.sender.send(Command::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        AVAILABLE.store(false, Ordering::Relaxed);
        let _ = self.sender.send(Command::Show(None));
        // Stay alive: ksni re-registers when the desktop's watcher returns.
        true
    }
    fn watcher_online(&self) {
        AVAILABLE.store(true, Ordering::Relaxed);
    }
}

pub(super) fn install() -> Result<(mpsc::Receiver<Command>, Handle)> {
    let (sender, receiver) = mpsc::channel();
    let (icon, tooltip) = presentation(None)?;
    // With no session bus or SNI host, spawn fails and the panel stays visible.
    // Do not assume_sni_available: GNOME without an extension has no tray.
    let handle = start_with_availability(&AVAILABLE, || {
        Ok(LinuxTray {
            sender: sender.clone(),
            icon,
            tooltip,
        }
        .spawn()?)
    })?;
    Ok((
        receiver,
        Handle {
            service: handle,
            sender,
        },
    ))
}

fn start_with_availability<T>(
    availability: &AtomicBool,
    start: impl FnOnce() -> Result<T>,
) -> Result<T> {
    // spawn can deliver watcher callbacks before returning. Initialize first
    // so a startup loss notification cannot be overwritten by a later store.
    availability.store(true, Ordering::Relaxed);
    let result = start();
    if result.is_err() {
        availability.store(false, Ordering::Relaxed);
    }
    result
}

fn presentation(battery: Option<&TrayBattery>) -> Result<(ksni::Icon, String)> {
    let (rgba, width, height) = match battery {
        Some(battery) => platform::battery_icon_rgba(
            battery.percent,
            battery.charging,
            battery.style,
            battery.size,
            battery.ink,
        )?,
        None => crate::runtime::openmouse_icon_rgba()?,
    };
    let tooltip = battery
        .map(tray_battery::tooltip)
        .unwrap_or_else(|| "OpenMouse Bridge".into());
    let (rgba, side) = pad_square(rgba, width, height);
    Ok((argb_icon(rgba, side, side), tooltip))
}

/// Centres the image on a transparent square. Tray hosts scale the pixmap
/// into a square slot, so the tall logo would otherwise be stretched wide.
fn pad_square(rgba: Vec<u8>, width: u32, height: u32) -> (Vec<u8>, u32) {
    if width == height {
        return (rgba, width);
    }
    let side = width.max(height);
    let (left, top) = ((side - width) / 2, (side - height) / 2);
    let mut square = vec![0; (side * side * 4) as usize];
    let row = (width * 4) as usize;
    for (y, source) in rgba.chunks_exact(row).enumerate() {
        let start = (((top + y as u32) * side + left) * 4) as usize;
        square[start..start + row].copy_from_slice(source);
    }
    (square, side)
}

fn argb_icon(mut rgba: Vec<u8>, width: u32, height: u32) -> ksni::Icon {
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: width as i32,
        height: height as i32,
        data: rgba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_restore_does_not_require_an_x_connection() {
        use raw_window_handle::{RawWindowHandle, WaylandWindowHandle};
        let handle = WaylandWindowHandle::new(std::ptr::NonNull::dangling());
        restore_x11_window(RawWindowHandle::Wayland(handle)).unwrap();
    }

    #[test]
    fn watcher_loss_during_startup_is_not_overwritten() {
        let availability = AtomicBool::new(false);
        start_with_availability(&availability, || {
            assert!(availability.load(Ordering::Relaxed));
            // Model watcher_offline running before spawn returns its handle.
            availability.store(false, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        assert!(!availability.load(Ordering::Relaxed));
    }

    #[test]
    fn startup_success_and_failure_set_availability() {
        let availability = AtomicBool::new(false);
        start_with_availability(&availability, || Ok(())).unwrap();
        assert!(availability.load(Ordering::Relaxed));
        let result: Result<()> =
            start_with_availability(&availability, || anyhow::bail!("tray registration failed"));
        assert!(result.is_err());
        assert!(!availability.load(Ordering::Relaxed));
    }

    #[test]
    fn icon_uses_network_order_argb_with_alpha_preserved() {
        let icon = argb_icon(vec![10, 20, 30, 40, 50, 60, 70, 0], 2, 1);
        assert_eq!((icon.width, icon.height), (2, 1));
        assert_eq!(icon.data, [40, 10, 20, 30, 0, 50, 60, 70]);
    }

    #[test]
    fn clicks_toggle_the_panel_and_the_menu_opens_it_or_exits() {
        let (sender, receiver) = mpsc::channel();
        let (icon, tooltip) = presentation(None).unwrap();
        let mut tray = LinuxTray {
            sender,
            icon,
            tooltip,
        };
        tray.activate(100, 200);
        assert!(matches!(
            receiver.try_recv(),
            Ok(Command::Toggle(Some(super::super::Anchor {
                left: 100.0,
                top: 200.0,
                ..
            })))
        ));
        tray.activate(0, 0);
        assert!(matches!(receiver.try_recv(), Ok(Command::Toggle(None))));
        for item in tray.menu() {
            if let ksni::MenuItem::Standard(item) = item {
                (item.activate)(&mut tray);
            }
        }
        assert!(matches!(receiver.try_recv(), Ok(Command::Show(None))));
        assert!(matches!(receiver.try_recv(), Ok(Command::Restart)));
        assert!(matches!(receiver.try_recv(), Ok(Command::Quit)));
    }

    #[test]
    fn tall_icons_are_centred_on_a_square() {
        // A 1x2 image gains one transparent column; integer centring puts it
        // on the right.
        let (square, side) = pad_square(vec![1, 2, 3, 4, 5, 6, 7, 8], 1, 2);
        assert_eq!(side, 2);
        assert_eq!(square, vec![1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0]);
        let (icon, _) = presentation(None).unwrap();
        assert_eq!(icon.width, icon.height);
    }

    #[test]
    fn watcher_loss_restores_panel_and_keeps_service_for_reregistration() {
        let (sender, receiver) = mpsc::channel();
        let (icon, tooltip) = presentation(None).unwrap();
        let tray = LinuxTray {
            sender,
            icon,
            tooltip,
        };
        tray.watcher_online();
        assert!(available());
        assert!(tray.watcher_offline(ksni::OfflineReason::No));
        assert!(!available());
        assert!(matches!(receiver.try_recv(), Ok(Command::Show(None))));
    }

    #[test]
    fn battery_and_plain_logo_have_matching_tooltips_and_pixels() {
        let battery = TrayBattery {
            device_name: "Mouse".into(),
            percent: 37,
            charging: false,
            stale: false,
            ink: (242, 184, 75),
            minutes_remaining: Some(90),
            size: 22,
            style: platform::TrayBatteryStyle::Glyph,
        };
        let (icon, tooltip) = presentation(Some(&battery)).unwrap();
        assert_eq!(tooltip, tray_battery::tooltip(&battery));
        assert_eq!(icon.data.len(), 22 * 22 * 4);
        assert!(icon.data.as_chunks::<4>().0.contains(&[255, 242, 184, 75]));
        let (logo, tooltip) = presentation(None).unwrap();
        assert_eq!(tooltip, "OpenMouse Bridge");
        assert_eq!(logo.data.len(), (logo.width * logo.height * 4) as usize);
    }
}
