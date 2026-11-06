//! StatusNotifierItem and DBusMenu on the session bus. ksni owns its worker
//! thread; callbacks only queue commands for GPUI's existing poll loop.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
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
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.sender.send(Command::Show(None));
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Open OpenMouse Bridge".into(),
                activate: Box::new(|tray: &mut Self| {
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
    let handle = LinuxTray {
        sender: sender.clone(),
        icon,
        tooltip,
    }
    .spawn()?;
    AVAILABLE.store(true, Ordering::Relaxed);
    Ok((
        receiver,
        Handle {
            service: handle,
            sender,
        },
    ))
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
    Ok((argb_icon(rgba, width, height), tooltip))
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
    fn icon_uses_network_order_argb_with_alpha_preserved() {
        let icon = argb_icon(vec![10, 20, 30, 40, 50, 60, 70, 0], 2, 1);
        assert_eq!((icon.width, icon.height), (2, 1));
        assert_eq!(icon.data, [40, 10, 20, 30, 0, 50, 60, 70]);
    }

    #[test]
    fn activation_and_menu_queue_panel_and_exit_commands() {
        let (sender, receiver) = mpsc::channel();
        let (icon, tooltip) = presentation(None).unwrap();
        let mut tray = LinuxTray {
            sender,
            icon,
            tooltip,
        };
        tray.activate(100, 200);
        assert!(matches!(receiver.try_recv(), Ok(Command::Show(None))));
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
