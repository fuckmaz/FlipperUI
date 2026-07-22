use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use tauri::{ipc::Channel, State};

use crate::commands::client::with_connection;
use crate::commands::library_scan::ScanProgressEvent;
use crate::commands::path::{validate_path, DevicePath};
use crate::error::{FlipperError, Result};
use crate::flipper::apps::{self, AppEntry};
use crate::flipper::{fap_icon, storage};
use crate::operation::{require_cancelled, OperationName};
use crate::state::AppState;

/// Scan one or more roots for `.fap` files and return a parsed list.
/// Emits `apps-scan-progress` events as it works.
#[tauri::command(rename_all = "snake_case")]
pub async fn apps_scan(
    roots: Vec<DevicePath>,
    excluded_dirs: Vec<DevicePath>,
    cached: Option<Vec<AppEntry>>,
    state: State<'_, AppState>,
    on_progress: Channel<ScanProgressEvent>,
) -> Result<Vec<AppEntry>> {
    let operation = state.operations.begin(OperationName::AppsScan)?;
    let operation_id = operation.id();
    let cancelled = operation.cancel_token();
    let initial_path = roots.first().map(ToString::to_string).unwrap_or_default();
    let _ = on_progress.send(ScanProgressEvent {
        operation_id,
        scanned: 0,
        total: 0,
        current_path: initial_path,
    });
    let roots = roots
        .into_iter()
        .map(DevicePath::into_string)
        .collect::<Vec<_>>();
    let excluded_dirs = excluded_dirs
        .into_iter()
        .map(DevicePath::into_string)
        .collect::<Vec<_>>();
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let _operation = operation;
        for r in &roots {
            validate_path(r)?;
        }

        let cached_map: HashMap<String, AppEntry> = cached
            .unwrap_or_default()
            .into_iter()
            .map(|entry| {
                let path = DevicePath::try_from(entry.path.as_str())?.into_string();
                DevicePath::try_from(entry.root.as_str())?;
                Ok((path, entry))
            })
            .collect::<Result<_>>()?;

        let mut on_progress = |scanned: u32, total: u32, current: &str| {
            let _ = on_progress.send(ScanProgressEvent {
                operation_id,
                scanned,
                total,
                current_path: current.to_string(),
            });
        };

        apps::scan_library(
            client,
            &roots,
            &excluded_dirs,
            &cached_map,
            &cancelled,
            &mut on_progress,
        )
    })
    .await
}

#[tauri::command]
pub fn apps_cancel_scan(operation_id: u64, state: State<AppState>) -> Result<()> {
    require_cancelled(
        state
            .operations
            .cancel(OperationName::AppsScan, operation_id),
    )
}

/// Parse a specific list of `.fap` paths without walking the library.
/// Used by the upload-completion path to incrementally merge freshly-installed
/// apps into the library view without a full rescan.
#[tauri::command(rename_all = "snake_case")]
pub async fn apps_parse_paths(
    paths: Vec<DevicePath>,
    roots: Vec<DevicePath>,
    state: State<'_, AppState>,
) -> Result<Vec<AppEntry>> {
    let paths = paths
        .into_iter()
        .map(DevicePath::into_string)
        .collect::<Vec<_>>();
    let roots = roots
        .into_iter()
        .map(DevicePath::into_string)
        .collect::<Vec<_>>();
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        for r in &roots {
            validate_path(r)?;
        }
        apps::parse_paths(client, &paths, &roots)
    })
    .await
}

/// Read a `.fap` and extract its embedded 10x10 icon, returned as
/// base64-encoded raw XBM bytes (32-byte icon slot; only the first 20 are
/// the 10x10 bitmap, the rest is padding).
///
/// Returns `Ok(None)` when the file has no embedded icon (or the manifest
/// can't be located) — the UI then falls back to the placeholder glyph.
#[tauri::command(rename_all = "snake_case")]
pub async fn apps_read_icon(
    path: DevicePath,
    state: State<'_, AppState>,
) -> Result<Option<String>> {
    if !path.to_lowercase().ends_with(".fap") {
        return Err(FlipperError::Session("Not a .fap file".into()));
    }

    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let bytes = storage::storage_read(client, &path, |_, _| {}, || false)?;
        let icon = fap_icon::extract(&bytes)
            .map(|d| base64::engine::general_purpose::STANDARD.encode(d.icon));
        Ok(icon)
    })
    .await
}
