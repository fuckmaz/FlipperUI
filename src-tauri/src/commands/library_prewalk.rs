//! Tauri command for the pre-scan directory size/density walk.
//!
//! Frontend flow: each library view calls `library_prewalk` first; if the
//! returned list is non-empty the user picks dirs to add to the persistent
//! exclusion list, then the real scan starts. See
//! [`crate::flipper::library_prewalk`] for the walker itself.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, State};

use crate::commands::client::with_connection;
use crate::commands::path::DevicePath;
use crate::error::Result;
use crate::flipper::library_prewalk::{self, DirStat};
use crate::operation::{require_cancelled, OperationName};
use crate::state::AppState;

/// Which library the prewalk is being run for. Used purely to route to the
/// existing per-library cancel flag — the prewalk itself is library-agnostic.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrewalkLibrary {
    Subghz,
    Infrared,
    Nfc,
    Rfid,
    Badusb,
}

#[derive(Serialize, Clone)]
pub struct PrewalkProgressEvent {
    #[serde(rename = "operationId")]
    operation_id: u64,
    visited: u32,
    current_path: String,
}

/// Walk `roots` recursively, returning only the directories that crossed the
/// entry-count or large-file thresholds. Emits `library-prewalk-progress`
/// events so the UI can show motion during slow (BLE) walks.
#[tauri::command(rename_all = "snake_case")]
pub async fn library_prewalk(
    library: PrewalkLibrary,
    roots: Vec<DevicePath>,
    excluded_dirs: Vec<DevicePath>,
    state: State<'_, AppState>,
    on_progress: Channel<PrewalkProgressEvent>,
) -> Result<Vec<DirStat>> {
    let _ = library;
    let operation = state.operations.begin(OperationName::LibraryPrewalk)?;
    let operation_id = operation.id();
    let cancelled = operation.cancel_token();
    let initial_path = roots.first().map(ToString::to_string).unwrap_or_default();
    let _ = on_progress.send(PrewalkProgressEvent {
        operation_id,
        visited: 0,
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
        prewalk_blocking(
            client,
            cancelled,
            operation_id,
            on_progress,
            roots,
            excluded_dirs,
        )
    })
    .await
}

#[tauri::command]
pub fn cancel_library_prewalk(operation_id: u64, state: State<AppState>) -> Result<()> {
    require_cancelled(
        state
            .operations
            .cancel(OperationName::LibraryPrewalk, operation_id),
    )
}

fn prewalk_blocking(
    client: &mut crate::flipper::client::FlipperClient,
    cancelled: Arc<AtomicBool>,
    operation_id: u64,
    on_progress: Channel<PrewalkProgressEvent>,
    roots: Vec<String>,
    excluded_dirs: Vec<String>,
) -> Result<Vec<DirStat>> {
    for root in &roots {
        crate::commands::path::validate_path(root)?;
    }
    let mut on_progress = |visited: u32, _total: u32, current: &str| {
        let _ = on_progress.send(PrewalkProgressEvent {
            operation_id,
            visited,
            current_path: current.to_string(),
        });
    };

    let root_refs: Vec<&str> = roots.iter().map(String::as_str).collect();
    let stats = library_prewalk::prewalk(
        client,
        &root_refs,
        &excluded_dirs,
        &cancelled,
        &mut on_progress,
    )?;
    Ok(library_prewalk::flagged(stats))
}
