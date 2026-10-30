//! What the tray icon shows for the connected mouse's battery: which battery,
//! the colour it is drawn in, the tooltip and the icon.

#[cfg(any(target_os = "macos", windows))]
use anyhow::{Context, Result};
use openmouse_bridge::{
    platform::{self, TrayBatteryStyle},
    service::{BridgeSnapshot, DeviceBattery},
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tray_icon::Icon;

const ACCENT: (u8, u8, u8) = (93, 222, 137);
const WARNING: (u8, u8, u8) = (242, 184, 75);
const DANGER: (u8, u8, u8) = (255, 107, 107);
const MUTED: (u8, u8, u8) = (170, 176, 183);

/// Windows' tooltip buffer tops out at 128 UTF-16 units and device names can be
/// long, so the tooltip is cut here.
#[cfg(any(target_os = "macos", windows, test))]
const TOOLTIP_CHARS: usize = 120;

/// What the tray shows. Comparing two of these is how the shell knows nothing
/// changed and the icon need not be redrawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayBattery {
    pub device_name: String,
    pub percent: u8,
    pub charging: bool,
    pub stale: bool,
    /// Green charged or charging, amber mid, red at or below the alert
    /// threshold, gray when the reading is stale.
    pub ink: (u8, u8, u8),
    pub minutes_remaining: Option<u32>,
    /// The icon's side in pixels.
    pub size: u32,
    /// The battery glyph or the percentage number.
    pub style: TrayBatteryStyle,
}

/// The battery the tray shows: a fresh reading if there is one, else the first
/// remembered one, drawn as last known.
pub fn select(snapshot: &BridgeSnapshot) -> Option<&DeviceBattery> {
    snapshot
        .batteries
        .iter()
        .find(|battery| !battery.stale)
        .or_else(|| snapshot.batteries.first())
}

pub fn ink(percent: u8, charging: bool, stale: bool, threshold: u8) -> (u8, u8, u8) {
    if stale {
        MUTED
    } else if charging {
        ACCENT
    } else if percent <= threshold {
        DANGER
    } else if percent <= 50 {
        WARNING
    } else {
        ACCENT
    }
}

/// The icon is drawn at twice the menu bar's height on macOS, which scales the
/// image down to fit and so stays sharp on a Retina display.
fn icon_size() -> u32 {
    if cfg!(target_os = "macos") {
        platform::tray_icon_size() * 2
    } else {
        platform::tray_icon_size()
    }
}

/// What to show for `snapshot`; `None` means no mouse has reported a battery
/// and the plain logo stays.
pub fn from_snapshot(snapshot: Option<&BridgeSnapshot>) -> Option<TrayBattery> {
    let snapshot = snapshot?;
    let battery = select(snapshot)?;
    let percent = battery.percent.min(100);
    Some(TrayBattery {
        device_name: battery.device_name.clone(),
        percent,
        charging: battery.charging,
        stale: battery.stale,
        ink: ink(
            percent,
            battery.charging,
            battery.stale,
            snapshot.battery_threshold_percent,
        ),
        minutes_remaining: battery.minutes_remaining,
        size: icon_size(),
        style: TrayBatteryStyle::from_config_str(&snapshot.tray_battery_style),
    })
}

#[cfg(any(target_os = "macos", windows, test))]
pub fn remaining_text(minutes: u32) -> String {
    let days = minutes / (24 * 60);
    let hours = minutes % (24 * 60) / 60;
    let minutes = minutes % 60;
    match (days, hours, minutes) {
        (0, 0, minutes) => format!("{minutes} m"),
        (0, hours, 0) => format!("{hours} h"),
        (0, hours, minutes) => format!("{hours} h {minutes} m"),
        (days, 0, _) => format!("{days} d"),
        (days, hours, _) => format!("{days} d {hours} h"),
    }
}

/// `Device — 87% · Charging · full in 45 m`.
#[cfg(any(target_os = "macos", windows, test))]
pub fn tooltip(battery: &TrayBattery) -> String {
    let status = match (battery.charging, battery.stale) {
        (true, _) => " · Charging",
        (false, true) => " · Not updating",
        (false, false) => "",
    };
    let estimate = battery.minutes_remaining.map(|minutes| {
        if battery.charging {
            format!("full in {}", remaining_text(minutes))
        } else {
            format!("{} left", remaining_text(minutes))
        }
    });
    let text = match estimate {
        Some(estimate) => format!(
            "{} — {}%{status} · {estimate}",
            battery.device_name, battery.percent
        ),
        None => format!("{} — {}%{status}", battery.device_name, battery.percent),
    };
    text.chars().take(TOOLTIP_CHARS).collect()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn icon(battery: &TrayBattery) -> Result<Icon> {
    let (rgba, width, height) = platform::battery_icon_rgba(
        battery.percent,
        battery.charging,
        battery.style,
        battery.size,
        battery.ink,
    )?;
    Icon::from_rgba(rgba, width, height).context("could not create the battery tray icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(name: &str, percent: u8, charging: bool, stale: bool) -> DeviceBattery {
        DeviceBattery {
            device_id: name.into(),
            device_name: name.into(),
            percent,
            charging,
            stale,
            minutes_remaining: None,
            dpi: None,
            polling_rate_hz: None,
            switch_mode: None,
        }
    }

    fn tray(percent: u8, charging: bool, stale: bool, minutes: Option<u32>) -> TrayBattery {
        TrayBattery {
            device_name: "Mouse".into(),
            percent,
            charging,
            stale,
            ink: ink(percent, charging, stale, 20),
            minutes_remaining: minutes,
            size: 22,
            style: TrayBatteryStyle::Glyph,
        }
    }

    #[test]
    fn ink_follows_the_charge_state() {
        assert_eq!(ink(90, false, true, 20), MUTED, "stale is gray");
        assert_eq!(
            ink(5, true, false, 20),
            ACCENT,
            "charging is green even when low"
        );
        assert_eq!(ink(20, false, false, 20), DANGER, "at the threshold");
        assert_eq!(ink(21, false, false, 20), WARNING);
        assert_eq!(ink(50, false, false, 20), WARNING);
        assert_eq!(ink(51, false, false, 20), ACCENT);
    }

    #[test]
    fn remaining_time_reads_naturally() {
        assert_eq!(remaining_text(0), "0 m");
        assert_eq!(remaining_text(45), "45 m");
        assert_eq!(remaining_text(60), "1 h");
        assert_eq!(remaining_text(90), "1 h 30 m");
        assert_eq!(remaining_text(24 * 60), "1 d");
        assert_eq!(remaining_text(24 * 60 + 90), "1 d 1 h");
        assert_eq!(remaining_text(47 * 60 + 59), "1 d 23 h");
    }

    #[test]
    fn the_tooltip_spells_out_the_state() {
        assert_eq!(tooltip(&tray(87, false, false, None)), "Mouse — 87%");
        assert_eq!(
            tooltip(&tray(87, true, false, Some(45))),
            "Mouse — 87% · Charging · full in 45 m"
        );
        assert_eq!(
            tooltip(&tray(40, false, false, Some(150))),
            "Mouse — 40% · 2 h 30 m left"
        );
        assert_eq!(
            tooltip(&tray(40, false, true, None)),
            "Mouse — 40% · Not updating"
        );
    }

    #[test]
    fn a_long_name_never_overruns_the_tooltip() {
        let mut long = tray(50, false, false, Some(600));
        long.device_name = "Wireless Gaming Mouse ".repeat(20);
        assert_eq!(tooltip(&long).chars().count(), TOOLTIP_CHARS);
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn the_icon_renders_for_both_styles_and_every_state() {
        for style in [TrayBatteryStyle::Glyph, TrayBatteryStyle::Percentage] {
            for (percent, charging, stale) in [
                (0, false, false),
                (55, false, false),
                (100, true, false),
                (30, false, true),
            ] {
                let mut shown = tray(percent, charging, stale, None);
                shown.style = style;
                icon(&shown).expect("an icon");
            }
        }
    }

    #[test]
    fn a_fresh_reading_beats_a_remembered_one() {
        let mut snapshot = crate::gpui_shell::test_snapshot();
        snapshot.batteries = vec![
            battery("Old", 10, false, true),
            battery("Live", 80, false, false),
        ];
        assert_eq!(select(&snapshot).unwrap().device_name, "Live");
        snapshot.batteries = vec![battery("Old", 10, false, true)];
        assert_eq!(select(&snapshot).unwrap().device_name, "Old");
        snapshot.batteries.clear();
        assert!(select(&snapshot).is_none());
        assert!(from_snapshot(Some(&snapshot)).is_none());
        assert!(from_snapshot(None).is_none());
    }

    #[test]
    fn the_snapshot_supplies_the_threshold_and_the_style() {
        let mut snapshot = crate::gpui_shell::test_snapshot();
        snapshot.batteries = vec![battery("Mouse", 25, false, false)];
        snapshot.battery_threshold_percent = 30;
        snapshot.tray_battery_style = "percentage".into();
        let shown = from_snapshot(Some(&snapshot)).unwrap();
        assert_eq!(shown.ink, DANGER, "25% is under a 30% threshold");
        assert_eq!(shown.style, TrayBatteryStyle::Percentage);
        snapshot.tray_battery_style = "nonsense".into();
        assert_eq!(
            from_snapshot(Some(&snapshot)).unwrap().style,
            TrayBatteryStyle::Glyph
        );
    }
}
