use std::sync::Arc;

use tauri::{ipc::Channel, State};

use crate::commands::client::with_connection;
use crate::commands::library_scan::{run_library_scan, ScanProgressEvent};
use crate::commands::path::DevicePath;
use crate::error::Result;
use crate::flipper::nfc::{self, NfcEntry};
use crate::operation::{require_cancelled, OperationName};
use crate::state::AppState;

/// Recursively scan a directory for `.nfc` files and parse their headers.
/// Emits `nfc-scan-progress` events as it works.
#[tauri::command(rename_all = "snake_case")]
pub async fn nfc_scan(
    root: DevicePath,
    excluded_dirs: Vec<DevicePath>,
    cached: Option<Vec<NfcEntry>>,
    state: State<'_, AppState>,
    on_progress: Channel<ScanProgressEvent>,
) -> Result<Vec<NfcEntry>> {
    let operation = state.operations.begin(OperationName::NfcScan)?;
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
                nfc::scan_library(
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

#[tauri::command]
pub fn nfc_cancel_scan(operation_id: u64, state: State<AppState>) -> Result<()> {
    require_cancelled(
        state
            .operations
            .cancel(OperationName::NfcScan, operation_id),
    )
}

/// Parse a specific list of `.nfc` paths without walking the library.
/// Used by the upload-completion path to incrementally merge freshly-written
/// files into the library view without a full rescan of `/ext/nfc`.
#[tauri::command(rename_all = "snake_case")]
pub async fn nfc_parse_paths(
    paths: Vec<DevicePath>,
    state: State<'_, AppState>,
) -> Result<Vec<NfcEntry>> {
    let paths = paths
        .into_iter()
        .map(DevicePath::into_string)
        .collect::<Vec<_>>();
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        nfc::parse_paths(client, &paths)
    })
    .await
}
