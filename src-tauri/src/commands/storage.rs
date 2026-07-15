use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::commands::client::{is_fatal_transport_error, with_client};
use crate::commands::path::validate_path;
use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::diag;
use crate::flipper::library_walk;
use crate::flipper::storage;
use crate::pb_storage;
use crate::state::{AppState, ConnectionMode};

fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn begin_transfer(generation: &AtomicU64) -> u64 {
    generation.fetch_add(1, Ordering::Relaxed) + 1
}

fn transfer_cancelled(cancelled_generation: &AtomicU64, generation: u64) -> bool {
    cancelled_generation.load(Ordering::Relaxed) == generation
}

fn upload_progress_pct(sent: usize, total: usize) -> u32 {
    if total == 0 {
        return 100;
    }
    if sent >= total {
        return 99;
    }
    ((sent * 100 / total) as u32).clamp(1, 99)
}

fn with_transfer_client<T>(
    mode_mutex: &Arc<Mutex<ConnectionMode>>,
    client_mutex: &Arc<Mutex<Option<FlipperClient>>>,
    ble_cancel_tx: &Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    app: &AppHandle,
    f: impl FnOnce(&mut FlipperClient) -> Result<T>,
) -> Result<T> {
    let result = with_client(mode_mutex, client_mutex, f);
    if let Err(e) = &result {
        if is_fatal_transfer_error(e) {
            tracing::warn!("tearing down connection after transfer RPC failure: {e}");
            diag::log_event("TransferConnectionTornDown", e.to_string());
            if let Ok(mut guard) = client_mutex.lock() {
                *guard = None;
            }
            if let Ok(mut tx_guard) = ble_cancel_tx.lock() {
                if let Some(tx) = tx_guard.take() {
                    let _ = tx.send(());
                }
            }
            let _ = app.emit("flipper-disconnected", e.to_string());
        }
    }
    result
}

fn is_fatal_transfer_error(e: &FlipperError) -> bool {
    match e {
        FlipperError::Serial(_) => true,
        FlipperError::Io(io) => !matches!(
            io.kind(),
            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
        ),
        FlipperError::Decode(_) | FlipperError::Encode(_) => true,
        _ => false,
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = temp_path_for(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        FlipperError::Io(e)
    })
}

fn temp_path_for(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| ".flipperui-download".into());
    name.push(".part");
    path.with_file_name(name)
}

/// Mirror of pb_storage::File for the frontend, with base64-encoded data.
#[derive(Serialize, Deserialize)]
pub struct FileEntry {
    /// 0 = file, 1 = directory
    pub file_type: i32,
    pub name: String,
    pub size: u32,
    pub md5sum: String,
}

impl From<pb_storage::File> for FileEntry {
    fn from(f: pb_storage::File) -> Self {
        FileEntry {
            file_type: f.r#type,
            name: f.name,
            size: f.size,
            md5sum: f.md5sum,
        }
    }
}

#[tauri::command]
pub async fn storage_list(path: String, state: State<'_, AppState>) -> Result<Vec<FileEntry>> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            storage::storage_list(c, &path)
                .map(|files| files.into_iter().map(FileEntry::from).collect())
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn storage_stat(path: String, state: State<'_, AppState>) -> Result<FileEntry> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            storage::storage_stat(c, &path).map(FileEntry::from)
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Read a file from the Flipper. Returns base64-encoded bytes to avoid
/// JSON number-array overhead for large files.
/// Emits `"download-progress"` events (u32 0–100) to the frontend after each chunk.
#[tauri::command]
pub async fn storage_read(
    path: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<String> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let generation = begin_transfer(&state.transfer_generation);
    let cancelled_generation = Arc::clone(&state.transfer_cancelled_generation);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            let data = storage::storage_read(
                c,
                &path,
                |received, total| {
                    let pct = (received * 100)
                        .checked_div(total)
                        .map(|v| v as u32)
                        .unwrap_or(0);
                    let _ = app.emit("download-progress", pct);
                },
                || transfer_cancelled(&cancelled_generation, generation),
            )?;
            Ok(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &data,
            ))
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Write a file to the Flipper. `data` is base64-encoded.
/// Emits `"upload-progress"` events (u32 0–100) to the frontend after each chunk.
#[tauri::command]
pub async fn storage_write(
    path: String,
    data: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let ble_cancel_tx = Arc::clone(&state.ble_cancel_tx);
    let generation = begin_transfer(&state.transfer_generation);
    let cancelled_generation = Arc::clone(&state.transfer_cancelled_generation);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
            .map_err(|e| FlipperError::Session(format!("base64 decode error: {e}")))?;

        let app_for_progress = app.clone();
        let result = with_transfer_client(&mode_mutex, &client_mutex, &ble_cancel_tx, &app, |c| {
            storage::storage_write(
                c,
                &path,
                &bytes,
                |sent, total| {
                    let pct = upload_progress_pct(sent, total);
                    let _ = app_for_progress.emit("upload-progress", pct);
                },
                || transfer_cancelled(&cancelled_generation, generation),
            )
        });
        if result.is_ok() {
            let _ = app.emit("upload-progress", 100);
        }
        result
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Read a remote Flipper file and persist it directly to a local filesystem
/// path. This avoids base64-encoding the payload through the webview.
#[tauri::command(rename_all = "snake_case")]
pub async fn storage_read_to_local(
    path: String,
    local_path: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let generation = begin_transfer(&state.transfer_generation);
    let cancelled_generation = Arc::clone(&state.transfer_cancelled_generation);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        let data = with_client(&mode_mutex, &client_mutex, |c| {
            storage::storage_read(
                c,
                &path,
                |received, total| {
                    let pct = (received * 100)
                        .checked_div(total)
                        .map(|v| v as u32)
                        .unwrap_or(0);
                    let _ = app.emit("download-progress", pct);
                },
                || transfer_cancelled(&cancelled_generation, generation),
            )
        })?;
        write_atomic(&PathBuf::from(local_path), &data)?;
        Ok(())
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Read a local filesystem path and upload it directly to the Flipper without
/// base64-encoding the payload through the webview.
#[tauri::command(rename_all = "snake_case")]
pub async fn storage_write_from_local(
    path: String,
    local_path: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let ble_cancel_tx = Arc::clone(&state.ble_cancel_tx);
    let generation = begin_transfer(&state.transfer_generation);
    let cancelled_generation = Arc::clone(&state.transfer_cancelled_generation);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        let bytes = std::fs::read(local_path)?;
        let app_for_progress = app.clone();
        let result = with_transfer_client(&mode_mutex, &client_mutex, &ble_cancel_tx, &app, |c| {
            storage::storage_write(
                c,
                &path,
                &bytes,
                |sent, total| {
                    let pct = upload_progress_pct(sent, total);
                    let _ = app_for_progress.emit("upload-progress", pct);
                },
                || transfer_cancelled(&cancelled_generation, generation),
            )
        });
        if result.is_ok() {
            let _ = app.emit("upload-progress", 100);
        }
        result
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn storage_mkdir(path: String, state: State<'_, AppState>) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            storage::storage_mkdir(c, &path)
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn storage_delete(
    path: String,
    recursive: bool,
    state: State<'_, AppState>,
) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            storage::storage_delete(c, &path, recursive)
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

const MAX_DELETE_BATCH_ITEMS: usize = 1_000;

/// One validated delete operation submitted by the file browser.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StorageDeleteTarget {
    pub path: String,
    pub recursive: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct StorageDeleteFailure {
    pub path: String,
    pub recursive: bool,
    pub error: String,
    /// True when the transport was torn down and no later item was attempted.
    pub fatal: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct StorageDeleteManyResult {
    pub deleted: Vec<StorageDeleteTarget>,
    pub failed: Vec<StorageDeleteFailure>,
    pub unattempted: Vec<StorageDeleteTarget>,
    pub stopped_reason: Option<String>,
}

/// Validate the complete batch before acquiring the client or issuing any RPC.
/// This prevents a malformed later target from being discovered only after
/// earlier entries have already been removed.
fn validate_destructive_path(path: &str) -> Result<()> {
    validate_path(path)?;

    // Destructive operations deliberately use a stricter grammar than normal
    // storage reads. Empty or dot components can spell a storage root (for
    // example `/ext/`, `/ext/.`, or `/ext//`) or create ambiguous aliases for
    // another target. Require a root plus at least one ordinary child segment.
    let mut components = path.split('/');
    if components.next() != Some("") {
        return Err(FlipperError::Session("Delete path must be absolute".into()));
    }
    let components: Vec<&str> = components.collect();
    if components.len() < 2 {
        return Err(FlipperError::Session(
            "Deleting a storage root is not allowed".into(),
        ));
    }
    if components.iter().any(|component| component.is_empty()) {
        return Err(FlipperError::Session(
            "Delete path cannot contain empty components".into(),
        ));
    }
    if components.contains(&".") {
        return Err(FlipperError::Session(
            "Delete path cannot contain dot components".into(),
        ));
    }
    Ok(())
}

fn validate_delete_many_targets(targets: &[StorageDeleteTarget]) -> Result<()> {
    if targets.is_empty() {
        return Err(FlipperError::Session(
            "Delete batch must contain at least one item".into(),
        ));
    }
    if targets.len() > MAX_DELETE_BATCH_ITEMS {
        return Err(FlipperError::Session(format!(
            "Delete batch exceeds the {MAX_DELETE_BATCH_ITEMS}-item limit"
        )));
    }

    let mut unique_paths = HashSet::with_capacity(targets.len());
    for target in targets {
        validate_destructive_path(&target.path)?;
        if !unique_paths.insert(target.path.as_str()) {
            return Err(FlipperError::Session(format!(
                "Delete batch contains duplicate path: {}",
                target.path
            )));
        }
    }
    Ok(())
}

/// Execute a pre-validated batch sequentially. The caller owns the client lock
/// for this entire function. Protocol-level failures are recorded and the next
/// item is attempted; fatal transport/framing failures stop the batch and mark
/// every later target as unattempted.
fn execute_delete_many(
    targets: Vec<StorageDeleteTarget>,
    mut delete: impl FnMut(&StorageDeleteTarget) -> Result<()>,
) -> StorageDeleteManyResult {
    let mut result = StorageDeleteManyResult {
        deleted: Vec::with_capacity(targets.len()),
        failed: Vec::new(),
        unattempted: Vec::new(),
        stopped_reason: None,
    };

    let mut targets = targets.into_iter();
    while let Some(target) = targets.next() {
        match delete(&target) {
            Ok(()) => result.deleted.push(target),
            Err(error) => {
                let fatal = is_fatal_transport_error(&error);
                let error = error.to_string();
                result.failed.push(StorageDeleteFailure {
                    path: target.path,
                    recursive: target.recursive,
                    error: error.clone(),
                    fatal,
                });
                if fatal {
                    result.stopped_reason = Some(error);
                    result.unattempted.extend(targets);
                    break;
                }
            }
        }
    }

    result
}

/// Delete a confirmed file-browser batch under one client lock.
///
/// All paths are validated before the lock/RPC phase. If a fatal wire error
/// occurs, the current item is reported as failed, later items are explicitly
/// unattempted, and the connection is torn down using the same BLE cancellation
/// and frontend disconnect event semantics as other fatal transfer commands.
#[tauri::command]
pub async fn storage_delete_many(
    targets: Vec<StorageDeleteTarget>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<StorageDeleteManyResult> {
    validate_delete_many_targets(&targets)?;

    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let ble_cancel_tx = Arc::clone(&state.ble_cancel_tx);

    tauri::async_runtime::spawn_blocking(move || {
        {
            let mode = mode_mutex
                .lock()
                .map_err(|_| FlipperError::Internal("Connection mode lock poisoned".into()))?;
            if *mode == ConnectionMode::Cli {
                return Err(FlipperError::CliModeActive);
            }
        }

        let mut client_guard = client_mutex
            .lock()
            .map_err(|_| FlipperError::Internal("Client lock poisoned".into()))?;
        let client = client_guard.as_mut().ok_or(FlipperError::NotConnected)?;
        let result = execute_delete_many(targets, |target| {
            storage::storage_delete(client, &target.path, target.recursive)
        });

        let fatal_reason = result.stopped_reason.clone();
        if fatal_reason.is_some() {
            // Clear the client while this batch still owns the lock so no RPC
            // can slip in between the fatal error and connection teardown.
            *client_guard = None;
        }
        drop(client_guard);

        if let Some(reason) = fatal_reason {
            tracing::warn!("tearing down connection after delete batch failure: {reason}");
            diag::log_event("DeleteBatchConnectionTornDown", reason.clone());
            if let Ok(mut tx_guard) = ble_cancel_tx.lock() {
                if let Some(tx) = tx_guard.take() {
                    let _ = tx.send(());
                }
            }
            let _ = app.emit("flipper-disconnected", &reason);
        }

        Ok(result)
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Rename (or move) a file/directory on the Flipper.
/// Both `old_path` and `new_path` must be absolute paths on the same storage.
#[tauri::command(rename_all = "snake_case")]
pub async fn storage_rename(
    old_path: String,
    new_path: String,
    state: State<'_, AppState>,
) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&old_path)?;
        validate_path(&new_path)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            storage::storage_rename(c, &old_path, &new_path)
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Storage space info for a path (e.g. "/ext" or "/int").
#[derive(Serialize, Deserialize)]
pub struct StorageInfo {
    pub total_space: u64,
    pub free_space: u64,
}

#[tauri::command]
pub async fn storage_du(path: String, state: State<'_, AppState>) -> Result<u64> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            storage::storage_du(c, &path)
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[tauri::command]
pub async fn storage_info(path: String, state: State<'_, AppState>) -> Result<StorageInfo> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            let (total, free) = storage::storage_info(c, &path)?;
            Ok(StorageInfo {
                total_space: total,
                free_space: free,
            })
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Get the modification timestamp of a file (Unix epoch seconds).
#[tauri::command]
pub async fn storage_timestamp(path: String, state: State<'_, AppState>) -> Result<u32> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            storage::storage_timestamp(c, &path)
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Cancel an in-progress transfer (read or write).
/// Marks the active transfer generation as cancelled.
#[tauri::command]
pub fn cancel_transfer(state: State<AppState>) -> Result<()> {
    let generation = state.transfer_generation.load(Ordering::Relaxed);
    state
        .transfer_cancelled_generation
        .store(generation, Ordering::Relaxed);
    Ok(())
}

/// Sum the byte size of every file under `path`, recursively. Used as the
/// denominator for whole-folder download progress.
fn sum_tree_bytes(client: &mut FlipperClient, path: &str) -> Result<u64> {
    let mut total: u64 = 0;
    let mut queue: Vec<String> = vec![path.to_string()];
    while let Some(dir) = queue.pop() {
        let entries = storage::storage_list(client, &dir)?;
        for e in entries {
            library_walk::validate_child_name(&e.name)?;
            let sub = join_remote(&dir, &e.name);
            if e.r#type == 1 {
                queue.push(sub);
            } else {
                total = total.saturating_add(e.size as u64);
            }
        }
    }
    Ok(total)
}

/// Download `remote_dir` into `local_dir` recursively. `local_dir` is the
/// fully-resolved destination — directory contents land directly inside it,
/// not under a wrapper folder. The wrapper is created by the caller so that
/// behaviour is explicit at the command boundary.
fn download_dir_recursive(
    client: &mut FlipperClient,
    remote_dir: &str,
    local_dir: &Path,
    total_bytes: u64,
    bytes_done: &mut u64,
    on_progress: &dyn Fn(u64, u64),
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    if cancelled() {
        return Err(FlipperError::TransferCancelled);
    }
    std::fs::create_dir_all(local_dir)?;
    let entries = storage::storage_list(client, remote_dir)?;
    for e in entries {
        if cancelled() {
            return Err(FlipperError::TransferCancelled);
        }
        library_walk::validate_child_name(&e.name)?;
        let remote_sub = join_remote(remote_dir, &e.name);
        let local_sub = local_dir.join(&e.name);
        if e.r#type == 1 {
            download_dir_recursive(
                client,
                &remote_sub,
                &local_sub,
                total_bytes,
                bytes_done,
                on_progress,
                cancelled,
            )?;
        } else {
            let start = *bytes_done;
            let file_size = e.size as u64;
            let data = storage::storage_read(
                client,
                &remote_sub,
                |received, _| {
                    let cumulative = start.saturating_add(received as u64);
                    on_progress(cumulative.min(total_bytes), total_bytes);
                },
                cancelled,
            )?;
            write_atomic(&local_sub, &data)?;
            *bytes_done = bytes_done.saturating_add(file_size);
            on_progress(*bytes_done, total_bytes);
        }
    }
    Ok(())
}

/// Recursively download a Flipper directory to a local destination.
///
/// `local_path` is the full destination folder; the caller is responsible for
/// appending the source directory's name (so picking `~/Downloads` for `apps`
/// passes `~/Downloads/apps` here). The folder is created if missing; existing
/// files at colliding paths are overwritten.
///
/// Emits `"download-progress"` events as `u32` percentages (0-100) computed
/// against the pre-walked total byte count, so the bar advances smoothly
/// across many files.
#[tauri::command(rename_all = "snake_case")]
pub async fn storage_read_dir_to_local(
    path: String,
    local_path: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let generation = begin_transfer(&state.transfer_generation);
    let cancelled_generation = Arc::clone(&state.transfer_cancelled_generation);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&path)?;
        let local_root = PathBuf::from(local_path);
        with_client(&mode_mutex, &client_mutex, |c| {
            let total_bytes = sum_tree_bytes(c, &path)?;
            // Emit 0% up front so the bar shows even before the first chunk
            // arrives (the pre-walk is fast but not instant on big trees).
            let _ = app.emit("download-progress", 0u32);

            let mut bytes_done: u64 = 0;
            let on_progress = |done: u64, total: u64| {
                let pct = done
                    .saturating_mul(100)
                    .checked_div(total)
                    .map(|v| v.min(100) as u32)
                    .unwrap_or(100);
                let _ = app.emit("download-progress", pct);
            };

            let is_cancelled = || transfer_cancelled(&cancelled_generation, generation);
            download_dir_recursive(
                c,
                &path,
                &local_root,
                total_bytes,
                &mut bytes_done,
                &on_progress,
                &is_cancelled,
            )?;

            // Empty folders: ensure final 100% lands so the UI clears cleanly.
            let _ = app.emit("download-progress", 100u32);
            Ok(())
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Extract a .tar archive on the Flipper.
#[tauri::command(rename_all = "snake_case")]
pub async fn storage_tar_extract(
    tar_path: String,
    out_path: String,
    state: State<'_, AppState>,
) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || {
        validate_path(&tar_path)?;
        validate_path(&out_path)?;
        with_client(&mode_mutex, &client_mutex, |c| {
            storage::storage_tar_extract(c, &tar_path, &out_path)
        })
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &str) -> StorageDeleteTarget {
        StorageDeleteTarget {
            path: path.to_string(),
            recursive: false,
        }
    }

    #[test]
    fn delete_batch_validation_rejects_every_invalid_batch_before_execution() {
        assert!(validate_delete_many_targets(&[]).is_err());
        for invalid_path in [
            "/ext",
            "/ext/",
            "/ext/.",
            "/ext//",
            "/int",
            "/int/.",
            "/int//folder",
            "/any",
            "/any//",
            "/any/./folder",
            "/ext/folder/",
            "/ext/folder//child",
            "/ext/../int/secret",
        ] {
            assert!(
                validate_delete_many_targets(&[target(invalid_path)]).is_err(),
                "should reject destructive path {invalid_path}"
            );
        }
        assert!(validate_delete_many_targets(&[target("/ext/a"), target("/ext/a"),]).is_err());

        assert!(validate_delete_many_targets(&[
            target("/ext/a"),
            StorageDeleteTarget {
                path: "/int/folder".into(),
                recursive: true,
            },
        ])
        .is_ok());
    }

    #[test]
    fn delete_batch_continues_after_non_fatal_item_failure() {
        let targets = vec![target("/ext/a"), target("/ext/b"), target("/ext/c")];
        let mut attempted = Vec::new();
        let result = execute_delete_many(targets, |item| {
            attempted.push(item.path.clone());
            if item.path == "/ext/b" {
                Err(FlipperError::Session("permission denied".into()))
            } else {
                Ok(())
            }
        });

        assert_eq!(attempted, ["/ext/a", "/ext/b", "/ext/c"]);
        assert_eq!(result.deleted, [target("/ext/a"), target("/ext/c")]);
        assert_eq!(result.failed.len(), 1);
        assert!(!result.failed[0].fatal);
        assert!(result.unattempted.is_empty());
        assert!(result.stopped_reason.is_none());
    }

    #[test]
    fn delete_batch_stops_and_accounts_for_later_items_after_fatal_failure() {
        let targets = vec![target("/ext/a"), target("/ext/b"), target("/ext/c")];
        let mut attempted = Vec::new();
        let result = execute_delete_many(targets, |item| {
            attempted.push(item.path.clone());
            if item.path == "/ext/b" {
                Err(FlipperError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "device disconnected",
                )))
            } else {
                Ok(())
            }
        });

        assert_eq!(attempted, ["/ext/a", "/ext/b"]);
        assert_eq!(result.deleted, [target("/ext/a")]);
        assert_eq!(result.failed.len(), 1);
        assert!(result.failed[0].fatal);
        assert_eq!(result.unattempted, [target("/ext/c")]);
        assert!(result
            .stopped_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("device disconnected")));
    }
}
