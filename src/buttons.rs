//! Button assignments for a Logitech mouse running from software (Host mode).
//!
//! A mouse on the Host profile stops sending its side buttons as ordinary
//! clicks; it reports them as events instead, and something on the PC has to
//! act on them (G HUB does). While a game's profile has assignments, Bridge
//! runs `native-hid/src/button-listener.mjs`, which puts the mouse on Host, turns
//! those events on and prints them, and then presses the assigned key, or the
//! side button's normal Back/Forward click when nothing is assigned. When the
//! game ends the helper stops the events and puts the mouse back in the mode
//! it was in.
//!
//! The assignments live in a game profile's `snapshot.hostButtons`:
//! `{"8": {"key": 58, "modifiers": 0}, "16": {...}}`, keyed by the button's bit
//! in the mouse's report (8 the back side button, 16 the front one), each a HID
//! keyboard usage and a modifier mask (1 ctrl, 2 shift, 4 alt, 8 win).

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin},
    sync::{Arc, Mutex, PoisonError, TryLockError, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::ApplicationProfile;

/// The back side button's bit in the mouse's button report.
pub const BACK: u8 = 0x08;
/// The front side button's bit.
pub const FRONT: u8 = 0x10;
/// The only buttons a Host mode mouse stops sending as normal clicks.
const SIDE_BUTTONS: [u8; 2] = [BACK, FRONT];

/// How long the helper gets to stop the events and restore the mouse's mode: it
/// allows itself three seconds for each of the two, then closes the mouse.
const STOP_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a starting helper may hold the mouse before Bridge stops waiting.
const START_TIMEOUT: Duration = Duration::from_secs(12);
/// How long to leave a mouse alone after the helper could not start or died.
const RETRY_DELAY: Duration = Duration::from_secs(15);

/// A keyboard key and its modifiers, as a profile stores them (HID usage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyAction {
    pub usage: u8,
    pub modifiers: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Key(KeyAction),
    /// A media key, by its HID consumer usage (volume, play/pause, ...).
    Media(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Down(Action),
    Up(Action),
    /// Press the side button's normal click: `BACK` is Back, `FRONT` is Forward.
    SideDown(u8),
    SideUp(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignments {
    /// The brand the profile names ("Logitech"), which the helper picks its driver by.
    pub brand: String,
    /// The mouse's name as the profile stores it ("PRO X 3 SUPERSTRIKE"), so the
    /// helper listens to that mouse when several of the brand are connected.
    pub device: String,
    pub actions: BTreeMap<u8, Action>,
    /// Set for the default profile: it only listens to a mouse that is already
    /// on the Host profile, and never switches one to it.
    pub require_host: bool,
}

impl Assignments {
    pub fn from_profile(profile: &ApplicationProfile, is_default: bool) -> Option<Self> {
        let (brand, device) = profile
            .device
            .id
            .split_once(':')
            .unwrap_or((profile.device.id.as_str(), ""));
        if !brand.eq_ignore_ascii_case("logitech") {
            return None;
        }
        let buttons = profile
            .settings
            .snapshot
            .as_ref()?
            .get("hostButtons")?
            .as_object()?;
        let mut actions = BTreeMap::new();
        for (bit, action) in buttons {
            let bit: u8 = bit.parse().ok().filter(|bit| SIDE_BUTTONS.contains(bit))?;
            let action = if let Some(usage) = action.get("consumer") {
                let usage = u16::try_from(usage.as_u64()?).ok()?;
                media_virtual_key(usage)?;
                Action::Media(usage)
            } else {
                let usage = u8::try_from(action.get("key")?.as_u64()?).ok()?;
                // A key this system cannot press would also swallow the button's normal click.
                can_press(usage).then_some(())?;
                let modifiers =
                    u8::try_from(action.get("modifiers").and_then(Value::as_u64).unwrap_or(0))
                        .ok()
                        .filter(|modifiers| *modifiers <= 0x0f)?;
                Action::Key(KeyAction { usage, modifiers })
            };
            actions.insert(bit, action);
        }
        (!actions.is_empty()).then(|| Self {
            brand: brand.to_owned(),
            device: device.to_owned(),
            actions,
            require_host: is_default,
        })
    }
}

/// What each held side button does, as it was when it went down: `None` is its
/// normal click. A press uses the assignments of that moment and its release the
/// same action, even if the profile changed while the button was held.
pub type HeldButtons = BTreeMap<u8, Option<Action>>;

pub fn transitions(
    held: &mut HeldButtons,
    assignments: &Assignments,
    previous: u8,
    now: u8,
) -> Vec<Step> {
    let mut steps = Vec::new();
    for bit in SIDE_BUTTONS {
        let (was, is) = (previous & bit != 0, now & bit != 0);
        if was == is {
            continue;
        }
        if is {
            let action = assignments.actions.get(&bit).copied();
            held.insert(bit, action);
            steps.push(action.map_or(Step::SideDown(bit), Step::Down));
        } else {
            let action = held
                .remove(&bit)
                .unwrap_or_else(|| assignments.actions.get(&bit).copied());
            steps.push(action.map_or(Step::SideUp(bit), Step::Up));
        }
    }
    steps
}

/// The steps for a change of the held buttons, all under one set of assignments.
pub fn steps(assignments: &Assignments, previous: u8, now: u8) -> Vec<Step> {
    let mut held = HeldButtons::new();
    for bit in SIDE_BUTTONS {
        if previous & bit != 0 {
            held.insert(bit, assignments.actions.get(&bit).copied());
        }
    }
    transitions(&mut held, assignments, previous, now)
}

/// How many held assignments share each key code. A shared modifier goes down
/// once and comes up with the last assignment holding it, not the first.
#[derive(Default)]
pub struct HeldKeys(BTreeMap<u16, u32>);

impl HeldKeys {
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// True when the key was not down yet, so it has to be pressed.
    pub fn press(&mut self, key: u16) -> bool {
        let count = self.0.entry(key).or_default();
        *count += 1;
        *count == 1
    }

    /// True when no other assignment still holds the key, so it has to be released.
    pub fn release(&mut self, key: u16) -> bool {
        match self.0.get_mut(&key) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => {
                self.0.remove(&key);
                true
            }
            None => true,
        }
    }
}

/// Whether Bridge can press the key on this system. A Mac has no key for Print
/// Screen, Scroll Lock, Pause or F21 to F24, which the other systems do.
fn can_press(usage: u8) -> bool {
    if cfg!(target_os = "macos") {
        keymap::mac_key(usage).is_some()
    } else {
        virtual_key(usage).is_some()
    }
}

/// The Windows virtual-key code for a HID keyboard usage, for the keys a
/// profile can hold.
pub fn virtual_key(usage: u8) -> Option<u16> {
    Some(match usage {
        0x04..=0x1d => 0x41 + u16::from(usage - 0x04), // A-Z
        0x1e..=0x26 => 0x31 + u16::from(usage - 0x1e), // 1-9
        0x27 => 0x30,                                  // 0
        0x28 => 0x0d,                                  // Enter
        0x29 => 0x1b,                                  // Escape
        0x2a => 0x08,                                  // Backspace
        0x2b => 0x09,                                  // Tab
        0x2c => 0x20,                                  // Space
        0x2d => 0xbd,                                  // -
        0x2e => 0xbb,                                  // =
        0x2f => 0xdb,                                  // [
        0x30 => 0xdd,                                  // ]
        0x31 => 0xdc,                                  // \
        0x33 => 0xba,                                  // ;
        0x34 => 0xde,                                  // '
        0x35 => 0xc0,                                  // `
        0x36 => 0xbc,                                  // ,
        0x37 => 0xbe,                                  // .
        0x38 => 0xbf,                                  // /
        0x3a..=0x45 => 0x70 + u16::from(usage - 0x3a), // F1-F12
        0x46 => 0x2c,                                  // Print Screen
        0x47 => 0x91,                                  // Scroll Lock
        0x48 => 0x13,                                  // Pause
        0x49 => 0x2d,                                  // Insert
        0x4a => 0x24,                                  // Home
        0x4b => 0x21,                                  // Page Up
        0x4c => 0x2e,                                  // Delete
        0x4d => 0x23,                                  // End
        0x4e => 0x22,                                  // Page Down
        0x4f => 0x27,                                  // Right
        0x50 => 0x25,                                  // Left
        0x51 => 0x28,                                  // Down
        0x52 => 0x26,                                  // Up
        0x68..=0x73 => 0x7c + u16::from(usage - 0x68), // F13-F24
        _ => return None,
    })
}

/// The Windows virtual-key code for a HID consumer usage (media key).
pub fn media_virtual_key(usage: u16) -> Option<u16> {
    Some(match usage {
        0xe2 => 0xad, // Mute
        0xe9 => 0xaf, // Volume up
        0xea => 0xae, // Volume down
        0xcd => 0xb3, // Play / pause
        0xb5 => 0xb0, // Next track
        0xb6 => 0xb1, // Previous track
        0xb7 => 0xb2, // Stop
        _ => return None,
    })
}

/// The modifier keys a mask names, in the order they are pressed.
pub fn modifier_keys(modifiers: u8) -> Vec<u16> {
    [(0x01, 0x11), (0x02, 0x10), (0x04, 0x12), (0x08, 0x5b)] // ctrl, shift, alt, win
        .into_iter()
        .filter(|(bit, _)| modifiers & bit != 0)
        .map(|(_, vk)| vk)
        .collect()
}

#[derive(Default)]
pub struct ButtonListener {
    running: Mutex<Option<Running>>,
    /// Not before this moment is a helper that failed started again.
    retry_after: Mutex<Option<Instant>>,
}

struct Running {
    /// Shared with the listener thread, so new assignments reach it without a restart.
    assignments: Arc<Mutex<Assignments>>,
    stop: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

impl Running {
    /// Asks the helper to stop and waits for it to restore the mouse.
    fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.thread.join();
    }

    fn matches(&self, desired: Option<&Assignments>) -> bool {
        let mine = self
            .assignments
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Some(&*mine) == desired && !self.thread.is_finished()
    }

    fn retarget(&self, wanted: &Assignments) -> bool {
        if self.thread.is_finished() {
            return false;
        }
        let mut mine = self
            .assignments
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if mine.brand != wanted.brand
            || mine.device != wanted.device
            || mine.require_host != wanted.require_host
        {
            return false;
        }
        if *mine != *wanted {
            *mine = wanted.clone();
            tracing::info!("Updated the button assignments");
        }
        true
    }
}

impl ButtonListener {
    /// Makes the helper match `desired`: started for new assignments, stopped
    /// when there are none. `may_start` is false while a control panel holds
    /// the mouse, so a new helper does not open it underneath. Stopping and
    /// starting wait on the helper, so they run off the caller's thread.
    pub fn update(self: &Arc<Self>, desired: Option<Assignments>, may_start: bool) {
        // New keys for a listener that is already running need no helper at
        // all, so they are applied at once even while a panel holds the mouse.
        // The caller is the async monitor loop, and a helper starting or stopping
        // holds `running` for seconds, so a busy lock means "ask again next tick".
        let Some(running) = self.try_running() else {
            return;
        };
        if let Some(wanted) = desired.as_ref()
            && running
                .as_ref()
                .is_some_and(|current| current.retarget(wanted))
        {
            return;
        }
        let in_step = running.as_ref().map_or(desired.is_none(), |current| {
            current.matches(desired.as_ref())
        });
        let holds_old = running.is_some();
        drop(running);
        if in_step {
            return;
        }
        // A panel holding the mouse keeps a new helper from starting. One for a profile
        // that is no longer the active one must not keep pressing keys meanwhile, so it
        // is stopped, and the new one starts once the panel lets go.
        let desired = if desired.is_some() && !may_start {
            if !holds_old {
                return;
            }
            None
        } else {
            desired
        };
        let waiting = *self
            .retry_after
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if desired.is_some() && waiting.is_some_and(|until| Instant::now() < until) {
            return;
        }
        let listener = Arc::clone(self);
        thread::spawn(move || listener.apply(desired));
    }

    /// The running helper, or `None` while another thread is starting or stopping one.
    fn try_running(&self) -> Option<std::sync::MutexGuard<'_, Option<Running>>> {
        match self.running.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    fn apply(&self, desired: Option<Assignments>) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if running.as_ref().map_or(desired.is_none(), |current| {
            current.matches(desired.as_ref())
        }) {
            return;
        }
        let died = running
            .as_ref()
            .is_some_and(|current| current.thread.is_finished());
        if let Some(old) = running.take() {
            old.stop();
        }
        // The lock is let go before the helper starts: starting can take seconds, and the
        // monitor loop reads this deadline to decide whether to ask again.
        *self
            .retry_after
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = died.then(|| Instant::now() + RETRY_DELAY);
        // A listener that just died waits out the cooldown before the mouse is probed again.
        if died && desired.is_some() {
            return;
        }
        if let Some(assignments) = desired {
            match start(assignments) {
                Ok(started) => *running = Some(started),
                Err(error) => {
                    tracing::warn!(%error, "Could not start the button listener");
                    *self
                        .retry_after
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) =
                        Some(Instant::now() + RETRY_DELAY);
                }
            }
        }
    }
}

impl Drop for ButtonListener {
    fn drop(&mut self) {
        let running = self
            .running
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = running.take() {
            old.stop();
        }
    }
}

enum Event {
    Buttons(u8),
    Ready,
    Failed(String),
    Closed,
}

fn start(assignments: Assignments) -> Result<Running> {
    // The helper talks to the mouse while it starts, and so does a profile
    // apply, so they take turns: this is held until the helper is listening.
    inject::prepare();
    let access = crate::drivers::device_access();
    let mut child = crate::drivers::spawn_button_listener(&assignments.brand)?;
    let stdin = child
        .stdin
        .take()
        .context("the button helper's stdin was unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("the button helper's stdout was unavailable")?;
    let (stop, stopped) = mpsc::channel();
    let (ready, started) = mpsc::channel();
    let shared = Arc::new(Mutex::new(assignments));
    let for_thread = Arc::clone(&shared);
    let thread = thread::Builder::new()
        .name("openmouse-bridge-buttons".into())
        .spawn(move || run(child, stdin, stdout, &for_thread, &stopped, &ready))
        .context("could not start the button listener thread")?;
    let _ = started.recv_timeout(START_TIMEOUT);
    drop(access);
    Ok(Running {
        assignments: shared,
        stop,
        thread,
    })
}

fn run(
    mut child: Child,
    mut stdin: ChildStdin,
    stdout: std::process::ChildStdout,
    shared: &Mutex<Assignments>,
    stopped: &mpsc::Receiver<()>,
    ready: &mpsc::Sender<()>,
) {
    let current = |shared: &Mutex<Assignments>| {
        shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    };
    let first = current(shared);
    let request = format!(
        "{{\"brand\":{},\"device\":{},\"requireHost\":{}}}\n",
        Value::from(first.brand.as_str()),
        Value::from(first.device.as_str()),
        first.require_host
    );
    if stdin
        .write_all(request.as_bytes())
        .and_then(|()| stdin.flush())
        .is_err()
    {
        tracing::warn!("Could not start the button helper");
    }

    let (events, incoming) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let message: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let event = match message.get("event").and_then(Value::as_str) {
                Some("buttons") => message
                    .get("mask")
                    .and_then(Value::as_u64)
                    .and_then(|mask| u8::try_from(mask).ok())
                    .map(Event::Buttons),
                Some("ready") => Some(Event::Ready),
                Some("error") => Some(Event::Failed(
                    message
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_owned(),
                )),
                _ => None,
            };
            if let Some(event) = event
                && events.send(event).is_err()
            {
                return;
            }
        }
        let _ = events.send(Event::Closed);
    });

    let mut held = 0u8;
    // What each held button does, from the moment it went down, so a key that was
    // pressed is the key that is released even if the profile changed while it was held.
    let mut in_use = HeldButtons::new();
    loop {
        if !matches!(stopped.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            break;
        }
        match incoming.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Buttons(mask)) => {
                for step in transitions(&mut in_use, &current(shared), held, mask) {
                    tracing::info!(mask, ?step, "Acting on a mouse button");
                    execute(step);
                }
                held = mask;
            }
            Ok(Event::Ready) => {
                tracing::info!("The mouse's buttons are being listened to");
                let _ = ready.send(());
            }
            Ok(Event::Failed(message)) => {
                tracing::warn!(%message, "The button helper reported a problem");
                let _ = ready.send(());
            }
            Ok(Event::Closed) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = ready.send(());
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    // Let go of anything still held, then close stdin: the helper stops the
    // events and puts the mouse back in the mode it was in.
    for step in transitions(&mut in_use, &current(shared), held, 0) {
        execute(step);
    }
    drop(stdin);
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    tracing::warn!("The button helper did not stop in time; ending it");
    // Collected as well: a killed child nobody waits for stays a zombie on Unix.
    if child.kill().is_ok() {
        let _ = child.wait();
    }
}

mod keymap;

#[cfg(target_os = "linux")]
#[path = "buttons/linux.rs"]
mod inject;

#[cfg(target_os = "macos")]
#[path = "buttons/macos.rs"]
mod inject;

/// No other system presses keys yet.
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
mod inject {
    use super::Step;

    pub fn prepare() {}

    pub fn execute(step: Step) {
        tracing::debug!(
            ?step,
            "Button actions are not carried out on this system yet"
        );
    }
}

fn execute(step: Step) {
    inject::execute(step);
}

#[cfg(target_os = "windows")]
mod inject {
    use std::{
        mem::size_of,
        sync::{Mutex, PoisonError},
    };

    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_XDOWN,
        MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW, SendInput,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{XBUTTON1, XBUTTON2};

    use super::{
        Action, BACK, HeldKeys, KeyAction, Step, media_virtual_key, modifier_keys, virtual_key,
    };

    static HELD: Mutex<HeldKeys> = Mutex::new(HeldKeys::new());

    /// `SendInput` needs no setup.
    pub fn prepare() {}

    pub fn execute(step: Step) {
        match step {
            Step::Down(Action::Key(action)) => keys(action, true),
            Step::Up(Action::Key(action)) => keys(action, false),
            Step::Down(Action::Media(usage)) => media(usage, true),
            Step::Up(Action::Media(usage)) => media(usage, false),
            Step::SideDown(bit) => side(bit, true),
            Step::SideUp(bit) => side(bit, false),
        }
    }

    /// Media keys have no scan code of their own, so they go by virtual key.
    fn media(usage: u16, down: bool) {
        let Some(vk) = media_virtual_key(usage) else {
            return;
        };
        // Two buttons on the same media key: it comes up with the last of them.
        let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        if !(if down {
            held.press(vk)
        } else {
            held.release(vk)
        }) {
            return;
        }
        let flags = KEYEVENTF_EXTENDEDKEY | if down { 0 } else { KEYEVENTF_KEYUP };
        send(&[INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }]);
    }

    /// Modifiers go down before the key and come up after it.
    fn keys(action: KeyAction, down: bool) {
        let Some(key) = virtual_key(action.usage) else {
            tracing::warn!(usage = action.usage, "That key cannot be pressed by Bridge");
            return;
        };
        let mut order = modifier_keys(action.modifiers);
        order.push(key);
        if !down {
            order.reverse();
        }
        let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        for vk in order {
            // A modifier two held buttons share stays down until both are let go.
            if if down {
                held.press(vk)
            } else {
                held.release(vk)
            } {
                send(&[key_input(vk, down)]);
            }
        }
    }

    fn key_input(vk: u16, down: bool) -> INPUT {
        // The scan code lets games that read it see the key; the high byte
        // 0xE0 marks an extended key (arrows, Insert, Home, ...).
        let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) };
        let mut flags = if down { 0 } else { KEYEVENTF_KEYUP };
        if scan != 0 {
            flags |= KEYEVENTF_SCANCODE;
        }
        if scan >> 8 == 0xe0 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: if scan != 0 { 0 } else { vk },
                    wScan: (scan & 0xff) as u16,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn side(bit: u8, down: bool) {
        let button = if bit == BACK { XBUTTON1 } else { XBUTTON2 };
        send(&[INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: u32::from(button),
                    dwFlags: if down {
                        MOUSEEVENTF_XDOWN
                    } else {
                        MOUSEEVENTF_XUP
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }]);
    }

    fn send(inputs: &[INPUT]) {
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<INPUT>() as i32,
            )
        };
        if sent as usize != inputs.len() {
            tracing::warn!("Windows did not accept the button input");
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::{ProfileApplication, ProfileDevice, ProfileSettings};

    fn profile(device: &str, snapshot: Value) -> ApplicationProfile {
        ApplicationProfile {
            application: ProfileApplication {
                name: "Game".into(),
                executable: String::new(),
                path: String::new(),
            },
            device: ProfileDevice {
                id: device.into(),
                name: "Mouse".into(),
            },
            settings: ProfileSettings {
                dpi: None,
                polling_rate_hz: None,
                snapshot: Some(snapshot),
            },
            enabled: true,
        }
    }

    fn assignments(keys: &[(u8, u8, u8)]) -> Assignments {
        Assignments {
            brand: "Logitech".into(),
            device: "Mouse".into(),
            actions: keys
                .iter()
                .map(|&(bit, usage, modifiers)| (bit, Action::Key(KeyAction { usage, modifiers })))
                .collect(),
            require_host: false,
        }
    }

    #[test]
    fn a_profile_names_the_mouse_it_is_for() {
        let parsed = Assignments::from_profile(
            &profile(
                "Logitech:PRO X 3 SUPERSTRIKE",
                json!({"hostButtons": {"16": {"key": 104, "modifiers": 0}}}),
            ),
            false,
        )
        .expect("assignments");
        assert_eq!(parsed.device, "PRO X 3 SUPERSTRIKE");
    }

    #[test]
    fn a_held_button_keeps_its_action_while_another_follows_the_new_profile() {
        let old = assignments(&[(BACK, 0x04, 0)]);
        let new = assignments(&[(BACK, 0x05, 0), (FRONT, 0x06, 0)]);
        let key = |usage| {
            Action::Key(KeyAction {
                usage,
                modifiers: 0,
            })
        };
        let mut held = HeldButtons::new();
        assert_eq!(
            transitions(&mut held, &old, 0, BACK),
            [Step::Down(key(0x04))]
        );
        // The profile changes while Back is held: Forward uses the new one, Back is let go as pressed.
        assert_eq!(
            transitions(&mut held, &new, BACK, BACK | FRONT),
            [Step::Down(key(0x06))]
        );
        assert_eq!(
            transitions(&mut held, &new, BACK | FRONT, FRONT),
            [Step::Up(key(0x04))]
        );
        assert_eq!(
            transitions(&mut held, &new, FRONT, 0),
            [Step::Up(key(0x06))]
        );
    }

    #[test]
    fn a_key_two_held_buttons_share_is_released_by_the_last_of_them() {
        let mut keys = HeldKeys::new();
        assert!(keys.press(0x11), "the first Ctrl goes down");
        assert!(!keys.press(0x11), "a second one is already down");
        assert!(!keys.release(0x11), "letting one go keeps Ctrl down");
        assert!(keys.release(0x11), "the last one releases it");
        assert!(keys.release(0x11), "a key nobody holds is released anyway");
    }

    #[test]
    fn a_profile_names_its_assignments_by_button_bit() {
        let parsed = Assignments::from_profile(&profile(
            "Logitech:PRO X3 SUPERSTRIKE",
            json!({"hostButtons": {"16": {"key": 104, "modifiers": 0}, "8": {"key": 4, "modifiers": 3}}}),
        ), false)
        .expect("assignments");
        assert_eq!(parsed.brand, "Logitech");
        assert_eq!(
            parsed.actions[&FRONT],
            Action::Key(KeyAction {
                usage: 104,
                modifiers: 0
            })
        );
        assert_eq!(
            parsed.actions[&BACK],
            Action::Key(KeyAction {
                usage: 4,
                modifiers: 3
            })
        );
    }

    #[test]
    fn a_profile_can_assign_a_media_key() {
        let parsed = Assignments::from_profile(
            &profile(
                "Logitech:Mouse",
                json!({"hostButtons": {"16": {"consumer": 233}}}),
            ),
            true,
        )
        .expect("assignments");
        assert_eq!(parsed.actions[&FRONT], Action::Media(233));
        assert!(
            parsed.require_host,
            "the default profile only listens to a mouse already on Host"
        );
        let unknown = Assignments::from_profile(
            &profile(
                "Logitech:Mouse",
                json!({"hostButtons": {"16": {"consumer": 1}}}),
            ),
            false,
        );
        assert_eq!(
            unknown, None,
            "a media key Bridge cannot press is not accepted"
        );
    }

    #[test]
    fn profiles_without_usable_assignments_start_nothing() {
        let logitech =
            |snapshot| Assignments::from_profile(&profile("Logitech:Mouse", snapshot), false);
        assert_eq!(logitech(json!({})), None);
        assert_eq!(logitech(json!({"hostButtons": {}})), None);
        assert_eq!(
            logitech(json!({"hostButtons": {"4": {"key": 4}}})),
            None,
            "the middle button is not assignable"
        );
        assert_eq!(logitech(json!({"hostButtons": {"8": {"key": 999}}})), None);
        assert_eq!(
            logitech(json!({"hostButtons": {"8": {"key": 0}}})),
            None,
            "a key Bridge cannot press would swallow the button's normal click"
        );
        assert_eq!(
            logitech(json!({"hostButtons": {"8": {"key": 0x46}}})).is_some(),
            !cfg!(target_os = "macos"),
            "Print Screen has no key on a Mac"
        );
        assert_eq!(
            logitech(json!({"hostButtons": {"8": {"key": 4, "modifiers": 99}}})),
            None
        );
        let other = profile("Razer:Viper", json!({"hostButtons": {"8": {"key": 4}}}));
        assert_eq!(
            Assignments::from_profile(&other, false),
            None,
            "only Logitech has the button events"
        );
    }

    #[test]
    fn a_press_and_release_become_one_key_down_and_up() {
        let assigned = assignments(&[(FRONT, 0x68, 0)]);
        let action = Action::Key(KeyAction {
            usage: 0x68,
            modifiers: 0,
        });
        assert_eq!(steps(&assigned, 0, FRONT), [Step::Down(action)]);
        assert_eq!(
            steps(&assigned, FRONT, FRONT),
            [],
            "holding changes nothing"
        );
        assert_eq!(steps(&assigned, FRONT, 0), [Step::Up(action)]);
    }

    #[test]
    fn an_unassigned_side_button_keeps_its_normal_click() {
        let assigned = assignments(&[(FRONT, 0x68, 0)]);
        assert_eq!(steps(&assigned, 0, BACK), [Step::SideDown(BACK)]);
        assert_eq!(steps(&assigned, BACK, 0), [Step::SideUp(BACK)]);
    }

    #[test]
    fn the_other_buttons_are_left_to_the_mouse() {
        let assigned = assignments(&[(FRONT, 0x68, 0)]);
        // left, right and middle already click on their own
        assert_eq!(steps(&assigned, 0, 0x01 | 0x02 | 0x04), []);
    }

    #[test]
    fn two_buttons_pressed_together_are_both_handled() {
        let assigned = assignments(&[(FRONT, 0x68, 0)]);
        assert_eq!(
            steps(&assigned, 0, BACK | FRONT),
            [
                Step::SideDown(BACK),
                Step::Down(Action::Key(KeyAction {
                    usage: 0x68,
                    modifiers: 0
                }))
            ]
        );
    }

    #[test]
    fn keys_map_to_windows_virtual_keys() {
        assert_eq!(virtual_key(0x04), Some(0x41), "A");
        assert_eq!(virtual_key(0x1d), Some(0x5a), "Z");
        assert_eq!(virtual_key(0x1e), Some(0x31), "1");
        assert_eq!(virtual_key(0x27), Some(0x30), "0");
        assert_eq!(virtual_key(0x3a), Some(0x70), "F1");
        assert_eq!(virtual_key(0x45), Some(0x7b), "F12");
        assert_eq!(virtual_key(0x68), Some(0x7c), "F13");
        assert_eq!(virtual_key(0x73), Some(0x87), "F24");
        assert_eq!(virtual_key(0x2c), Some(0x20), "Space");
        assert_eq!(virtual_key(0x52), Some(0x26), "Up");
        assert_eq!(virtual_key(0x01), None);
        assert_eq!(media_virtual_key(0xe9), Some(0xaf), "volume up");
        assert_eq!(media_virtual_key(0xcd), Some(0xb3), "play / pause");
        assert_eq!(media_virtual_key(0x01), None);
        assert_eq!(modifier_keys(0b0101), [0x11, 0x12], "ctrl and alt");
        assert_eq!(modifier_keys(0), Vec::<u16>::new());
    }
}
