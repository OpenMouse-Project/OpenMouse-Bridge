//! Presses keys and side buttons on Linux through virtual devices in `/dev/uinput`.
//!
//! uinput works the same on X11 and Wayland. The keyboard and the mouse are two
//! devices because a desktop only treats a device as a pointer when it has
//! relative axes; a button on a keyboard-only device would be ignored.

use std::{
    fs::{File, OpenOptions},
    io::{Error, Write},
    mem::{size_of_val, zeroed},
    os::fd::AsRawFd,
    sync::{Mutex, PoisonError},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use super::{Action, BACK, HeldKeys, KeyAction, Step, keymap};

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BUS_VIRTUAL: u16 = 0x06;
/// Keyboard codes the keymap can produce all sit below this.
const KEY_LIMIT: u16 = 256;

// ioctl requests from <linux/uinput.h>: UI_SET_EVBIT, UI_SET_KEYBIT, UI_SET_RELBIT, UI_DEV_SETUP, UI_DEV_CREATE.
const UI_SET_EVBIT: u32 = 0x4004_5564;
const UI_SET_KEYBIT: u32 = 0x4004_5565;
const UI_SET_RELBIT: u32 = 0x4004_5566;
const UI_DEV_SETUP: u32 = 0x405c_5503;
const UI_DEV_CREATE: u32 = 0x5501;

#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

struct Devices {
    keyboard: File,
    mouse: File,
    /// The keys held down, counted, so a modifier two buttons share is not let go early.
    held: HeldKeys,
}

/// Made on first use and kept until Bridge exits; closing the files removes the devices.
static DEVICES: Mutex<Option<Devices>> = Mutex::new(None);

/// Creates the virtual devices before the first button press, which the desktop
/// needs a moment to notice.
pub fn prepare() {
    let mut devices = DEVICES.lock().unwrap_or_else(PoisonError::into_inner);
    if devices.is_some() {
        return;
    }
    match create() {
        Ok(created) => *devices = Some(created),
        Err(error) => tracing::warn!(
            error = format!("{error:#}"),
            "Could not create the virtual keyboard and mouse, so button assignments do nothing. \
             Bridge needs write access to /dev/uinput; install-linux.sh adds a udev rule for it"
        ),
    }
}

fn create() -> Result<Devices> {
    let keyboard = device("OpenMouse Bridge keyboard", &[], 1..KEY_LIMIT)?;
    let mouse = device(
        "OpenMouse Bridge mouse",
        &[REL_X, REL_Y],
        [BTN_LEFT, BTN_SIDE, BTN_EXTRA],
    )?;
    thread::sleep(Duration::from_millis(200));
    Ok(Devices {
        keyboard,
        mouse,
        held: HeldKeys::new(),
    })
}

fn device(name: &str, axes: &[u16], keys: impl IntoIterator<Item = u16>) -> Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .context("could not open /dev/uinput")?;
    let fd = file.as_raw_fd();
    set(fd, UI_SET_EVBIT, EV_KEY)?;
    for key in keys {
        set(fd, UI_SET_KEYBIT, key)?;
    }
    if !axes.is_empty() {
        set(fd, UI_SET_EVBIT, EV_REL)?;
        for axis in axes {
            set(fd, UI_SET_RELBIT, *axis)?;
        }
    }
    let mut setup = UinputSetup {
        bustype: BUS_VIRTUAL,
        vendor: 0,
        product: 0,
        version: 1,
        name: [0; 80],
        ff_effects_max: 0,
    };
    let bytes = name.as_bytes();
    setup.name[..bytes.len().min(79)].copy_from_slice(&bytes[..bytes.len().min(79)]);
    // SAFETY: `setup` is the struct UI_DEV_SETUP expects and lives through the call.
    check(unsafe { libc::ioctl(fd, UI_DEV_SETUP as _, &setup) })?;
    // SAFETY: UI_DEV_CREATE takes no argument.
    check(unsafe { libc::ioctl(fd, UI_DEV_CREATE as _) })?;
    Ok(file)
}

fn set(fd: i32, request: u32, value: u16) -> Result<()> {
    // SAFETY: these requests take their argument by value.
    check(unsafe { libc::ioctl(fd, request as _, libc::c_int::from(value)) })
}

fn check(result: libc::c_int) -> Result<()> {
    if result < 0 {
        return Err(Error::last_os_error()).context("a /dev/uinput request failed");
    }
    Ok(())
}

pub fn execute(step: Step) {
    let mut devices = DEVICES.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(devices) = devices.as_mut() else {
        return;
    };
    match step {
        Step::Down(action) => press(devices, action, true),
        Step::Up(action) => press(devices, action, false),
        Step::SideDown(bit) => write_key(&devices.mouse, side(bit), true),
        Step::SideUp(bit) => write_key(&devices.mouse, side(bit), false),
    }
}

fn side(bit: u8) -> u16 {
    if bit == BACK { BTN_SIDE } else { BTN_EXTRA }
}

/// Modifiers go down before the key and come up after it.
fn press(devices: &mut Devices, action: Action, down: bool) {
    let mut order = match action {
        Action::Key(KeyAction { usage, modifiers }) => {
            let Some(key) = keymap::linux_key(usage) else {
                tracing::warn!(usage, "That key cannot be pressed by Bridge");
                return;
            };
            let mut order = keymap::linux_modifier_keys(modifiers);
            order.push(key);
            order
        }
        Action::Media(usage) => keymap::linux_media_key(usage).into_iter().collect(),
    };
    if !down {
        order.reverse();
    }
    for code in order {
        let changes = if down {
            devices.held.press(code)
        } else {
            devices.held.release(code)
        };
        if changes {
            write_key(&devices.keyboard, code, down);
        }
    }
}

fn write_key(mut device: &File, code: u16, down: bool) {
    let events = [event(EV_KEY, code, i32::from(down)), event(EV_SYN, 0, 0)];
    // SAFETY: `input_event` is plain integers, so its bytes are what the kernel reads.
    let bytes =
        unsafe { std::slice::from_raw_parts(events.as_ptr().cast::<u8>(), size_of_val(&events)) };
    if let Err(error) = device.write_all(bytes) {
        tracing::warn!(%error, "Linux did not accept the button input");
    }
}

fn event(kind: u16, code: u16, value: i32) -> libc::input_event {
    // SAFETY: an all-zero `input_event` is valid (integers only); the timestamp is filled in by the kernel.
    let mut event: libc::input_event = unsafe { zeroed() };
    event.type_ = kind;
    event.code = code;
    event.value = value;
    event
}
