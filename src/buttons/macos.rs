//! Presses keys and side buttons on macOS by posting CoreGraphics events.
//!
//! macOS only lets a program post input once the user has allowed it under
//! System Settings > Privacy & Security > Accessibility; without that the
//! events are dropped silently, so `prepare` checks and asks for it.

use std::sync::{Mutex, PoisonError};

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventType, CGMouseButton,
    CGPreflightPostEventAccess, CGRequestPostEventAccess,
};

use super::{Action, BACK, HeldKeys, KeyAction, Step, keymap};

/// The keys held down, counted, so a modifier two buttons share is not let go early.
static HELD: Mutex<HeldKeys> = Mutex::new(HeldKeys::new());

/// Asks macOS to let Bridge post input, which shows its permission prompt the first time.
pub fn prepare() {
    if !CGPreflightPostEventAccess() {
        CGRequestPostEventAccess();
        tracing::warn!(
            "macOS has not allowed Bridge to press keys, so button assignments do nothing until it is \
             allowed under System Settings > Privacy & Security > Accessibility"
        );
    }
}

pub fn execute(step: Step) {
    match step {
        Step::Down(action) => press(action, true),
        Step::Up(action) => press(action, false),
        Step::SideDown(bit) => side(bit, true),
        Step::SideUp(bit) => side(bit, false),
    }
}

/// Modifiers go down before the key and come up after it.
fn press(action: Action, down: bool) {
    let Action::Key(KeyAction { usage, modifiers }) = action else {
        tracing::warn!("Media keys cannot be pressed by Bridge on macOS yet");
        return;
    };
    let Some(key) = keymap::mac_key(usage) else {
        tracing::warn!(usage, "That key cannot be pressed by Bridge");
        return;
    };
    let mut order = keymap::mac_modifier_keys(modifiers);
    order.push(key);
    if !down {
        order.reverse();
    }
    let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
    for code in order {
        let changes = if down {
            held.press(code)
        } else {
            held.release(code)
        };
        if !changes {
            continue;
        }
        match CGEvent::new_keyboard_event(None, code, down) {
            Some(event) => CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event)),
            None => tracing::warn!("macOS did not create the key event"),
        }
    }
}

/// The side buttons are the mouse's fourth (Back) and fifth (Forward) buttons.
fn side(bit: u8, down: bool) {
    let button: u32 = if bit == BACK { 3 } else { 4 };
    let position =
        CGEvent::new(None).map_or(CGPoint::ZERO, |event| CGEvent::location(Some(&event)));
    let kind = if down {
        CGEventType::OtherMouseDown
    } else {
        CGEventType::OtherMouseUp
    };
    match CGEvent::new_mouse_event(None, kind, position, CGMouseButton::Center) {
        Some(event) => {
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventButtonNumber,
                i64::from(button),
            );
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        }
        None => tracing::warn!("macOS did not create the mouse event"),
    }
}
