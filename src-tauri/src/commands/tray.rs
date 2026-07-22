//! Runtime toggles for the system-tray icon and (on macOS) the dock icon.
//!
//! The frontend persists these preferences via `tauri-plugin-store` and then
//! calls into here to apply them. The tray is created at startup by default;
//! these commands add the ability to remove/re-install it and to switch the
//! macOS activation policy between `Regular` (normal dock presence) and
//! `Accessory` (no dock icon, menubar-only style).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::Deserialize;
use tauri::AppHandle;

use crate::error::{FlipperError, Result};
use crate::{build_tray_menu, install_tray, tray_icon_for, TRAY_ID};

/// Snapshot of device-side state mirrored into the tray menu. The frontend
/// pushes this whenever the connection state or battery changes; the tray
/// rebuilds its menu so users can see basic info without opening the window.
#[derive(Clone, Default, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrayStatus {
    pub connected: bool,
    pub device_name: Option<String>,
    pub firmware_version: Option<String>,
    pub battery_charge: Option<u8>,
    pub battery_charging: bool,
}

/// Latest pushed status, used to rebuild the menu after a tray re-install
/// (e.g. when the user toggles tray off → on in Settings).
static TRAY_STATUS: Mutex<Option<TrayStatus>> = Mutex::new(None);

pub fn tray_status() -> TrayStatus {
    TRAY_STATUS
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

/// Process-wide desired state for the monochrome tray icon. Read by
/// `install_tray` (so a re-install after `set_tray_enabled(false)` →
/// `set_tray_enabled(true)` keeps the chosen style) and updated by
/// `set_tray_monochrome`.
static TRAY_MONOCHROME: AtomicBool = AtomicBool::new(false);

pub fn tray_monochrome() -> bool {
    TRAY_MONOCHROME.load(Ordering::Relaxed)
}

/// Show or hide the system-tray icon. Idempotent: toggling to the current
/// state is a no-op.
#[tauri::command]
pub fn set_tray_enabled(app: AppHandle, enabled: bool) -> Result<()> {
    if enabled {
        // `remove_tray_by_id` returns None if no tray exists with that id, so
        // we can't use its presence to know whether to install — instead we
        // rely on `install_tray` being fine to call when none exists.
        if app.tray_by_id(TRAY_ID).is_none() {
            install_tray(&app, tray_monochrome())
                .map_err(|error| FlipperError::Internal(error.to_string()))?;
        }
    } else {
        let _ = app.remove_tray_by_id(TRAY_ID);
    }
    Ok(())
}

/// Toggle the macOS dock icon. On non-macOS platforms this is a no-op so the
/// frontend can call it unconditionally.
#[tauri::command]
pub fn set_dock_visible(app: AppHandle, visible: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let policy = if visible {
            tauri::ActivationPolicy::Regular
        } else {
            tauri::ActivationPolicy::Accessory
        };
        app.set_activation_policy(policy)
            .map_err(|error| FlipperError::Internal(error.to_string()))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, visible);
    }
    Ok(())
}

/// Push the latest device status into the tray. Rebuilds the tray menu so the
/// status header (device name, battery) reflects current state. Safe to call
/// when the tray is disabled — we cache the status and replay it on the next
/// install.
#[tauri::command]
pub fn update_tray_status(app: AppHandle, status: TrayStatus) -> Result<()> {
    {
        let mut guard = TRAY_STATUS
            .lock()
            .map_err(|error| FlipperError::Internal(error.to_string()))?;
        *guard = Some(status.clone());
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let menu = build_tray_menu(&app, &status)
            .map_err(|error| FlipperError::Internal(error.to_string()))?;
        tray.set_menu(Some(menu))
            .map_err(|error| FlipperError::Internal(error.to_string()))?;
    }
    Ok(())
}

/// Swap the tray icon between the normal full-color art and a monochrome
/// glyph that adopts the menubar's foreground color (template image on
/// macOS). Persists for the rest of the process so any future re-install
/// (after toggling tray off → on) keeps the chosen style.
#[tauri::command]
pub fn set_tray_monochrome(app: AppHandle, monochrome: bool) -> Result<()> {
    TRAY_MONOCHROME.store(monochrome, Ordering::Relaxed);
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let icon = tray_icon_for(&app, monochrome)
            .map_err(|error| FlipperError::Internal(error.to_string()))?;
        tray.set_icon(Some(icon))
            .map_err(|error| FlipperError::Internal(error.to_string()))?;
        // Template flag controls macOS auto-tinting; setting it on non-mac
        // platforms is harmless.
        let _ = tray.set_icon_as_template(monochrome);
    }
    Ok(())
}
