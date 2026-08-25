//! Native HID access for the OpenMouse web app, so a browser without WebHID
//! (Firefox, Safari) can still drive a mouse through Bridge.
//!
//! The web app talks to `@openmouse/protocol`'s driver classes, and those are
//! written against WebHID's `HIDDevice`. Rather than reimplement any vendor
//! protocol here, this module exposes the same primitives WebHID does —
//! enumerate, open, send/receive reports, stream input reports — over the
//! loopback socket in `crate::api`, and the web app wraps them back into a
//! `navigator.hid` shim. The drivers never learn they are not in Chrome.
//!
//! Two behaviours here are hardware findings from this project's other native
//! HID adapters, not stylistic choices, and both are load-bearing:
//!
//! * The Generic Desktop mouse and keyboard collections are never opened.
//!   Chrome refuses to expose them to WebHID for the same reason: opening one
//!   freezes the device's own input on macOS. `descriptor::CollectionInfo::protected`
//!   is the check; on entry it runs against the enumerated usage before any
//!   handle is opened.
//! * One logical device is usually several enumerated entries ("splits"), one
//!   per top-level collection, and a given report id may only be answerable on
//!   one of them. Writes try every split and remember which one answered, as
//!   Desktop's `src-tauri/src/hid.rs` and `native-hid/src/hid-device-adapter.mjs`
//!   both had to learn.

pub mod descriptor;
pub mod socket;

use std::{
    collections::HashMap,
    ffi::CString,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use hidapi::{HidApi, HidDevice};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use descriptor::CollectionInfo;

/// How long a blocking read waits before the reader checks its stop flag.
const READ_POLL_TIMEOUT_MS: i32 = 200;
/// WebHID sizes a feature report from the parsed descriptor; hidapi needs the
/// caller to say. 64 covers every report the OpenMouse drivers ask for, and a
/// shorter reply is simply the leading bytes of it.
const FEATURE_REPORT_LENGTH: usize = 64;
/// Largest report descriptor HID allows.
const DESCRIPTOR_LENGTH: usize = 4096;

/// One logical device, in the shape the WebHID shim needs to build an
/// `HIDDevice` on the other side.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSummary {
    pub key: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub product_name: String,
    pub collections: Vec<CollectionInfo>,
}

/// An input report on its way to the browser.
pub struct InputReport {
    pub key: String,
    pub report_id: u8,
    pub data: Vec<u8>,
}

struct Split {
    device: Mutex<HidDevice>,
    stop: Arc<AtomicBool>,
}

struct OpenGroup {
    splits: Vec<Arc<Split>>,
    readers: Vec<JoinHandle<()>>,
    /// Which split answered a given report id last, so the next request for
    /// that id starts where it succeeded instead of re-probing from the top.
    routes: Mutex<HashMap<u8, usize>>,
}

impl Drop for OpenGroup {
    fn drop(&mut self) {
        for split in &self.splits {
            split.stop.store(true, Ordering::Relaxed);
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

/// Native HID for one connected browser socket. Dropping it closes every
/// device that socket opened.
pub struct HidSession {
    api: HidApi,
    /// Logical device key to the enumerated paths behind it.
    paths: HashMap<String, Vec<CString>>,
    /// Parsed collections per path. A descriptor never changes for a given
    /// path, and reading one costs an open, so it is read at most once.
    descriptors: HashMap<CString, Vec<CollectionInfo>>,
    open: HashMap<String, OpenGroup>,
    reports: UnboundedSender<InputReport>,
}

impl HidSession {
    pub fn new(reports: UnboundedSender<InputReport>) -> Result<Self> {
        Ok(Self {
            api: HidApi::new()?,
            paths: HashMap::new(),
            descriptors: HashMap::new(),
            open: HashMap::new(),
            reports,
        })
    }

    /// Every connectable device for the given vendor ids, with its collections.
    ///
    /// The vendor ids come from the web app's own `SUPPORTED_HID_FILTERS`, so
    /// the browser never learns about HID devices OpenMouse has no driver for
    /// — keyboards, security keys, and the rest stay invisible to the page.
    pub fn list(&mut self, vendor_ids: &[u16]) -> Result<Vec<DeviceSummary>> {
        self.api.refresh_devices()?;

        let mut groups: Vec<(String, Vec<CString>, u16, u16, String)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for info in self.api.device_list() {
            if !vendor_ids.contains(&info.vendor_id()) {
                continue;
            }
            // Checked before any handle is opened — see the module docs.
            let probe = CollectionInfo {
                usage_page: info.usage_page(),
                usage: info.usage(),
                ..CollectionInfo::default()
            };
            if probe.protected() {
                continue;
            }
            let key = format!(
                "{:04x}:{:04x}:{}",
                info.vendor_id(),
                info.product_id(),
                info.interface_number()
            );
            let path = info.path().to_owned();
            match index.get(&key) {
                Some(position) => {
                    let paths: &mut Vec<CString> = &mut groups[*position].1;
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
                None => {
                    index.insert(key.clone(), groups.len());
                    groups.push((
                        key,
                        vec![path],
                        info.vendor_id(),
                        info.product_id(),
                        info.product_string().unwrap_or_default().to_string(),
                    ));
                }
            }
        }

        let mut summaries = Vec::new();
        self.paths.clear();
        for (key, paths, vendor_id, product_id, product_name) in groups {
            let mut collections = Vec::new();
            for path in &paths {
                collections.extend(
                    self.collections_for(path)
                        .into_iter()
                        .filter(|collection| !collection.protected()),
                );
            }
            // Nothing a page may touch: not a device as far as WebHID is
            // concerned, so do not advertise it.
            if collections.is_empty() {
                continue;
            }
            self.paths.insert(key.clone(), paths);
            summaries.push(DeviceSummary {
                key,
                vendor_id,
                product_id,
                product_name,
                collections,
            });
        }
        Ok(summaries)
    }

    /// Reads and parses one path's report descriptor, caching the result.
    /// A path that cannot be opened or read contributes no collections rather
    /// than failing the whole enumeration — one busy interface must not hide
    /// every other mouse on the system.
    fn collections_for(&mut self, path: &CString) -> Vec<CollectionInfo> {
        if let Some(cached) = self.descriptors.get(path) {
            return cached.clone();
        }
        let parsed = self
            .api
            .open_path(path)
            .ok()
            .and_then(|device| {
                let mut buffer = vec![0u8; DESCRIPTOR_LENGTH];
                let length = device.get_report_descriptor(&mut buffer).ok()?;
                buffer.truncate(length);
                Some(descriptor::parse(&buffer))
            })
            .unwrap_or_default();
        self.descriptors.insert(path.clone(), parsed.clone());
        parsed
    }

    pub fn open(&mut self, key: &str) -> Result<()> {
        if self.open.contains_key(key) {
            return Ok(());
        }
        let paths = self
            .paths
            .get(key)
            .ok_or_else(|| anyhow!("{key} is not a known device; list devices first"))?
            .clone();

        let mut splits = Vec::new();
        let mut readers = Vec::new();
        let mut last_error = None;
        for path in &paths {
            match self.api.open_path(path) {
                Ok(device) => {
                    let split = Arc::new(Split {
                        device: Mutex::new(device),
                        stop: Arc::new(AtomicBool::new(false)),
                    });
                    readers.push(spawn_reader(
                        key.to_string(),
                        split.clone(),
                        self.reports.clone(),
                    ));
                    splits.push(split);
                }
                Err(error) => last_error = Some(error),
            }
        }
        if splits.is_empty() {
            bail!(
                "could not open {key}: {}",
                last_error
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "no interface answered".into())
            );
        }
        self.open.insert(
            key.to_string(),
            OpenGroup {
                splits,
                readers,
                routes: Mutex::new(HashMap::new()),
            },
        );
        Ok(())
    }

    pub fn close(&mut self, key: &str) {
        self.open.remove(key);
    }

    pub fn is_open(&self, key: &str) -> bool {
        self.open.contains_key(key)
    }

    pub fn send_report(&self, key: &str, report_id: u8, data: &[u8]) -> Result<()> {
        let frame = frame(report_id, data);
        self.try_each(key, report_id, |device| device.write(&frame).map(|_| ()))
    }

    pub fn send_feature_report(&self, key: &str, report_id: u8, data: &[u8]) -> Result<()> {
        let frame = frame(report_id, data);
        self.try_each(key, report_id, |device| device.send_feature_report(&frame))
    }

    pub fn receive_feature_report(&self, key: &str, report_id: u8) -> Result<Vec<u8>> {
        let mut bytes = self.try_each(key, report_id, |device| {
            let mut buffer = vec![0u8; FEATURE_REPORT_LENGTH];
            buffer[0] = report_id;
            let length = device.get_feature_report(&mut buffer)?;
            buffer.truncate(length);
            Ok(buffer)
        })?;
        // WebHID's DataView starts after the report id; hidapi includes it.
        if !bytes.is_empty() && bytes[0] == report_id {
            bytes.remove(0);
        }
        Ok(bytes)
    }

    fn group(&self, key: &str) -> Result<&OpenGroup> {
        self.open
            .get(key)
            .ok_or_else(|| anyhow!("{key} is not open"))
    }

    /// Runs an operation against whichever split accepts it, starting with the
    /// one that answered this report id last time.
    fn try_each<T>(
        &self,
        key: &str,
        report_id: u8,
        mut operation: impl FnMut(&HidDevice) -> hidapi::HidResult<T>,
    ) -> Result<T> {
        let group = self.group(key)?;
        let hinted = group.routes.lock().unwrap().get(&report_id).copied();
        let order: Vec<usize> = match hinted {
            Some(hint) if hint < group.splits.len() => std::iter::once(hint)
                .chain((0..group.splits.len()).filter(|position| *position != hint))
                .collect(),
            _ => (0..group.splits.len()).collect(),
        };

        let mut last_error = None;
        for position in order {
            let device = group.splits[position].device.lock().unwrap();
            match operation(&device) {
                Ok(value) => {
                    group.routes.lock().unwrap().insert(report_id, position);
                    return Ok(value);
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(anyhow!(
            "no interface of {key} accepted report 0x{report_id:02x}: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "no interface is open".into())
        ))
    }
}

/// The report id leads the write buffer, as the OS HID stack expects for a
/// numbered report — and as a zero byte when the device declares none.
fn frame(report_id: u8, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(data.len() + 1);
    frame.push(report_id);
    frame.extend_from_slice(data);
    frame
}

fn spawn_reader(
    key: String,
    split: Arc<Split>,
    reports: UnboundedSender<InputReport>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0u8; FEATURE_REPORT_LENGTH];
        while !split.stop.load(Ordering::Relaxed) {
            // try_lock, plus an unconditional sleep outside the lock. A
            // request/reply exchange must never queue behind this poll, and
            // CONFIRMED on real hardware in Desktop's src-tauri/src/hid.rs: a
            // bare try_lock is not enough on its own, because read_timeout
            // holds the guard for its full duration and this loop would
            // immediately re-acquire it, starving a writer parked on lock()
            // indefinitely. The sleep is what leaves a gap for the writer.
            let read = match split.device.try_lock() {
                Ok(device) => device.read_timeout(&mut buffer, READ_POLL_TIMEOUT_MS),
                Err(_) => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };
            thread::sleep(Duration::from_millis(5));
            match read {
                Ok(0) => continue,
                Ok(length) => {
                    // Some backends prefix a numbered report with its id and
                    // some do not; there is no reliable way to tell from the
                    // bytes alone. Both other adapters in this project assume
                    // the prefix, and the drivers match replies on payload
                    // content rather than strictly on report id.
                    let report = InputReport {
                        key: key.clone(),
                        report_id: buffer[0],
                        data: buffer[1..length].to_vec(),
                    };
                    if reports.send(report).is_err() {
                        return;
                    }
                }
                // A disconnect surfaces as a read error. The pending request's
                // own timeout is what reports it, exactly as a WebHID device
                // going away does; nothing useful to do here but stop.
                Err(_) => return,
            }
        }
    })
}
