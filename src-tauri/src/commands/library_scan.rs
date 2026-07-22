use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde::Serialize;
use tauri::ipc::Channel;

use crate::commands::path::{validate_path, DevicePath};
use crate::error::Result;
use crate::flipper::client::FlipperClient;

#[derive(Serialize, Clone)]
pub struct ScanProgressEvent {
    #[serde(rename = "operationId")]
    pub operation_id: u64,
    pub scanned: u32,
    pub total: u32,
    pub current_path: String,
}

/// Runs the shared pre-scan boilerplate for every per-library Tauri command
/// (subghz / infrared / nfc / badusb / apps / future libraries):
///
/// 1. Validates every root path.
/// 2. Builds a `HashMap` of cached entries keyed by `key_of`.
/// 3. Wraps `app.emit(progress_event, …)` in an `FnMut` the library walker can call.
/// 4. Delegates to `scan`, which owns the library-specific walk.
///
/// The caller runs this inside one connection-actor RPC job, so the actor owns
/// the client and enforces RPC mode before this helper starts.
#[allow(clippy::too_many_arguments)]
pub fn run_library_scan<E, F>(
    client: &mut FlipperClient,
    cancelled: Arc<AtomicBool>,
    operation_id: u64,
    on_progress: Channel<ScanProgressEvent>,
    roots: &[&str],
    cached: Option<Vec<E>>,
    key_of: fn(&E) -> String,
    scan: F,
) -> Result<Vec<E>>
where
    F: FnOnce(
        &mut FlipperClient,
        &HashMap<String, E>,
        &Arc<AtomicBool>,
        &mut dyn FnMut(u32, u32, &str),
    ) -> Result<Vec<E>>,
{
    for root in roots {
        validate_path(root)?;
    }
    let cached_map: HashMap<String, E> = cached
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            let key = DevicePath::try_from(key_of(&entry))?.into_string();
            Ok((key, entry))
        })
        .collect::<Result<_>>()?;

    let mut report_progress = |scanned: u32, total: u32, current: &str| {
        let _ = on_progress.send(ScanProgressEvent {
            operation_id,
            scanned,
            total,
            current_path: current.to_string(),
        });
    };

    scan(client, &cached_map, &cancelled, &mut report_progress)
}
