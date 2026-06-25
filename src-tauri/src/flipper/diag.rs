use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::pb;

/// Keep the last N frames so the log can't grow without bound while the app
/// runs with diagnostics on. 500 entries covers roughly 15 s of screen stream
/// traffic, which is long enough to investigate a misbehaving exchange.
const RING_CAP: usize = 500;

static ENABLED: AtomicBool = AtomicBool::new(false);
static BUFFER: OnceLock<Mutex<VecDeque<DiagEntry>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Direction {
    Tx,
    Rx,
    Event,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagEntry {
    pub ts_ms: u64,
    pub dir: Direction,
    pub command_id: u32,
    pub command_status: i32,
    pub command_status_name: String,
    pub has_next: bool,
    pub content_kind: String,
    pub detail: String,
    pub payload_bytes: usize,
}

fn buffer() -> &'static Mutex<VecDeque<DiagEntry>> {
    BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(RING_CAP)))
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if !on {
        clear();
    }
}

pub fn clear() {
    if let Ok(mut b) = buffer().lock() {
        b.clear();
    }
}

pub fn snapshot() -> Vec<DiagEntry> {
    buffer()
        .lock()
        .map(|b| b.iter().cloned().collect())
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Log a frame. Called from the framing layer; a no-op when disabled. The
/// enabled check is cheap (atomic load) so leaving the call sites in hot paths
/// is fine.
pub fn log(dir: Direction, msg: &pb::Main, payload_bytes: usize) {
    if !is_enabled() {
        return;
    }
    let content_kind = content_kind(msg);
    let entry = DiagEntry {
        ts_ms: now_ms(),
        dir,
        command_id: msg.command_id,
        command_status: msg.command_status,
        command_status_name: status_name(msg.command_status),
        has_next: msg.has_next,
        content_kind,
        detail: content_detail(msg).unwrap_or_default(),
        payload_bytes,
    };
    push(entry);
}

/// Log a higher-level RPC event, such as transfer chunk sizing or teardown.
pub fn log_event(kind: impl Into<String>, detail: impl Into<String>) {
    if !is_enabled() {
        return;
    }
    let entry = DiagEntry {
        ts_ms: now_ms(),
        dir: Direction::Event,
        command_id: 0,
        command_status: 0,
        command_status_name: "OK".into(),
        has_next: false,
        content_kind: kind.into(),
        detail: detail.into(),
        payload_bytes: 0,
    };
    push(entry);
}

fn push(entry: DiagEntry) {
    if let Ok(mut b) = buffer().lock() {
        if b.len() == RING_CAP {
            b.pop_front();
        }
        b.push_back(entry);
    }
}

fn status_name(status: i32) -> String {
    pb::CommandStatus::try_from(status)
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|_| format!("Unknown({status})"))
}

/// Short human label for the Content variant, e.g. "StorageListRequest".
/// Derived from the Debug repr so it automatically tracks new protobuf
/// variants as the `.proto` files evolve.
fn content_kind(msg: &pb::Main) -> String {
    let Some(c) = &msg.content else {
        return String::new();
    };
    let dbg = format!("{c:?}");
    // Debug renders as e.g. `StorageListRequest(StorageListRequest { ... })` —
    // take the identifier before the first `(`.
    match dbg.find('(') {
        Some(i) => dbg[..i].to_string(),
        None => dbg,
    }
}

fn content_detail(msg: &pb::Main) -> Option<String> {
    use pb::main::Content;

    match msg.content.as_ref()? {
        Content::StorageWriteRequest(r) => {
            let data_bytes = r.file.as_ref().map(|f| f.data.len()).unwrap_or(0);
            Some(format!("path={} data_bytes={data_bytes}", compact(&r.path)))
        }
        Content::StorageReadRequest(r) => Some(format!("path={}", compact(&r.path))),
        Content::StorageReadResponse(r) => r.file.as_ref().map(file_detail),
        Content::StorageListRequest(r) => Some(format!(
            "path={} include_md5={} filter_max_size={}",
            compact(&r.path),
            r.include_md5,
            r.filter_max_size
        )),
        Content::StorageListResponse(r) => Some(format!("files={}", r.file.len())),
        Content::StorageStatRequest(r) => Some(format!("path={}", compact(&r.path))),
        Content::StorageStatResponse(r) => r.file.as_ref().map(file_detail),
        Content::StorageInfoRequest(r) => Some(format!("path={}", compact(&r.path))),
        Content::StorageInfoResponse(r) => Some(format!(
            "total_space={} free_space={}",
            r.total_space, r.free_space
        )),
        Content::StorageDeleteRequest(r) => Some(format!(
            "path={} recursive={}",
            compact(&r.path),
            r.recursive
        )),
        Content::StorageMkdirRequest(r) => Some(format!("path={}", compact(&r.path))),
        Content::StorageRenameRequest(r) => Some(format!(
            "old_path={} new_path={}",
            compact(&r.old_path),
            compact(&r.new_path)
        )),
        Content::StorageTimestampRequest(r) => Some(format!("path={}", compact(&r.path))),
        Content::StorageTimestampResponse(r) => Some(format!("timestamp={}", r.timestamp)),
        Content::StorageMd5sumRequest(r) => Some(format!("path={}", compact(&r.path))),
        Content::StorageMd5sumResponse(r) => Some(format!("md5={}", r.md5sum)),
        Content::SystemPingRequest(r) => Some(format!("data_bytes={}", r.data.len())),
        Content::SystemPingResponse(r) => Some(format!("data_bytes={}", r.data.len())),
        Content::SystemDeviceInfoResponse(r) => Some(format!("key={}", compact(&r.key))),
        Content::GuiScreenFrame(r) => Some(format!(
            "data_bytes={} orientation={}",
            r.data.len(),
            r.orientation
        )),
        Content::GuiSendInputEventRequest(r) => {
            Some(format!("key={} type={}", input_key_name(r.key), input_type_name(r.r#type)))
        }
        _ => None,
    }
}

fn input_key_name(key: i32) -> String {
    match key {
        0 => "UP".into(),
        1 => "DOWN".into(),
        2 => "RIGHT".into(),
        3 => "LEFT".into(),
        4 => "OK".into(),
        5 => "BACK".into(),
        other => format!("Key({other})"),
    }
}

fn input_type_name(ty: i32) -> String {
    match ty {
        0 => "PRESS".into(),
        1 => "RELEASE".into(),
        2 => "SHORT".into(),
        3 => "LONG".into(),
        4 => "REPEAT".into(),
        other => format!("Type({other})"),
    }
}

fn file_detail(file: &crate::pb_storage::File) -> String {
    format!(
        "name={} type={} size={} data_bytes={}",
        compact(&file.name),
        file.r#type,
        file.size,
        file.data.len()
    )
}

fn compact(value: &str) -> String {
    const MAX: usize = 120;
    if value.chars().count() <= MAX {
        return value.to_string();
    }
    format!("{}...", value.chars().take(MAX).collect::<String>())
}
