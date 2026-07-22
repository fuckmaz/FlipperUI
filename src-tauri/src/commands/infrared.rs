use std::sync::Arc;

use tauri::{ipc::Channel, State};

use crate::commands::client::with_connection;
use crate::commands::library_scan::{run_library_scan, ScanProgressEvent};
use crate::commands::path::DevicePath;
use crate::error::Result;
use crate::flipper::infrared::{self, IrEntry};
use crate::operation::{require_cancelled, OperationName};
use crate::state::AppState;

/// Recursively scan a directory for .ir files and parse their signal blocks.
/// Emits `infrared-scan-progress` events as it works.
#[tauri::command(rename_all = "snake_case")]
pub async fn infrared_scan(
    root: DevicePath,
    excluded_dirs: Vec<DevicePath>,
    cached: Option<Vec<IrEntry>>,
    state: State<'_, AppState>,
    on_progress: Channel<ScanProgressEvent>,
) -> Result<Vec<IrEntry>> {
    let operation = state.operations.begin(OperationName::InfraredScan)?;
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
                infrared::scan_library(
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
pub fn infrared_cancel_scan(operation_id: u64, state: State<AppState>) -> Result<()> {
    require_cancelled(
        state
            .operations
            .cancel(OperationName::InfraredScan, operation_id),
    )
}
