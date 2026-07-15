//! Tauri commands for the firmware-flash tool.
//!
//! `firmware_providers` / `firmware_fetch_directory` back the source picker;
//! `firmware_flash` runs the whole self-update pipeline (download → verify →
//! unpack → upload to `/ext/update` → `SystemUpdateRequest` → reboot into the
//! on-device updater), streaming `firmware-flash-progress` events the modal
//! renders as a live console.

use std::cell::Cell;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::{diag, firmware};
use crate::flipper::{session, storage};
use crate::state::{
    AppState, ConnectionMode, FirmwareCancelOutcome, FirmwareCommitOutcome, FirmwareOperationState,
};

/// Static descriptor of a firmware source, surfaced to the source picker.
#[derive(Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub blurb: String,
}

/// The streamed flash-progress event payload (`firmware-flash-progress`).
///
/// `message` empty == a pure progress tick (move the footer bar, don't log a
/// line). Non-empty == a log line. `pct` is present for `download`/`upload`.
#[derive(Serialize, Clone)]
struct FlashProgress {
    /// download | verify | prepare | upload | install | reboot | done | error
    stage: String,
    message: String,
    pct: Option<u32>,
    /// info | ok | warn | error
    level: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FirmwareCancelStatus {
    Cancelled,
    TooLate,
    NoActiveOperation,
}

#[derive(Serialize)]
pub struct FirmwareCancelResponse {
    status: FirmwareCancelStatus,
    message: String,
}

/// Where the bundle comes from. Remote sources identify a registered catalog
/// entry; URLs and checksums are deliberately not accepted from the webview.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FlashSource {
    Remote {
        provider_id: String,
        channel_id: String,
        version: String,
        timestamp: u64,
        selection_token: String,
    },
    Local {
        local_path: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlashOptions {
    /// Remove any existing bundle dir before uploading.
    clean: bool,
}

fn emit(app: &AppHandle, stage: &str, level: &str, message: impl Into<String>, pct: Option<u32>) {
    let _ = app.emit(
        "firmware-flash-progress",
        FlashProgress {
            stage: stage.to_string(),
            message: message.into(),
            pct,
            level: level.to_string(),
        },
    );
}

fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{:.2} MB", b / MB)
    }
}

fn overall_pct(done: u64, total: u64) -> u32 {
    if total == 0 {
        return 100;
    }
    ((done.saturating_mul(100)) / total).min(100) as u32
}

struct FirmwareOperationGuard {
    state: Arc<Mutex<FirmwareOperationState>>,
    operation_id: u64,
}

impl Drop for FirmwareOperationGuard {
    fn drop(&mut self) {
        match self.state.lock() {
            Ok(mut state) => state.finish(self.operation_id),
            Err(poisoned) => poisoned.into_inner().finish(self.operation_id),
        }
    }
}

fn begin_firmware_operation(
    state: Arc<Mutex<FirmwareOperationState>>,
) -> Result<FirmwareOperationGuard> {
    let operation_id = state
        .lock()
        .map_err(|_| FlipperError::Internal("firmware operation state is poisoned".into()))?
        .begin()
        .ok_or_else(|| FlipperError::Internal("a firmware flash is already in progress".into()))?;
    Ok(FirmwareOperationGuard {
        state,
        operation_id,
    })
}

fn firmware_operation_cancelled(state: &Mutex<FirmwareOperationState>, operation_id: u64) -> bool {
    match state.lock() {
        Ok(state) => state.is_cancelled(operation_id),
        // A poisoned operation state cannot safely authorize a mutation.
        Err(_) => true,
    }
}

fn ensure_not_cancelled(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(FlipperError::TransferCancelled)
    } else {
        Ok(())
    }
}

fn begin_firmware_commit(state: &Mutex<FirmwareOperationState>, operation_id: u64) -> Result<()> {
    let outcome = state
        .lock()
        .map_err(|_| FlipperError::Internal("firmware operation state is poisoned".into()))?
        .begin_commit(operation_id);
    match outcome {
        FirmwareCommitOutcome::Started => Ok(()),
        FirmwareCommitOutcome::Cancelled => Err(FlipperError::TransferCancelled),
        FirmwareCommitOutcome::NotActive => Err(FlipperError::Internal(
            "firmware operation is no longer active".into(),
        )),
    }
}

/// Cancel only the active firmware flash. Once the operation crosses the
/// updater-request commit barrier, cancellation cannot safely claim success:
/// the device may already be staging or rebooting.
#[tauri::command]
pub fn cancel_firmware_flash(state: State<AppState>) -> Result<FirmwareCancelResponse> {
    let outcome = state
        .firmware_operation
        .lock()
        .map_err(|_| FlipperError::Internal("firmware operation state is poisoned".into()))?
        .cancel_active();
    let response = match outcome {
        FirmwareCancelOutcome::Cancelled => FirmwareCancelResponse {
            status: FirmwareCancelStatus::Cancelled,
            message: "Firmware cancellation requested".into(),
        },
        FirmwareCancelOutcome::TooLate => FirmwareCancelResponse {
            status: FirmwareCancelStatus::TooLate,
            message: "The updater request has started; it is too late to cancel safely".into(),
        },
        FirmwareCancelOutcome::NoActiveOperation => FirmwareCancelResponse {
            status: FirmwareCancelStatus::NoActiveOperation,
            message: "No firmware flash is active".into(),
        },
    };
    Ok(response)
}

/// Send the UPDATE reboot and always invalidate the client slot afterwards.
/// Even a reported write failure may have partially reached the device, so the
/// old transport must never be reused.
fn reboot_into_updater(client_slot: &mut Option<FlipperClient>) -> Result<()> {
    let result = match client_slot.as_mut() {
        Some(client) => session::reboot(client, 2),
        None => Err(FlipperError::NotConnected),
    };
    *client_slot = None;
    result
}

/// Treat "already exists" as success for an mkdir that's only there to ensure a
/// directory is present.
fn ignore_already_exists(r: Result<()>) -> Result<()> {
    match r {
        // ERROR_STORAGE_EXIST = 6
        Err(FlipperError::Rpc { status: 6, .. }) => Ok(()),
        other => other,
    }
}

/// Cleaning an absent update directory is success; every other storage error
/// must be surfaced rather than silently turning a denied/failed clean into a
/// mixed old/new update bundle.
fn ignore_not_exist(r: Result<()>) -> Result<()> {
    match r {
        // ERROR_STORAGE_NOT_EXIST = 7
        Err(FlipperError::Rpc { status: 7, .. }) => Ok(()),
        other => other,
    }
}

fn is_fatal_device_error(error: &FlipperError) -> bool {
    match error {
        FlipperError::Serial(_) => true,
        FlipperError::Io(io) => !matches!(
            io.kind(),
            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
        ),
        FlipperError::Decode(_) | FlipperError::Encode(_) => true,
        _ => false,
    }
}

/// Map a non-OK `UpdateResultCode` to an actionable message.
fn update_code_message(code: i32) -> String {
    let detail = match code {
        1 => "manifest path is invalid",
        2 => "manifest folder not found",
        3 => "manifest is invalid",
        4 => "an update stage file is missing",
        5 => "a stage failed its integrity check",
        6 => "manifest pointer error",
        7 => "firmware target mismatch (wrong hardware)",
        8 => "manifest version is outdated for this firmware",
        9 => "internal storage is full",
        _ => "unspecified updater error",
    };
    format!("Flipper rejected the update: {detail} (code {code})")
}

/// Ensure every ancestor directory of `rel_path` exists under `base`, recording
/// what we've made so repeated prefixes aren't re-created. Flat bundles (the
/// common case) hit this zero times.
fn ensure_parent_dirs(
    client: &mut FlipperClient,
    base: &str,
    rel_path: &str,
    made: &mut HashSet<String>,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let Some((dir_part, _)) = rel_path.rsplit_once('/') else {
        return Ok(());
    };
    let mut acc = base.to_string();
    for seg in dir_part.split('/') {
        acc.push('/');
        acc.push_str(seg);
        if made.insert(acc.clone()) {
            ensure_not_cancelled(cancelled)?;
            ignore_already_exists(storage::storage_mkdir(client, &acc))?;
        }
    }
    Ok(())
}

#[tauri::command]
pub fn firmware_providers() -> Vec<ProviderInfo> {
    firmware::PROVIDERS
        .iter()
        .map(|p| ProviderInfo {
            id: p.id.to_string(),
            name: p.name.to_string(),
            blurb: p.blurb.to_string(),
        })
        .collect()
}

/// Fetch + normalize a provider's `directory.json`. Pure network — no device
/// lock — so it works even while a transfer is busy.
#[tauri::command(rename_all = "snake_case")]
pub async fn firmware_fetch_directory(provider_id: String) -> Result<firmware::FirmwareCatalog> {
    let p = firmware::provider(&provider_id).ok_or_else(|| {
        FlipperError::Internal(format!("unknown firmware provider: {provider_id}"))
    })?;
    tauri::async_runtime::spawn_blocking(move || firmware::fetch_catalog(p))
        .await
        .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Run the full self-update pipeline. Emits `firmware-flash-progress` events
/// throughout; the device disconnects once it reboots into its own updater, so
/// a successful run ends on the `done` stage with the client dropped.
#[tauri::command]
pub async fn firmware_flash(
    source: FlashSource,
    options: FlashOptions,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let ble_cancel_tx = Arc::clone(&state.ble_cancel_tx);
    let firmware_operation = Arc::clone(&state.firmware_operation);
    let operation_guard = begin_firmware_operation(Arc::clone(&firmware_operation))?;
    let operation_id = operation_guard.operation_id;

    tauri::async_runtime::spawn_blocking(move || {
        let _operation_guard = operation_guard;
        let is_cancelled = || firmware_operation_cancelled(&firmware_operation, operation_id);
        ensure_not_cancelled(&is_cancelled)?;

        // ── 1. Acquire the bundle bytes ────────────────────────────────────
        let (bytes, expected_sha256): (Vec<u8>, Option<String>) = match source {
            FlashSource::Local { local_path } => {
                emit(
                    &app,
                    "download",
                    "info",
                    format!("Reading {local_path}"),
                    None,
                );
                let data = firmware::read_local_archive(std::path::Path::new(&local_path))?;
                emit(
                    &app,
                    "download",
                    "ok",
                    format!(
                        "Loaded {} ({})",
                        short_name(&local_path),
                        human_size(data.len() as u64)
                    ),
                    Some(100),
                );
                (data, None)
            }
            FlashSource::Remote {
                provider_id,
                channel_id,
                version,
                timestamp,
                selection_token,
            } => {
                let provider = firmware::provider(&provider_id).ok_or_else(|| {
                    FlipperError::Internal(format!("unknown firmware provider: {provider_id}"))
                })?;
                emit(
                    &app,
                    "download",
                    "info",
                    "Resolving the selected build from the provider catalog…",
                    Some(0),
                );
                let resolved = firmware::resolve_firmware(
                    provider,
                    &channel_id,
                    &version,
                    timestamp,
                    &selection_token,
                )?;
                emit(
                    &app,
                    "download",
                    "info",
                    format!("Downloading {}", resolved.label),
                    Some(0),
                );
                let last_pct = Cell::new(-1i32);
                let app_dl = app.clone();
                let data = firmware::download(
                    &resolved.url,
                    provider.download_hosts,
                    |done, total| {
                        let pct = overall_pct(done, total) as i32;
                        if pct != last_pct.get() {
                            last_pct.set(pct);
                            emit(&app_dl, "download", "info", "", Some(pct as u32));
                        }
                    },
                    &is_cancelled,
                )?;
                emit(
                    &app,
                    "download",
                    "ok",
                    format!("Downloaded {}", human_size(data.len() as u64)),
                    Some(100),
                );
                (data, Some(resolved.sha256))
            }
        };

        if is_cancelled() {
            return Err(FlipperError::TransferCancelled);
        }

        // ── 2. Verify checksum ─────────────────────────────────────────────
        // Online bundles always have a backend-resolved mandatory checksum.
        // Local archives have no trusted expected value, but remain fully
        // subject to the structural and resource validation below.
        if let Some(expected) = expected_sha256 {
            emit(&app, "verify", "info", "Verifying SHA-256…", None);
            let actual = firmware::sha256_hex(&bytes);
            if !actual.eq_ignore_ascii_case(&expected) {
                emit(
                    &app,
                    "verify",
                    "error",
                    "Checksum mismatch — aborting",
                    None,
                );
                return Err(FlipperError::Internal(
                    "SHA-256 mismatch: the download is corrupt or was tampered with".into(),
                ));
            }
            emit(&app, "verify", "ok", "Checksum verified", None);
        } else {
            emit(
                &app,
                "verify",
                "info",
                "Local archive — validating structure and resource limits",
                None,
            );
        }

        // ── 3. Unpack the .tgz ─────────────────────────────────────────────
        emit(&app, "prepare", "info", "Unpacking update bundle…", None);
        let bundle = firmware::unpack_update_archive(&bytes)?;
        let dir = format!("/ext/update/{}", bundle.top_dir);
        let manifest_path = format!("{dir}/{}", bundle.manifest_rel);
        let total = bundle.total_bytes();
        emit(
            &app,
            "prepare",
            "ok",
            format!("{} files · {}", bundle.files.len(), human_size(total)),
            None,
        );

        // ── 4. Device IO: upload → stage update → reboot ───────────────────
        // Lock the client manually (rather than `with_client`) so we can drop
        // it after the reboot, matching the `reboot` command — the serial port
        // vanishes the instant the device reboots into the updater.
        {
            let conn_mode = mode_mutex.lock().unwrap();
            if *conn_mode == ConnectionMode::Cli {
                return Err(FlipperError::CliModeActive);
            }
            drop(conn_mode);
        }
        let mut guard = client_mutex.lock().unwrap();
        let device_result: Result<()> = (|| {
            let client = guard.as_mut().ok_or(FlipperError::NotConnected)?;
            ensure_not_cancelled(&is_cancelled)?;

            if options.clean {
                ensure_not_cancelled(&is_cancelled)?;
                emit(&app, "upload", "info", "Clearing /ext/update…", None);
                ignore_not_exist(storage::storage_delete(client, &dir, true))?;
            }

            ensure_not_cancelled(&is_cancelled)?;
            ignore_already_exists(storage::storage_mkdir(client, "/ext/update"))?;
            ensure_not_cancelled(&is_cancelled)?;
            ignore_already_exists(storage::storage_mkdir(client, &dir))?;

            let mut made_dirs: HashSet<String> = HashSet::new();
            let mut done: u64 = 0;
            let last_up_pct = Cell::new(-1i32);
            for file in &bundle.files {
                ensure_not_cancelled(&is_cancelled)?;
                ensure_parent_dirs(
                    client,
                    &dir,
                    &file.rel_path,
                    &mut made_dirs,
                    &is_cancelled,
                )?;
                ensure_not_cancelled(&is_cancelled)?;
                let remote = format!("{dir}/{}", file.rel_path);
                emit(
                    &app,
                    "upload",
                    "info",
                    format!(
                        "↑ {}  ({})",
                        file.rel_path,
                        human_size(file.data.len() as u64)
                    ),
                    Some(overall_pct(done, total)),
                );
                let start = done;
                let app_up = app.clone();
                storage::storage_write(
                    client,
                    &remote,
                    &file.data,
                    |sent, _| {
                        let cur = start.saturating_add(sent as u64);
                        let pct = overall_pct(cur, total) as i32;
                        if pct != last_up_pct.get() {
                            last_up_pct.set(pct);
                            emit(&app_up, "upload", "info", "", Some(pct as u32));
                        }
                    },
                    is_cancelled,
                )?;
                ensure_not_cancelled(&is_cancelled)?;
                done = done.saturating_add(file.data.len() as u64);
            }
            ensure_not_cancelled(&is_cancelled)?;
            emit(&app, "upload", "ok", "Upload complete", Some(100));

            // Atomically cross the irreversible boundary immediately before
            // the updater request. A concurrent cancel either marks the
            // operation first and prevents this call, or observes Committing
            // and reports that it is too late. Once committed, request_update
            // and reboot are one uninterrupted sequence.
            ensure_not_cancelled(&is_cancelled)?;
            emit(&app, "install", "info", "Sending update request…", None);
            begin_firmware_commit(&firmware_operation, operation_id)?;
            let code = session::request_update(client, &manifest_path)?;
            if code != 0 {
                let msg = update_code_message(code);
                emit(&app, "install", "error", msg.clone(), None);
                return Err(FlipperError::Internal(msg));
            }
            emit(&app, "install", "ok", "Update staged on device", None);

            emit(&app, "reboot", "info", "Rebooting into updater…", None);
            let reboot_result = reboot_into_updater(&mut guard);
            if let Err(error) = reboot_result {
                emit(
                    &app,
                    "reboot",
                    "error",
                    "Updater start is indeterminate — check the device screen and reconnect before retrying",
                    None,
                );
                return Err(error);
            }
            Ok(())
        })();
        drop(guard);

        if let Err(error) = &device_result {
            if is_fatal_device_error(error) {
                tracing::warn!("tearing down connection after firmware RPC failure: {error}");
                diag::log_event("FirmwareConnectionTornDown", error.to_string());
                if let Ok(mut guard) = client_mutex.lock() {
                    *guard = None;
                }
                if let Ok(mut tx_guard) = ble_cancel_tx.lock() {
                    if let Some(tx) = tx_guard.take() {
                        let _ = tx.send(());
                    }
                }
                let _ = app.emit("flipper-disconnected", error.to_string());
            }
        }
        device_result?;

        emit(
            &app,
            "done",
            "ok",
            "Updater started — follow progress on the device screen; installation is not yet verified",
            Some(100),
        );
        Ok(())
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

fn short_name(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use crate::flipper::client::FlipperClient;
    use crate::flipper::transport::{Transport, TransportKind};

    use super::{
        ensure_not_cancelled, ignore_not_exist, is_fatal_device_error, reboot_into_updater,
        FlashOptions, FlashSource,
    };
    use crate::error::FlipperError;

    struct FailingWriteTransport;

    impl Transport for FailingWriteTransport {
        fn read_exact(&mut self, _buf: &mut [u8]) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no reads"))
        }

        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }

        fn write_all(&mut self, _buf: &[u8]) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "reboot write failed",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn set_timeout(&mut self, _dur: Duration) -> io::Result<()> {
            Ok(())
        }

        fn unread(&mut self, _bytes: &[u8]) {}

        fn kind(&self) -> TransportKind {
            TransportKind::Serial
        }
    }

    #[test]
    fn remote_flash_source_accepts_only_catalog_identity() {
        let selected = serde_json::json!({
            "kind": "remote",
            "provider_id": "official",
            "channel_id": "release",
            "version": "1.4.3",
            "timestamp": 1_765_000_000u64,
            "selection_token": "a".repeat(64),
        });
        assert!(serde_json::from_value::<FlashSource>(selected).is_ok());
    }

    #[test]
    fn remote_flash_source_rejects_webview_url_and_checksum() {
        let untrusted = serde_json::json!({
            "kind": "remote",
            "provider_id": "official",
            "channel_id": "release",
            "version": "1.4.3",
            "timestamp": 1_765_000_000u64,
            "selection_token": "a".repeat(64),
            "url": "https://evil.test/firmware.tgz",
            "sha256": "a".repeat(64),
            "label": "untrusted",
        });
        assert!(serde_json::from_value::<FlashSource>(untrusted).is_err());
    }

    #[test]
    fn flash_options_reject_stale_security_fields() {
        assert!(serde_json::from_value::<FlashOptions>(serde_json::json!({
            "clean": true,
        }))
        .is_ok());

        let stale = serde_json::json!({
            "clean": true,
            "verify": false,
            "url": "https://evil.test/firmware.tgz",
            "sha256": "a".repeat(64),
        });
        assert!(serde_json::from_value::<FlashOptions>(stale).is_err());
    }

    #[test]
    fn updater_reboot_propagates_write_failure_and_clears_client() {
        let mut client = Some(FlipperClient::new(Box::new(FailingWriteTransport)));
        let result = reboot_into_updater(&mut client);
        assert!(result.is_err());
        assert!(client.is_none());
    }

    #[test]
    fn cancellation_barrier_prevents_the_next_mutation_stage() {
        assert!(ensure_not_cancelled(&|| false).is_ok());
        assert!(matches!(
            ensure_not_cancelled(&|| true),
            Err(FlipperError::TransferCancelled)
        ));
    }

    #[test]
    fn clean_delete_ignores_only_storage_not_exist() {
        assert!(ignore_not_exist(Err(FlipperError::Rpc {
            status: 7,
            command_id: 1,
        }))
        .is_ok());
        for status in [5, 6, 8, 9, 10, 11] {
            assert!(ignore_not_exist(Err(FlipperError::Rpc {
                status,
                command_id: 1,
            }))
            .is_err());
        }
    }

    #[test]
    fn fatal_device_error_classifier_matches_transport_failures() {
        assert!(is_fatal_device_error(&FlipperError::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "gone",
        ))));
        assert!(!is_fatal_device_error(&FlipperError::TransferCancelled));
        assert!(!is_fatal_device_error(&FlipperError::Rpc {
            status: 9,
            command_id: 1,
        }));
    }
}
