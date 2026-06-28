//! Tauri commands for the firmware-flash tool.
//!
//! `firmware_providers` / `firmware_fetch_directory` back the source picker;
//! `firmware_flash` runs the whole self-update pipeline (download → verify →
//! unpack → upload to `/ext/update` → `SystemUpdateRequest` → reboot into the
//! on-device updater), streaming `firmware-flash-progress` events the modal
//! renders as a live console.

use std::cell::Cell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::firmware;
use crate::flipper::{session, storage};
use crate::state::{AppState, ConnectionMode};

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

/// Where the bundle comes from.
#[derive(Deserialize)]
pub struct FlashSource {
    /// "remote" (download `url`) or "local" (read `local_path`).
    kind: String,
    url: Option<String>,
    sha256: Option<String>,
    local_path: Option<String>,
    /// Short label for the log header, e.g. "Official · Release 1.4.3".
    label: Option<String>,
}

#[derive(Deserialize)]
pub struct FlashOptions {
    /// Verify the download against the source's SHA-256 (when one is known).
    verify: bool,
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

fn begin_transfer(generation: &AtomicU64) -> u64 {
    generation.fetch_add(1, Ordering::Relaxed) + 1
}

fn transfer_cancelled(cancelled_generation: &AtomicU64, generation: u64) -> bool {
    cancelled_generation.load(Ordering::Relaxed) == generation
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
) -> Result<()> {
    let Some((dir_part, _)) = rel_path.rsplit_once('/') else {
        return Ok(());
    };
    let mut acc = base.to_string();
    for seg in dir_part.split('/') {
        acc.push('/');
        acc.push_str(seg);
        if made.insert(acc.clone()) {
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
    tauri::async_runtime::spawn_blocking(move || firmware::fetch_catalog(p.id, p.directory_url))
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
    let generation = begin_transfer(&state.transfer_generation);
    let cancelled_generation = Arc::clone(&state.transfer_cancelled_generation);

    tauri::async_runtime::spawn_blocking(move || {
        let is_cancelled = || transfer_cancelled(&cancelled_generation, generation);
        let label = source
            .label
            .clone()
            .unwrap_or_else(|| "firmware".to_string());

        // ── 1. Acquire the bundle bytes ────────────────────────────────────
        let bytes: Vec<u8> = if source.kind == "local" {
            let path = source
                .local_path
                .clone()
                .ok_or_else(|| FlipperError::Internal("no local file selected".into()))?;
            emit(&app, "download", "info", format!("Reading {path}"), None);
            let data = std::fs::read(&path)?;
            emit(
                &app,
                "download",
                "ok",
                format!(
                    "Loaded {} ({})",
                    short_name(&path),
                    human_size(data.len() as u64)
                ),
                Some(100),
            );
            data
        } else {
            let url = source
                .url
                .clone()
                .ok_or_else(|| FlipperError::Internal("no download URL".into()))?;
            emit(
                &app,
                "download",
                "info",
                format!("Downloading {label}"),
                Some(0),
            );
            let last_pct = Cell::new(-1i32);
            let app_dl = app.clone();
            let data = firmware::download(
                &url,
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
            data
        };

        if is_cancelled() {
            return Err(FlipperError::TransferCancelled);
        }

        // ── 2. Verify checksum ─────────────────────────────────────────────
        if options.verify {
            match source.sha256.as_deref().filter(|s| !s.is_empty()) {
                Some(expected) => {
                    emit(&app, "verify", "info", "Verifying SHA-256…", None);
                    let actual = firmware::sha256_hex(&bytes);
                    if !actual.eq_ignore_ascii_case(expected) {
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
                }
                None => emit(
                    &app,
                    "verify",
                    "warn",
                    "No checksum available — skipped",
                    None,
                ),
            }
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
        let client = guard.as_mut().ok_or(FlipperError::NotConnected)?;

        if options.clean {
            emit(&app, "upload", "info", "Clearing /ext/update…", None);
            let _ = storage::storage_delete(client, &dir, true); // ignore: may not exist
        }

        ignore_already_exists(storage::storage_mkdir(client, "/ext/update"))?;
        ignore_already_exists(storage::storage_mkdir(client, &dir))?;

        let mut made_dirs: HashSet<String> = HashSet::new();
        let mut done: u64 = 0;
        let last_up_pct = Cell::new(-1i32);
        for file in &bundle.files {
            if is_cancelled() {
                return Err(FlipperError::TransferCancelled);
            }
            ensure_parent_dirs(client, &dir, &file.rel_path, &mut made_dirs)?;
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
                &is_cancelled,
            )?;
            done = done.saturating_add(file.data.len() as u64);
        }
        emit(&app, "upload", "ok", "Upload complete", Some(100));

        // Stage the update. The device validates the bundle here but does not
        // apply anything until the reboot below.
        emit(&app, "install", "info", "Sending update request…", None);
        let code = session::request_update(client, &manifest_path)?;
        if code != 0 {
            let msg = update_code_message(code);
            emit(&app, "install", "error", msg.clone(), None);
            return Err(FlipperError::Internal(msg));
        }
        emit(&app, "install", "ok", "Update staged on device", None);

        // Reboot into the updater. The device disconnects; drop the client so
        // the UI reflects reality and a reconnect starts clean.
        emit(&app, "reboot", "info", "Rebooting into updater…", None);
        let _ = session::reboot(client, 2); // UPDATE
        *guard = None;
        drop(guard);

        emit(
            &app,
            "done",
            "ok",
            "Flipper is applying the update — follow progress on the device screen",
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
