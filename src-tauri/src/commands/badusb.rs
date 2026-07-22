use std::sync::Arc;

use tauri::{ipc::Channel, State};

use crate::commands::client::with_connection;
use crate::commands::library_scan::{run_library_scan, ScanProgressEvent};
use crate::commands::path::DevicePath;
use crate::error::Result;
use crate::flipper::badusb::{self, BadUsbEntry};
use crate::operation::{require_cancelled, OperationName};
use crate::state::AppState;

/// Recursively scan `/ext/badusb` and `/ext/badkb` for `.txt` Duckyscript
/// files, parse their line counts + leading comments, and return the combined
/// list. Emits `badusb-scan-progress` events as it works.
#[tauri::command(rename_all = "snake_case")]
pub async fn badusb_scan(
    usb_root: DevicePath,
    kb_root: DevicePath,
    excluded_dirs: Vec<DevicePath>,
    cached: Option<Vec<BadUsbEntry>>,
    state: State<'_, AppState>,
    on_progress: Channel<ScanProgressEvent>,
) -> Result<Vec<BadUsbEntry>> {
    let operation = state.operations.begin(OperationName::BadusbScan)?;
    let operation_id = operation.id();
    let cancelled = operation.cancel_token();
    let _ = on_progress.send(ScanProgressEvent {
        operation_id,
        scanned: 0,
        total: 0,
        current_path: usb_root.to_string(),
    });
    let usb_root = usb_root.into_string();
    let kb_root = kb_root.into_string();
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
            &[&usb_root, &kb_root],
            cached,
            |e| e.path.clone(),
            |client, cached_map, cancelled, on_progress| {
                let roots: &[(&str, &str)] =
                    &[(usb_root.as_str(), "usb"), (kb_root.as_str(), "kb")];
                badusb::scan_library(
                    client,
                    roots,
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
pub fn badusb_cancel_scan(operation_id: u64, state: State<AppState>) -> Result<()> {
    require_cancelled(
        state
            .operations
            .cancel(OperationName::BadusbScan, operation_id),
    )
}

/// Parse a specific list of BadUSB / BadKB `.txt` paths
#[tauri::command(rename_all = "snake_case")]
pub async fn badusb_parse_paths(
    paths: Vec<DevicePath>,
    state: State<'_, AppState>,
) -> Result<Vec<BadUsbEntry>> {
    let paths = paths
        .into_iter()
        .map(DevicePath::into_string)
        .collect::<Vec<_>>();
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        badusb::parse_paths(client, &paths)
    })
    .await
}
