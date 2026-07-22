use std::sync::Arc;

use tauri::{ipc::Channel, State};

use crate::commands::client::with_connection;
use crate::commands::library_scan::{run_library_scan, ScanProgressEvent};
use crate::commands::path::DevicePath;
use crate::error::Result;
use crate::flipper::subghz::{self, SubGhzEntry};
use crate::operation::{require_cancelled, OperationName};
use crate::state::AppState;

/// Recursively scan a directory for .sub files and parse their headers.
///
/// `cached` is an optional list of previously-parsed entries (with mtime)
/// from the frontend's on-disk cache. When supplied, files whose mtime
/// hasn't moved are reused from cache instead of being re-read over serial.
///
/// Emits `subghz-scan-progress` events with `{ scanned, total, current_path }`
/// after each file. Returns the full list once the walk completes (or
/// `TransferCancelled` if the frontend called [`subghz_cancel_scan`]).
#[tauri::command(rename_all = "snake_case")]
pub async fn subghz_scan(
    root: DevicePath,
    excluded_dirs: Vec<DevicePath>,
    cached: Option<Vec<SubGhzEntry>>,
    state: State<'_, AppState>,
    on_progress: Channel<ScanProgressEvent>,
) -> Result<Vec<SubGhzEntry>> {
    let operation = state.operations.begin(OperationName::SubghzScan)?;
    let operation_id = operation.id();
    let cancelled = operation.cancel_token();
    let _ = on_progress.send(ScanProgressEvent {
        operation_id,
        scanned: 0,
        total: 0,
        current_path: root.to_string(),
    });
    let root = root.into_string();
    let excluded_dirs = excluded_dirs
        .into_iter()
        .map(DevicePath::into_string)
        .collect::<Vec<_>>();
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let _operation = operation;
        run_library_scan(
            client,
            cancelled,
            operation_id,
            on_progress,
            &[&root],
            cached,
            |e| e.path.clone(),
            |client, cached_map, cancelled, on_progress| {
                subghz::scan_library(
                    client,
                    &root,
                    &excluded_dirs,
                    cached_map,
                    cancelled,
                    on_progress,
                )
            },
        )
    })
    .await
}

/// Abort an in-progress SubGhz library scan.
/// The scan loop checks the flag between files and returns `TransferCancelled`.
#[tauri::command]
pub fn subghz_cancel_scan(operation_id: u64, state: State<AppState>) -> Result<()> {
    require_cancelled(
        state
            .operations
            .cancel(OperationName::SubghzScan, operation_id),
    )
}
