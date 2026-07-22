use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, AppHandle, Emitter, State};

use crate::commands::client::{
    connection_handle, execute_connection, retire_connection_owner, with_connection,
};
use crate::commands::path::{validate_path, DevicePath};
use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::library_walk;
use crate::flipper::storage;
use crate::operation::{require_cancelled, OperationName, ProgressTracker};
use crate::pb_storage;
use crate::state::AppState;

fn join_remote(dir: &str, name: &str) -> Result<String> {
    library_walk::join_path(dir, name)
}

const TEMP_CREATE_ATTEMPTS: u32 = 64;

struct OwnedTempFile {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl OwnedTempFile {
    fn allocate(target: &Path, operation_id: u64) -> Result<Self> {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let base = target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("download");
        for attempt in 0..TEMP_CREATE_ATTEMPTS {
            let path =
                target.with_file_name(format!(".{base}.flipperui-{operation_id}-{attempt}.part"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        committed: false,
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(FlipperError::ConnectionBusy)
    }

    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| FlipperError::Internal("temporary file is closed".into()))?
            .write_all(data)?;
        Ok(())
    }

    fn commit(mut self, target: &Path) -> Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            file.sync_all()?;
        }
        std::fs::rename(&self.path, target)?;
        self.committed = true;
        Ok(())
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OwnedTempFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn write_atomic(path: &Path, data: &[u8], operation_id: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = OwnedTempFile::allocate(path, operation_id)?;
    temporary.write_all(data)?;
    temporary.commit(path)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    operation_id: u64,
    completed: u64,
    total: u64,
    percent: u32,
}

fn send_transfer_progress(
    channel: &Channel<TransferProgress>,
    operation_id: u64,
    snapshot: crate::operation::ProgressSnapshot,
) {
    let _ = channel.send(TransferProgress {
        operation_id,
        completed: snapshot.completed,
        total: snapshot.total,
        percent: snapshot.percent,
    });
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
pub async fn storage_list(path: DevicePath, state: State<'_, AppState>) -> Result<Vec<FileEntry>> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        validate_path(&path)?;
        storage::storage_list(client, &path)
            .map(|files| files.into_iter().map(FileEntry::from).collect())
    })
    .await
}

#[tauri::command]
pub async fn storage_stat(path: DevicePath, state: State<'_, AppState>) -> Result<FileEntry> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        validate_path(&path)?;
        storage::storage_stat(client, &path).map(FileEntry::from)
    })
    .await
}

/// Read a file from the Flipper. Returns base64-encoded bytes to avoid
/// JSON number-array overhead for large files.
/// Emits `"download-progress"` events (u32 0–100) to the frontend after each chunk.
#[tauri::command]
pub async fn storage_read(
    path: DevicePath,
    state: State<'_, AppState>,
    on_progress: Channel<TransferProgress>,
) -> Result<String> {
    let operation = state.operations.begin(OperationName::Transfer)?;
    let operation_id = operation.id();
    let cancelled = operation.cancel_token();
    let tracker = RefCell::new(ProgressTracker::default());
    send_transfer_progress(
        &on_progress,
        operation_id,
        tracker.borrow_mut().update(0, 0),
    );

    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let _operation = operation;
        validate_path(&path)?;
        let data = storage::storage_read(
            client,
            &path,
            |received, total| {
                let snapshot = tracker.borrow_mut().update(received as u64, total as u64);
                send_transfer_progress(&on_progress, operation_id, snapshot);
            },
            || cancelled.load(Ordering::Acquire),
        )?;
        let total = data.len() as u64;
        let snapshot = tracker.borrow_mut().finish(total, total);
        send_transfer_progress(&on_progress, operation_id, snapshot);
        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &data,
        ))
    })
    .await
}

/// Write a file to the Flipper. `data` is base64-encoded.
/// Emits `"upload-progress"` events (u32 0–100) to the frontend after each chunk.
#[tauri::command]
pub async fn storage_write(
    path: DevicePath,
    data: String,
    state: State<'_, AppState>,
    on_progress: Channel<TransferProgress>,
) -> Result<()> {
    let operation = state.operations.begin(OperationName::Transfer)?;
    let operation_id = operation.id();
    let cancelled = operation.cancel_token();
    let tracker = RefCell::new(ProgressTracker::default());
    send_transfer_progress(
        &on_progress,
        operation_id,
        tracker.borrow_mut().update(0, 0),
    );

    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let _operation = operation;
        validate_path(&path)?;
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
            .map_err(|e| FlipperError::Session(format!("base64 decode error: {e}")))?;

        let result = storage::storage_write(
            client,
            &path,
            &bytes,
            |sent, total| {
                let snapshot = tracker.borrow_mut().update(sent as u64, total as u64);
                send_transfer_progress(&on_progress, operation_id, snapshot);
            },
            || cancelled.load(Ordering::Acquire),
        );
        if result.is_ok() {
            let total = bytes.len() as u64;
            let snapshot = tracker.borrow_mut().finish(total, total);
            send_transfer_progress(&on_progress, operation_id, snapshot);
        }
        result
    })
    .await
}

/// Read a remote Flipper file and persist it directly to a local filesystem
/// path. This avoids base64-encoding the payload through the webview.
#[tauri::command(rename_all = "snake_case")]
pub async fn storage_read_to_local(
    path: DevicePath,
    local_path: String,
    state: State<'_, AppState>,
    on_progress: Channel<TransferProgress>,
) -> Result<()> {
    let operation = state.operations.begin(OperationName::Transfer)?;
    let operation_id = operation.id();
    let cancelled = operation.cancel_token();
    let tracker = Arc::new(std::sync::Mutex::new(ProgressTracker::default()));
    send_transfer_progress(
        &on_progress,
        operation_id,
        tracker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .update(0, 0),
    );
    let progress_in_job = on_progress.clone();
    let tracker_in_job = Arc::clone(&tracker);
    let cancelled_in_job = Arc::clone(&cancelled);

    let data = with_connection(Arc::clone(&state.connection_owner), move |client| {
        validate_path(&path)?;
        storage::storage_read(
            client,
            &path,
            |received, total| {
                let snapshot = tracker_in_job
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .update(received as u64, total as u64);
                send_transfer_progress(&progress_in_job, operation_id, snapshot);
            },
            || cancelled_in_job.load(Ordering::Acquire),
        )
    })
    .await?;
    if cancelled.load(Ordering::Acquire) {
        return Err(FlipperError::TransferCancelled);
    }
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = operation;
        write_atomic(&PathBuf::from(local_path), &data, operation_id)?;
        let total = data.len() as u64;
        let snapshot = tracker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .finish(total, total);
        send_transfer_progress(&on_progress, operation_id, snapshot);
        Ok(())
    })
    .await
    .map_err(|error| FlipperError::Internal(error.to_string()))?
}

/// Read a local filesystem path and upload it directly to the Flipper without
/// base64-encoding the payload through the webview.
#[tauri::command(rename_all = "snake_case")]
pub async fn storage_write_from_local(
    path: DevicePath,
    local_path: String,
    state: State<'_, AppState>,
    on_progress: Channel<TransferProgress>,
) -> Result<()> {
    let operation = state.operations.begin(OperationName::Transfer)?;
    let operation_id = operation.id();
    let cancelled = operation.cancel_token();
    let tracker = RefCell::new(ProgressTracker::default());
    send_transfer_progress(
        &on_progress,
        operation_id,
        tracker.borrow_mut().update(0, 0),
    );

    validate_path(&path)?;
    let bytes = tauri::async_runtime::spawn_blocking(move || std::fs::read(local_path))
        .await
        .map_err(|error| FlipperError::Internal(error.to_string()))??;
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let _operation = operation;
        let result = storage::storage_write(
            client,
            &path,
            &bytes,
            |sent, total| {
                let snapshot = tracker.borrow_mut().update(sent as u64, total as u64);
                send_transfer_progress(&on_progress, operation_id, snapshot);
            },
            || cancelled.load(Ordering::Acquire),
        );
        if result.is_ok() {
            let total = bytes.len() as u64;
            let snapshot = tracker.borrow_mut().finish(total, total);
            send_transfer_progress(&on_progress, operation_id, snapshot);
        }
        result
    })
    .await
}

#[tauri::command]
pub async fn storage_mkdir(path: DevicePath, state: State<'_, AppState>) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        validate_path(&path)?;
        storage::storage_mkdir(client, &path)
    })
    .await
}

#[tauri::command]
pub async fn storage_delete(
    path: DevicePath,
    recursive: bool,
    state: State<'_, AppState>,
) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        validate_path(&path)?;
        storage::storage_delete(client, &path, recursive)
    })
    .await
}

const MAX_DELETE_BATCH_ITEMS: usize = 1_000;

/// One validated delete operation submitted by the file browser.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StorageDeleteTarget {
    pub path: DevicePath,
    pub recursive: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct StorageDeleteFailure {
    pub path: DevicePath,
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
fn validate_destructive_path(path: &DevicePath) -> Result<()> {
    // DevicePath has already normalized aliases such as `/ext//a` and
    // `/ext/./a`, so the only remaining destructive boundary is protecting
    // the three storage roots themselves.
    if path.is_root() {
        return Err(FlipperError::InvalidDevicePath {
            path: path.to_string(),
            reason: "deleting a storage root is not allowed".into(),
        });
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
            return Err(FlipperError::InvalidDevicePath {
                path: target.path.to_string(),
                reason: "delete batch contains a duplicate path".into(),
            });
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
                let fatal = crate::error::is_fatal_transport_error(&error);
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

    let owner = Arc::clone(&state.connection_owner);
    let handle = connection_handle(&owner)?;
    let result = execute_connection(&handle, move |client| {
        let result = execute_delete_many(targets, |target| {
            storage::storage_delete(client, &target.path, target.recursive)
        });
        Ok(result)
    })
    .await?;

    if let Some(reason) = result.stopped_reason.as_ref() {
        tracing::warn!("tearing down connection after delete batch failure: {reason}");
        let lifecycle = Arc::clone(&state.connection_lifecycle);
        let _lifecycle = lifecycle.lock().await;
        if retire_connection_owner(&owner, &handle) {
            let _ = handle.shutdown().await;
            if let Some(cancel) = state
                .ble_cancel_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = cancel.send(());
            }
            let _ = app.emit("flipper-disconnected", reason);
        }
    }
    Ok(result)
}

/// Rename (or move) a file/directory on the Flipper.
/// Both `old_path` and `new_path` must be absolute paths on the same storage.
#[tauri::command(rename_all = "snake_case")]
pub async fn storage_rename(
    old_path: DevicePath,
    new_path: DevicePath,
    state: State<'_, AppState>,
) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        validate_path(&old_path)?;
        validate_path(&new_path)?;
        storage::storage_rename(client, &old_path, &new_path)
    })
    .await
}

/// Storage space info for a path (e.g. "/ext" or "/int").
#[derive(Serialize, Deserialize)]
pub struct StorageInfo {
    pub total_space: u64,
    pub free_space: u64,
}

#[tauri::command]
pub async fn storage_du(path: DevicePath, state: State<'_, AppState>) -> Result<u64> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        validate_path(&path)?;
        storage::storage_du(client, &path)
    })
    .await
}

#[tauri::command]
pub async fn storage_info(path: DevicePath, state: State<'_, AppState>) -> Result<StorageInfo> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        validate_path(&path)?;
        let (total, free) = storage::storage_info(client, &path)?;
        Ok(StorageInfo {
            total_space: total,
            free_space: free,
        })
    })
    .await
}

/// Get the modification timestamp of a file (Unix epoch seconds).
#[tauri::command]
pub async fn storage_timestamp(path: DevicePath, state: State<'_, AppState>) -> Result<u32> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        validate_path(&path)?;
        storage::storage_timestamp(client, &path)
    })
    .await
}

/// Cancel exactly the transfer identified by its invocation-scoped progress
/// channel. Retired IDs are rejected and cannot affect a replacement.
#[tauri::command]
pub fn cancel_transfer(operation_id: u64, state: State<AppState>) -> Result<()> {
    require_cancelled(
        state
            .operations
            .cancel(OperationName::Transfer, operation_id),
    )
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
            let sub = join_remote(&dir, &e.name)?;
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
#[derive(Clone, Copy)]
struct DownloadDirContext<'a> {
    total_bytes: u64,
    operation_id: u64,
    on_progress: &'a dyn Fn(u64, u64),
    cancelled: &'a dyn Fn() -> bool,
}

fn download_dir_recursive(
    client: &mut FlipperClient,
    remote_dir: &str,
    local_dir: &Path,
    bytes_done: &mut u64,
    context: DownloadDirContext<'_>,
) -> Result<()> {
    if (context.cancelled)() {
        return Err(FlipperError::TransferCancelled);
    }
    std::fs::create_dir_all(local_dir)?;
    let entries = storage::storage_list(client, remote_dir)?;
    for e in entries {
        if (context.cancelled)() {
            return Err(FlipperError::TransferCancelled);
        }
        library_walk::validate_child_name(&e.name)?;
        let remote_sub = join_remote(remote_dir, &e.name)?;
        let local_sub = local_dir.join(&e.name);
        if e.r#type == 1 {
            download_dir_recursive(client, &remote_sub, &local_sub, bytes_done, context)?;
        } else {
            let start = *bytes_done;
            let file_size = e.size as u64;
            let data = storage::storage_read(
                client,
                &remote_sub,
                |received, _| {
                    let cumulative = start.saturating_add(received as u64);
                    (context.on_progress)(cumulative.min(context.total_bytes), context.total_bytes);
                },
                context.cancelled,
            )?;
            write_atomic(&local_sub, &data, context.operation_id)?;
            *bytes_done = bytes_done.saturating_add(file_size);
            (context.on_progress)(*bytes_done, context.total_bytes);
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
    path: DevicePath,
    local_path: String,
    state: State<'_, AppState>,
    on_progress: Channel<TransferProgress>,
) -> Result<()> {
    let operation = state.operations.begin(OperationName::Transfer)?;
    let operation_id = operation.id();
    let cancelled = operation.cancel_token();
    let tracker = RefCell::new(ProgressTracker::default());
    send_transfer_progress(
        &on_progress,
        operation_id,
        tracker.borrow_mut().update(0, 0),
    );

    with_connection(Arc::clone(&state.connection_owner), move |client| {
        let _operation = operation;
        validate_path(&path)?;
        let local_root = PathBuf::from(local_path);
        let total_bytes = sum_tree_bytes(client, &path)?;
        let snapshot = tracker.borrow_mut().update(0, total_bytes);
        send_transfer_progress(&on_progress, operation_id, snapshot);

        let mut bytes_done: u64 = 0;
        let report = |done: u64, total: u64| {
            let snapshot = tracker.borrow_mut().update(done, total);
            send_transfer_progress(&on_progress, operation_id, snapshot);
        };

        let is_cancelled = || cancelled.load(Ordering::Acquire);
        download_dir_recursive(
            client,
            &path,
            &local_root,
            &mut bytes_done,
            DownloadDirContext {
                total_bytes,
                operation_id,
                on_progress: &report,
                cancelled: &is_cancelled,
            },
        )?;

        let snapshot = tracker.borrow_mut().finish(bytes_done, total_bytes);
        send_transfer_progress(&on_progress, operation_id, snapshot);
        Ok(())
    })
    .await
}

/// Extract a .tar archive on the Flipper.
#[tauri::command(rename_all = "snake_case")]
pub async fn storage_tar_extract(
    tar_path: DevicePath,
    out_path: DevicePath,
    state: State<'_, AppState>,
) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        validate_path(&tar_path)?;
        validate_path(&out_path)?;
        storage::storage_tar_extract(client, &tar_path, &out_path)
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    fn test_dir(label: &str) -> PathBuf {
        let id = TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "flipperui-storage-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn target(path: &str) -> StorageDeleteTarget {
        StorageDeleteTarget {
            path: DevicePath::try_from(path).unwrap(),
            recursive: false,
        }
    }

    #[test]
    fn delete_batch_validation_rejects_every_invalid_batch_before_execution() {
        assert!(validate_delete_many_targets(&[]).is_err());
        for root_alias in ["/ext", "/ext/", "/ext/.", "/ext//", "/int", "/any//"] {
            assert!(
                validate_delete_many_targets(&[target(root_alias)]).is_err(),
                "should reject storage root {root_alias}"
            );
        }
        for invalid_path in ["/ext/../int/secret", "/tmp/file", "/ext\\file"] {
            let payload = serde_json::json!({ "path": invalid_path, "recursive": false });
            assert!(
                serde_json::from_value::<StorageDeleteTarget>(payload).is_err(),
                "boundary should reject destructive path {invalid_path}"
            );
        }

        // Harmless aliases normalize before duplicate detection, so the same
        // device entry cannot be deleted twice under different spellings.
        assert!(validate_delete_many_targets(&[target("/ext/a"), target("/ext//./a"),]).is_err());
        assert!(validate_delete_many_targets(&[target("/ext/a"), target("/ext/a"),]).is_err());

        assert!(validate_delete_many_targets(&[
            target("/ext/a"),
            StorageDeleteTarget {
                path: DevicePath::try_from("/int/folder").unwrap(),
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
            attempted.push(item.path.to_string());
            if item.path.as_str() == "/ext/b" {
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
            attempted.push(item.path.to_string());
            if item.path.as_str() == "/ext/b" {
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

    #[test]
    fn same_target_temp_allocations_do_not_collide_or_delete_each_other() {
        let dir = test_dir("temp-collision");
        let target = dir.join("capture.sub");
        let first = OwnedTempFile::allocate(&target, 41).unwrap();
        let first_path = first.path().to_path_buf();
        let second = OwnedTempFile::allocate(&target, 41).unwrap();
        let second_path = second.path().to_path_buf();

        assert_ne!(first_path, second_path);
        assert_eq!(first_path.parent(), target.parent());
        assert_eq!(second_path.parent(), target.parent());
        assert!(first_path.exists());
        assert!(second_path.exists());

        drop(first);
        assert!(!first_path.exists());
        assert!(
            second_path.exists(),
            "one owner must not remove another's temp"
        );
        drop(second);
        assert!(!second_path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn temp_owner_cleans_failure_but_preserves_committed_target() {
        let dir = test_dir("temp-ownership");
        let target = dir.join("download.bin");

        let abandoned_path = {
            let mut abandoned = OwnedTempFile::allocate(&target, 51).unwrap();
            abandoned.write_all(b"partial").unwrap();
            abandoned.path().to_path_buf()
        };
        assert!(!abandoned_path.exists());
        assert!(!target.exists());

        write_atomic(&target, b"complete", 52).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"complete");
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
            .count();
        assert_eq!(leftovers, 0);

        std::fs::remove_dir_all(dir).unwrap();
    }
}
