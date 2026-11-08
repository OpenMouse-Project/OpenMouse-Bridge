//! Which window has focus on KDE Plasma's Wayland session (issue #59).
//!
//! Wayland gives apps no way to see the focused window, so the X11 lookup in
//! `applications` finds nothing for native Wayland windows. KWin can run
//! scripts that call D-Bus, though: Bridge loads a small one that reports
//! every focus change to a method Bridge serves on the session bus, and
//! unloads it when Bridge stops. Calls from anyone but KWin are ignored.
//!
//! Window titles are deliberately not sent: the PID is all profile matching
//! needs, and titles can carry private text.

use std::{
    path::PathBuf,
    sync::{Mutex, PoisonError},
    thread,
};

use anyhow::{Context, Result};
use zbus::{
    blocking::{Connection, connection, fdo::DBusProxy},
    interface,
    message::Header,
    names::BusName,
};

const BUS_NAME: &str = "io.openmouse.Bridge";
const OBJECT_PATH: &str = "/Focus";
const PLUGIN_NAME: &str = "openmouse-bridge-focus";
const KWIN: &str = "org.kde.KWin";

/// The KWin script. `windowActivated` and `activeWindow` are KWin 6 names, so
/// this needs Plasma 6; on Plasma 5 the script fails to run and Bridge keeps
/// using X11 alone.
const SCRIPT: &str = r#"// OpenMouse Bridge: reports which window has focus, for game and app profiles.
// Loaded and unloaded by Bridge; safe to ignore.
function report(window) {
    callDBus("io.openmouse.Bridge", "/Focus", "io.openmouse.Bridge.Focus",
             "WindowActivated",
             window ? window.pid : 0,
             window ? String(window.resourceClass) : "",
             window ? String(window.desktopFileName) : "");
}
workspace.windowActivated.connect(report);
report(workspace.activeWindow);
"#;

#[derive(Default)]
struct Shared {
    /// KWin's unique bus name; only its calls are believed.
    kwin_owner: Option<String>,
    /// Whether the script is loaded and reporting.
    reporting: bool,
    /// The focused window's process, as KWin last reported it.
    focused_pid: Option<u32>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    kwin_owner: None,
    reporting: false,
    focused_pid: None,
});

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The focused window's PID as KWin reports it. `None` when KWin is not
/// reporting, so the caller falls back to X11; `Some(None)` when it reports
/// no focused window, or one whose PID it does not know.
pub fn focused_pid() -> Option<Option<u32>> {
    let shared = shared();
    shared.reporting.then_some(shared.focused_pid)
}

/// The script's D-Bus target.
struct Focus;

#[interface(name = "io.openmouse.Bridge.Focus")]
impl Focus {
    #[zbus(name = "WindowActivated")]
    fn window_activated(
        &self,
        #[zbus(header)] header: Header<'_>,
        pid: i32,
        resource_class: String,
        desktop_file_name: String,
    ) {
        let sender = header.sender().map(ToString::to_string);
        let mut shared = shared();
        if sender.is_none() || sender != shared.kwin_owner {
            tracing::debug!(?sender, "ignored a focus report not sent by KWin");
            return;
        }
        shared.focused_pid = u32::try_from(pid).ok().filter(|pid| *pid != 0);
        tracing::debug!(
            pid,
            resource_class,
            desktop_file_name,
            "KWin reported the focused window"
        );
    }
}

/// Keeps the KWin script loaded; dropping it unloads the script.
pub struct FocusWatch {
    connection: Connection,
}

impl Drop for FocusWatch {
    fn drop(&mut self) {
        shared().reporting = false;
        unload_script(&self.connection);
    }
}

/// Starts reporting focus through KWin when this is a Wayland session with
/// KWin on the bus. `None` otherwise, or when anything fails: the X11
/// lookup stays the only source then.
pub fn start() -> Option<FocusWatch> {
    std::env::var_os("WAYLAND_DISPLAY")?;
    match try_start() {
        Ok(watch) => Some(watch),
        Err(error) => {
            tracing::info!(%error, "KWin focus reporting is unavailable; using X11 only");
            None
        }
    }
}

fn try_start() -> Result<FocusWatch> {
    let connection = connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Focus)?
        .build()
        .context("could not serve the focus reporter on the session bus")?;
    let dbus = DBusProxy::new(&connection)?;
    // KWin restarting (a crash, `kwin_wayland --replace`) drops the script,
    // so it is loaded again into the new KWin. The subscription comes before
    // the lookup and the first load: a restart in between is then still seen.
    let changes = dbus
        .receive_name_owner_changed_with_args(&[(0, KWIN)])
        .context("could not watch KWin on the session bus")?;
    let owner = dbus
        .get_name_owner(BusName::try_from(KWIN)?)
        .context("KWin is not on the session bus")?;
    shared().kwin_owner = Some(owner.to_string());
    load_script(&connection)?;
    tracing::info!("KWin reports the focused window to Bridge");

    let watcher = connection.clone();
    thread::Builder::new()
        .name("openmouse-bridge-kwin-focus".into())
        .spawn(move || watch_kwin(&watcher, changes))
        .context("could not start the KWin watcher")?;
    Ok(FocusWatch { connection })
}

fn watch_kwin(connection: &Connection, changes: zbus::blocking::fdo::NameOwnerChangedIterator) {
    for change in changes {
        let Ok(args) = change.args() else {
            continue;
        };
        let owner = args.new_owner().as_ref().map(ToString::to_string);
        {
            let mut shared = shared();
            shared.kwin_owner = owner.clone();
            shared.focused_pid = None;
            if owner.is_none() {
                shared.reporting = false;
            }
        }
        if owner.is_some() {
            match load_script(connection) {
                Ok(()) => tracing::info!("KWin restarted; focus reporting resumed"),
                Err(error) => tracing::warn!(%error, "could not reload the KWin focus script"),
            }
        }
    }
}

/// Writes the script where KWin can read it, replaces any copy a previous
/// Bridge left loaded, and runs it.
fn load_script(connection: &Connection) -> Result<()> {
    let path = script_path()?;
    std::fs::write(&path, SCRIPT).with_context(|| format!("could not write {}", path.display()))?;
    unload_script(connection);
    let id: i32 = connection
        .call_method(
            Some(KWIN),
            "/Scripting",
            Some("org.kde.kwin.Scripting"),
            "loadScript",
            &(path.to_string_lossy().as_ref(), PLUGIN_NAME),
        )
        .context("KWin would not load the focus script")?
        .body()
        .deserialize()?;
    anyhow::ensure!(id >= 0, "KWin refused the focus script");
    connection
        .call_method(
            Some(KWIN),
            format!("/Scripting/Script{id}").as_str(),
            Some("org.kde.kwin.Script"),
            "run",
            &(),
        )
        .context("KWin would not run the focus script")?;
    shared().reporting = true;
    Ok(())
}

fn unload_script(connection: &Connection) {
    let _ = connection.call_method(
        Some(KWIN),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "unloadScript",
        &(PLUGIN_NAME,),
    );
}

fn script_path() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().context("could not find the cache directory")?;
    let dir = dirs.cache_dir().join("openmouse-bridge");
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir.join("kwin-focus.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_calls_the_method_bridge_serves() {
        assert!(SCRIPT.contains(&format!("\"{BUS_NAME}\", \"{OBJECT_PATH}\"")));
        assert!(SCRIPT.contains("\"io.openmouse.Bridge.Focus\""));
        assert!(SCRIPT.contains("\"WindowActivated\""));
        // No window titles leave KWin.
        assert!(!SCRIPT.contains("caption"));
    }
}
