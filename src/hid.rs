use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use axum::extract::ws::{Message, WebSocket};
use hidapi::{HidApi, HidDevice, MAX_REPORT_DESCRIPTOR_SIZE};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

const MAX_REPORT_BYTES: usize = MAX_REPORT_DESCRIPTOR_SIZE;
const READ_TIMEOUT_MS: i32 = 100;
const RAZER_VENDOR_ID: u16 = 0x1532;
const RAZER_FEATURE_BUFFER_LEN: usize = 91;

#[derive(Debug, Deserialize)]
struct ClientMessage {
    id: u64,
    #[serde(flatten)]
    command: Command,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Command {
    List {
        #[serde(rename = "vendorIds")]
        vendor_ids: Vec<u16>,
    },
    Open {
        device: String,
    },
    Close {
        device: String,
    },
    Listen {
        device: String,
    },
    Unlisten {
        device: String,
    },
    SendReport {
        device: String,
        #[serde(rename = "reportId")]
        report_id: u8,
        data: Vec<u8>,
    },
    SendFeatureReport {
        device: String,
        #[serde(rename = "reportId")]
        report_id: u8,
        data: Vec<u8>,
    },
    ReceiveFeatureReport {
        device: String,
        #[serde(rename = "reportId")]
        report_id: u8,
    },
}

#[derive(Debug, Serialize)]
struct Reply {
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    devices: Option<Vec<DeviceSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Vec<u8>>,
}

impl Reply {
    fn ok(id: u64) -> Self {
        Self {
            id,
            ok: true,
            error: None,
            devices: None,
            data: None,
        }
    }

    fn error(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            error: Some(error.into()),
            devices: None,
            data: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct InputReport {
    #[serde(rename = "type")]
    kind: &'static str,
    device: String,
    #[serde(rename = "reportId")]
    report_id: u8,
    data: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSummary {
    key: String,
    vendor_id: u16,
    product_id: u16,
    product_name: String,
    collections: Vec<CollectionInfo>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectionInfo {
    usage_page: u16,
    usage: u16,
    input_reports: Vec<ReportInfo>,
    output_reports: Vec<ReportInfo>,
    feature_reports: Vec<ReportInfo>,
    children: Vec<CollectionInfo>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportInfo {
    report_id: u8,
    items: Vec<ReportItem>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportItem {
    report_size: u32,
    report_count: u32,
}

#[derive(Clone, Debug, Default)]
struct ReportLayout {
    feature_bits: HashMap<u8, usize>,
    input_bits: HashMap<u8, usize>,
}

impl ReportLayout {
    fn feature_buffer_len(&self, report_id: u8) -> Result<usize, String> {
        let bits = self.feature_bits.get(&report_id).copied().ok_or_else(|| {
            format!("feature report {report_id} is not declared by this interface")
        })?;
        Ok(bits
            .div_ceil(8)
            .saturating_add(1)
            .clamp(2, MAX_REPORT_BYTES))
    }

    fn input_buffer_len(&self) -> usize {
        self.input_bits
            .values()
            .copied()
            .max()
            .unwrap_or(8 * 64)
            .div_ceil(8)
            .saturating_add(1)
            .clamp(2, MAX_REPORT_BYTES)
    }

    fn input_uses_report_ids(&self) -> bool {
        self.input_bits.keys().any(|report_id| *report_id != 0)
    }
}

#[derive(Clone)]
struct CandidatePath {
    path: CString,
    layout: ReportLayout,
}

#[derive(Clone)]
struct Candidate {
    paths: Vec<CandidatePath>,
    summary: DeviceSummary,
}

struct OpenPath {
    device: Arc<Mutex<HidDevice>>,
    layout: ReportLayout,
}

struct OpenDevice {
    paths: Vec<OpenPath>,
    vendor_id: u16,
    listening: bool,
    stop: Option<Arc<AtomicBool>>,
    readers: Vec<thread::JoinHandle<()>>,
}

impl OpenDevice {
    fn stop_reader(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Release);
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        self.listening = false;
    }
}

impl Drop for OpenDevice {
    fn drop(&mut self) {
        self.stop_reader();
    }
}

struct HidSession {
    api: HidApi,
    next_key: u64,
    device_keys: HashMap<Vec<u8>, String>,
    candidates: HashMap<String, Candidate>,
    open_devices: HashMap<String, OpenDevice>,
    listeners: HashSet<String>,
    input_tx: mpsc::UnboundedSender<InputReport>,
}

impl HidSession {
    fn new(input_tx: mpsc::UnboundedSender<InputReport>) -> Result<Self, String> {
        Ok(Self {
            api: HidApi::new()
                .map_err(|error| format!("could not initialize native HID: {error}"))?,
            next_key: 1,
            device_keys: HashMap::new(),
            candidates: HashMap::new(),
            open_devices: HashMap::new(),
            listeners: HashSet::new(),
            input_tx,
        })
    }

    fn execute(&mut self, message: ClientMessage) -> Reply {
        let id = message.id;
        let outcome = match message.command {
            Command::List { vendor_ids } => self.list(vendor_ids).map(|devices| Reply {
                devices: Some(devices),
                ..Reply::ok(id)
            }),
            Command::Open { device } => self.open(&device).map(|()| Reply::ok(id)),
            Command::Close { device } => self.close(&device).map(|()| Reply::ok(id)),
            Command::Listen { device } => self.listen(&device).map(|()| Reply::ok(id)),
            Command::Unlisten { device } => self.unlisten(&device).map(|()| Reply::ok(id)),
            Command::SendReport {
                device,
                report_id,
                data,
            } => self
                .send_report(&device, report_id, data, false)
                .map(|()| Reply::ok(id)),
            Command::SendFeatureReport {
                device,
                report_id,
                data,
            } => self
                .send_report(&device, report_id, data, true)
                .map(|()| Reply::ok(id)),
            Command::ReceiveFeatureReport { device, report_id } => self
                .receive_feature_report(&device, report_id)
                .map(|data| Reply {
                    data: Some(data),
                    ..Reply::ok(id)
                }),
        };
        outcome.unwrap_or_else(|error| Reply::error(id, error))
    }

    fn list(&mut self, vendor_ids: Vec<u16>) -> Result<Vec<DeviceSummary>, String> {
        if vendor_ids.len() > 256 {
            return Err("too many HID vendor ids were requested".into());
        }
        let vendors: HashSet<u16> = vendor_ids.into_iter().collect();
        if vendors.is_empty() {
            return Ok(Vec::new());
        }
        self.api
            .refresh_devices()
            .map_err(|error| format!("could not refresh HID devices: {error}"))?;

        let mut by_device = HashMap::<Vec<u8>, Vec<hidapi::DeviceInfo>>::new();
        for info in self.api.device_list() {
            if vendors.contains(&info.vendor_id()) {
                by_device
                    .entry(device_group_key(info))
                    .or_default()
                    .push(info.clone());
            }
        }

        let mut summaries = Vec::with_capacity(by_device.len());
        for (device_id, infos) in by_device {
            let key = if let Some(key) = self.device_keys.get(&device_id) {
                key.clone()
            } else {
                let key = format!("hid-{}", self.next_key);
                self.next_key += 1;
                self.device_keys.insert(device_id, key.clone());
                key
            };
            if !self.candidates.contains_key(&key) {
                let candidate = inspect_candidate(&self.api, key.clone(), &infos)?;
                self.candidates.insert(key.clone(), candidate);
            }
            if let Some(candidate) = self.candidates.get(&key) {
                summaries.push(candidate.summary.clone());
            }
        }
        summaries.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(summaries)
    }

    fn open(&mut self, key: &str) -> Result<(), String> {
        if self.open_devices.contains_key(key) {
            return Ok(());
        }
        let candidate = self
            .candidates
            .get(key)
            .ok_or_else(|| "the selected HID interface is no longer available".to_owned())?;
        let mut paths = Vec::with_capacity(candidate.paths.len());
        let mut errors = Vec::new();
        for candidate_path in &candidate.paths {
            match self.api.open_path(&candidate_path.path) {
                Ok(device) => paths.push(OpenPath {
                    device: Arc::new(Mutex::new(device)),
                    layout: candidate_path.layout.clone(),
                }),
                Err(error) => errors.push(error.to_string()),
            }
        }
        if paths.is_empty() {
            return Err(format!(
                "could not open the HID interface: {}",
                errors.join("; ")
            ));
        }
        self.open_devices.insert(
            key.to_owned(),
            OpenDevice {
                paths,
                vendor_id: candidate.summary.vendor_id,
                listening: false,
                stop: None,
                readers: Vec::new(),
            },
        );
        if self.listeners.contains(key) {
            self.start_reader(key)?;
        }
        Ok(())
    }

    fn close(&mut self, key: &str) -> Result<(), String> {
        let Some(mut open) = self.open_devices.remove(key) else {
            return Ok(());
        };
        open.stop_reader();
        Ok(())
    }

    fn listen(&mut self, key: &str) -> Result<(), String> {
        if !self.candidates.contains_key(key) {
            return Err("the selected HID interface is no longer available".into());
        }
        self.listeners.insert(key.to_owned());
        if self.open_devices.contains_key(key) {
            self.start_reader(key)?;
        }
        Ok(())
    }

    fn start_reader(&mut self, key: &str) -> Result<(), String> {
        let open = self
            .open_devices
            .get_mut(key)
            .ok_or_else(|| "the HID interface must be open before listening".to_owned())?;
        if open.listening {
            return Ok(());
        }
        let stop = Arc::new(AtomicBool::new(false));
        for (index, path) in open.paths.iter().enumerate() {
            let device = Arc::clone(&path.device);
            let buffer_len = path.layout.input_buffer_len();
            let uses_report_ids = path.layout.input_uses_report_ids();
            let thread_stop = Arc::clone(&stop);
            let tx = self.input_tx.clone();
            let device_key = key.to_owned();
            let reader = thread::Builder::new()
                .name(format!("openmouse-hid-{key}-{index}"))
                .spawn(move || {
                    let mut buffer = vec![0; buffer_len];
                    while !thread_stop.load(Ordering::Acquire) {
                        let result = device
                            .lock()
                            .map_err(|_| "native HID lock was poisoned".to_owned())
                            .and_then(|device| {
                                device
                                    .read_timeout(&mut buffer, READ_TIMEOUT_MS)
                                    .map_err(|error| error.to_string())
                            });
                        match result {
                            Ok(0) => {}
                            Ok(size) => {
                                let (report_id, data) = if uses_report_ids {
                                    (buffer[0], buffer[1..size].to_vec())
                                } else {
                                    (0, buffer[..size].to_vec())
                                };
                                if tx
                                    .send(InputReport {
                                        kind: "inputreport",
                                        device: device_key.clone(),
                                        report_id,
                                        data,
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(error) => {
                                tracing::warn!(
                                    device = %device_key,
                                    interface = index,
                                    %error,
                                    "Native HID input reader stopped"
                                );
                                break;
                            }
                        }
                    }
                })
                .map_err(|error| format!("could not start the HID input reader: {error}"))?;
            open.readers.push(reader);
        }
        open.listening = true;
        open.stop = Some(stop);
        Ok(())
    }

    fn unlisten(&mut self, key: &str) -> Result<(), String> {
        self.listeners.remove(key);
        if let Some(open) = self.open_devices.get_mut(key) {
            open.stop_reader();
        }
        Ok(())
    }

    fn send_report(
        &self,
        key: &str,
        report_id: u8,
        data: Vec<u8>,
        feature: bool,
    ) -> Result<(), String> {
        if data.len() >= MAX_REPORT_BYTES {
            return Err("HID report exceeds the native descriptor limit".into());
        }
        let open = self
            .open_devices
            .get(key)
            .ok_or_else(|| "the HID interface is not open".to_owned())?;
        let mut frame = Vec::with_capacity(data.len() + 1);
        frame.push(report_id);
        frame.extend_from_slice(&data);
        let mut sent = false;
        let mut last_error = None;
        for path in &open.paths {
            let result = path
                .device
                .lock()
                .map_err(|_| "native HID lock was poisoned".to_owned())
                .and_then(|device| {
                    if feature {
                        device.send_feature_report(&frame).map(|_| ())
                    } else {
                        device.write(&frame).map(|_| ())
                    }
                    .map_err(|error| error.to_string())
                });
            match result {
                Ok(()) => sent = true,
                Err(error) => last_error = Some(error),
            }
        }
        if sent {
            Ok(())
        } else {
            let operation = if feature { "feature" } else { "output" };
            Err(format!(
                "could not send {operation} report {report_id}: {}",
                last_error.unwrap_or_else(|| "no HID path was open".to_owned())
            ))
        }
    }

    fn receive_feature_report(&self, key: &str, report_id: u8) -> Result<Vec<u8>, String> {
        let open = self
            .open_devices
            .get(key)
            .ok_or_else(|| "the HID interface is not open".to_owned())?;
        let mut available = false;
        let mut fallback_response = None;
        let mut last_error = None;
        for path in &open.paths {
            let buffer_len = match path.layout.feature_buffer_len(report_id) {
                Ok(length) => length,
                Err(_) if open.vendor_id == RAZER_VENDOR_ID => RAZER_FEATURE_BUFFER_LEN,
                Err(_) => continue,
            };
            available = true;
            let mut data = vec![0; buffer_len];
            data[0] = report_id;
            let result = path
                .device
                .lock()
                .map_err(|_| "native HID lock was poisoned".to_owned())
                .and_then(|device| {
                    device
                        .get_feature_report(&mut data)
                        .map_err(|error| error.to_string())
                });
            match result {
                Ok(size) => {
                    let response = if size == 0 {
                        Vec::new()
                    } else {
                        data[1..size].to_vec()
                    };
                    if open.vendor_id != RAZER_VENDOR_ID || response.iter().any(|byte| *byte != 0) {
                        return Ok(response);
                    }
                    fallback_response.get_or_insert(response);
                }
                Err(error) => last_error = Some(error),
            }
        }
        if let Some(response) = fallback_response {
            return Ok(response);
        }
        if !available {
            return Err(format!(
                "feature report {report_id} is not declared by this interface"
            ));
        }
        Err(format!(
            "could not receive feature report {report_id}: {}",
            last_error.unwrap_or_else(|| "no HID path was open".to_owned())
        ))
    }
}

fn razer_group_key(product_id: u16, serial: Option<&str>) -> Vec<u8> {
    format!("razer:{product_id:04x}:{}", serial.unwrap_or("no-serial")).into_bytes()
}

fn device_group_key(info: &hidapi::DeviceInfo) -> Vec<u8> {
    if info.vendor_id() == RAZER_VENDOR_ID {
        return razer_group_key(
            info.product_id(),
            info.serial_number().filter(|serial| !serial.is_empty()),
        );
    }
    info.path().to_bytes().to_vec()
}

fn inspect_candidate(
    api: &HidApi,
    key: String,
    infos: &[hidapi::DeviceInfo],
) -> Result<Candidate, String> {
    let primary = infos
        .first()
        .ok_or_else(|| "HID enumeration returned an empty interface".to_owned())?;
    let mut unique_paths = Vec::new();
    let mut seen_paths = HashSet::new();
    for info in infos {
        if seen_paths.insert(info.path().to_bytes().to_vec()) {
            unique_paths.push(info.path().to_owned());
        }
    }

    let inspect_descriptors = primary.vendor_id() != RAZER_VENDOR_ID;
    let mut collections = Vec::new();
    let mut paths = Vec::with_capacity(unique_paths.len());
    for path in unique_paths {
        let parsed = inspect_descriptors
            .then(|| {
                api.open_path(&path).ok().and_then(|device| {
                    let mut descriptor = vec![0; MAX_REPORT_DESCRIPTOR_SIZE];
                    let size = device.get_report_descriptor(&mut descriptor).ok()?;
                    parse_report_descriptor(&descriptor[..size]).ok()
                })
            })
            .flatten();
        let layout = if let Some((mut path_collections, path_layout)) = parsed {
            collections.append(&mut path_collections);
            path_layout
        } else {
            ReportLayout::default()
        };
        paths.push(CandidatePath { path, layout });
    }
    if collections.is_empty() {
        collections = infos
            .iter()
            .map(|info| CollectionInfo {
                usage_page: info.usage_page(),
                usage: info.usage(),
                ..CollectionInfo::default()
            })
            .collect();
    }

    Ok(Candidate {
        paths,
        summary: DeviceSummary {
            key,
            vendor_id: primary.vendor_id(),
            product_id: primary.product_id(),
            product_name: infos
                .iter()
                .find_map(|info| info.product_string().filter(|name| !name.is_empty()))
                .unwrap_or("HID device")
                .to_owned(),
            collections,
        },
    })
}

#[derive(Clone, Default)]
struct GlobalState {
    usage_page: u16,
    report_id: u8,
    report_size: u32,
    report_count: u32,
}

#[derive(Default)]
struct CollectionNode {
    info: CollectionInfo,
    parent: Option<usize>,
}

fn parse_report_descriptor(bytes: &[u8]) -> Result<(Vec<CollectionInfo>, ReportLayout), String> {
    let mut globals = GlobalState::default();
    let mut global_stack = Vec::new();
    let mut usages = Vec::<u32>::new();
    let mut nodes = Vec::<CollectionNode>::new();
    let mut collections = Vec::<usize>::new();
    let mut roots = Vec::<usize>::new();
    let mut layout = ReportLayout::default();
    let mut offset = 0;

    while offset < bytes.len() {
        let prefix = bytes[offset];
        offset += 1;
        if prefix == 0xfe {
            if offset + 2 > bytes.len() {
                return Err("truncated long HID item".into());
            }
            let size = bytes[offset] as usize;
            offset += 2;
            if offset + size > bytes.len() {
                return Err("truncated long HID item payload".into());
            }
            offset += size;
            continue;
        }

        let size = match prefix & 0x03 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        if offset + size > bytes.len() {
            return Err("truncated HID item payload".into());
        }
        let data = &bytes[offset..offset + size];
        offset += size;
        let value = item_value(data);
        let item_type = (prefix >> 2) & 0x03;
        let tag = prefix >> 4;

        match (item_type, tag) {
            (1, 0) => globals.usage_page = value as u16,
            (1, 7) => globals.report_size = value,
            (1, 8) => globals.report_id = value as u8,
            (1, 9) => globals.report_count = value,
            (1, 10) => global_stack.push(globals.clone()),
            (1, 11) => {
                globals = global_stack
                    .pop()
                    .ok_or_else(|| "HID descriptor popped an empty global stack".to_owned())?;
            }
            (2, 0) => usages.push(if size == 4 && value > u16::MAX as u32 {
                value
            } else {
                (u32::from(globals.usage_page) << 16) | (value & 0xffff)
            }),
            (0, 10) => {
                let usage = usages.first().copied().unwrap_or(0);
                let index = nodes.len();
                let parent = collections.last().copied();
                nodes.push(CollectionNode {
                    info: CollectionInfo {
                        usage_page: (usage >> 16) as u16,
                        usage: usage as u16,
                        ..CollectionInfo::default()
                    },
                    parent,
                });
                if parent.is_none() {
                    roots.push(index);
                }
                collections.push(index);
                usages.clear();
            }
            (0, 12) => {
                collections
                    .pop()
                    .ok_or_else(|| "HID descriptor closed an unopened collection".to_owned())?;
                usages.clear();
            }
            (0, 8 | 9 | 11) => {
                if let Some(collection) = collections.last().copied() {
                    let item = ReportItem {
                        report_size: globals.report_size,
                        report_count: globals.report_count,
                    };
                    let reports = match tag {
                        8 => &mut nodes[collection].info.input_reports,
                        9 => &mut nodes[collection].info.output_reports,
                        _ => &mut nodes[collection].info.feature_reports,
                    };
                    append_report(reports, globals.report_id, item);
                    let bits = usize::try_from(globals.report_size)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(
                            usize::try_from(globals.report_count).unwrap_or(usize::MAX),
                        );
                    let totals = match tag {
                        8 => &mut layout.input_bits,
                        11 => &mut layout.feature_bits,
                        _ => {
                            usages.clear();
                            continue;
                        }
                    };
                    *totals.entry(globals.report_id).or_default() = totals
                        .get(&globals.report_id)
                        .copied()
                        .unwrap_or_default()
                        .saturating_add(bits);
                }
                usages.clear();
            }
            (0, _) => usages.clear(),
            _ => {}
        }
    }

    if !collections.is_empty() {
        return Err("HID descriptor ended inside a collection".into());
    }
    Ok((
        roots
            .into_iter()
            .map(|index| build_collection(index, &nodes))
            .collect(),
        layout,
    ))
}

fn item_value(bytes: &[u8]) -> u32 {
    bytes.iter().enumerate().fold(0, |value, (shift, byte)| {
        value | (u32::from(*byte) << (shift * 8))
    })
}

fn append_report(reports: &mut Vec<ReportInfo>, report_id: u8, item: ReportItem) {
    if let Some(report) = reports
        .iter_mut()
        .find(|report| report.report_id == report_id)
    {
        report.items.push(item);
    } else {
        reports.push(ReportInfo {
            report_id,
            items: vec![item],
        });
    }
}

fn build_collection(index: usize, nodes: &[CollectionNode]) -> CollectionInfo {
    let mut info = nodes[index].info.clone();
    info.children = nodes
        .iter()
        .enumerate()
        .filter_map(|(child, node)| (node.parent == Some(index)).then_some(child))
        .map(|child| build_collection(child, nodes))
        .collect();
    info
}

pub async fn serve(mut socket: WebSocket) {
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let session = match HidSession::new(input_tx) {
        Ok(session) => Arc::new(Mutex::new(session)),
        Err(error) => {
            let _ = send_json(&mut socket, &Reply::error(0, error)).await;
            return;
        }
    };

    loop {
        tokio::select! {
            message = socket.recv() => {
                let Some(Ok(message)) = message else { break };
                let Message::Text(text) = message else {
                    if matches!(message, Message::Close(_)) { break; }
                    continue;
                };
                let request = match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(request) => request,
                    Err(error) => {
                        if send_json(&mut socket, &Reply::error(0, format!("invalid HID command: {error}"))).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let worker = Arc::clone(&session);
                let reply = match tokio::task::spawn_blocking(move || {
                    worker
                        .lock()
                        .map_err(|_| "native HID session lock was poisoned".to_owned())
                        .map(|mut session| session.execute(request))
                }).await {
                    Ok(Ok(reply)) => reply,
                    Ok(Err(error)) => Reply::error(0, error),
                    Err(error) => Reply::error(0, format!("native HID worker stopped: {error}")),
                };
                if send_json(&mut socket, &reply).await.is_err() { break; }
            }
            report = input_rx.recv() => {
                let Some(report) = report else { break };
                if send_json(&mut socket, &report).await.is_err() { break; }
            }
        }
    }

    drop(session);
}

async fn send_json(socket: &mut WebSocket, value: &impl Serialize) -> Result<(), axum::Error> {
    let json = serde_json::to_string(value).expect("serializing a fixed HID response cannot fail");
    socket.send(Message::Text(json.into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_becomes_webhid_collections_and_report_lengths() {
        let descriptor = [
            0x05, 0x01, // Usage Page (Generic Desktop)
            0x09, 0x02, // Usage (Mouse)
            0xa1, 0x01, // Collection (Application)
            0x85, 0x08, // Report ID 8
            0x75, 0x08, // Report Size 8
            0x95, 0x10, // Report Count 16
            0x81, 0x02, // Input
            0x91, 0x02, // Output
            0x85, 0x05, // Report ID 5
            0x95, 0x40, // Report Count 64
            0xb1, 0x02, // Feature
            0xc0, // End Collection
        ];
        let (collections, layout) = parse_report_descriptor(&descriptor).unwrap();
        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].usage_page, 1);
        assert_eq!(collections[0].usage, 2);
        assert_eq!(collections[0].input_reports[0].report_id, 8);
        assert_eq!(collections[0].input_reports[0].items[0].report_count, 16);
        assert_eq!(collections[0].output_reports[0].report_id, 8);
        assert_eq!(collections[0].feature_reports[0].report_id, 5);
        assert_eq!(layout.feature_buffer_len(5).unwrap(), 65);
        assert!(layout.input_uses_report_ids());
    }

    #[test]
    fn razer_interfaces_share_one_transport_identity() {
        assert_eq!(
            razer_group_key(0x00b0, Some("receiver-123")),
            razer_group_key(0x00b0, Some("receiver-123"))
        );
        assert_eq!(razer_group_key(0x00b0, None), razer_group_key(0x00b0, None));
        assert_ne!(
            razer_group_key(0x00b0, Some("receiver-123")),
            razer_group_key(0x00b0, Some("receiver-456"))
        );
        assert_ne!(razer_group_key(0x00b0, None), razer_group_key(0x00b1, None));
    }

    #[test]
    fn descriptor_preserves_nested_collections() {
        let descriptor = [
            0x06, 0x00, 0xff, // Usage Page 0xff00
            0x09, 0x01, 0xa1, 0x01, // top-level collection
            0x09, 0x02, 0xa1, 0x02, // child collection
            0x75, 0x01, 0x95, 0x08, 0x81, 0x02, // eight one-bit inputs
            0xc0, 0xc0,
        ];
        let (collections, _) = parse_report_descriptor(&descriptor).unwrap();
        assert_eq!(collections[0].usage_page, 0xff00);
        assert_eq!(collections[0].children.len(), 1);
        assert_eq!(collections[0].children[0].usage, 2);
        assert_eq!(
            collections[0].children[0].input_reports[0].items[0].report_size,
            1
        );
    }
}
