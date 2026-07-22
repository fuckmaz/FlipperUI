use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use std::collections::HashMap;

use crate::commands::client::{
    connection_handle, execute_connection, retire_connection_owner, with_connection,
};
use crate::error::{FlipperError, Result};
use crate::flipper::ble::{connection::connect_ble, scanner};
use crate::flipper::capabilities::{normalize_device_handshake, DeviceCapabilities};
use crate::flipper::connection_actor::ConnectionHandle;
use crate::flipper::session;
use crate::flipper::transport::TransportKind;
use crate::pb_system;
use crate::state::{AppState, BleSessionCompletion, CliOutputGate};

#[derive(Serialize, Deserialize)]
pub struct PortInfo {
    pub name: String,
    pub is_flipper: bool,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub manufacturer: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct DeviceInfo {
    pub port: String,
    pub hardware_name: Option<String>,
    pub hardware_version: Option<String>,
    /// STM32 unique ID — stable per device, used as the cache key for
    /// per-device state like the Sub-GHz library index.
    pub hardware_uid: Option<String>,
    pub firmware_version: Option<String>,
    pub firmware_build_date: Option<String>,
    pub capabilities: DeviceCapabilities,
}

fn device_info_from_handshake(
    port: String,
    info_map: &HashMap<String, String>,
    transport: TransportKind,
) -> Result<DeviceInfo> {
    let handshake = normalize_device_handshake(info_map, transport)?;
    Ok(DeviceInfo {
        port,
        hardware_name: handshake.hardware_name,
        hardware_version: handshake.hardware_version,
        hardware_uid: Some(handshake.hardware_uid),
        firmware_version: Some(handshake.firmware_version),
        firmware_build_date: handshake.firmware_build_date,
        capabilities: handshake.capabilities,
    })
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct DeviceDateTime {
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub day: u32,
    pub month: u32,
    pub year: u32,
    /// Weekday uses the Flipper/ISO shape: Monday = 1, Sunday = 7.
    pub weekday: u32,
}

impl DeviceDateTime {
    fn validate(self) -> Result<()> {
        validate_range("hour", self.hour, 0, 23)?;
        validate_range("minute", self.minute, 0, 59)?;
        validate_range("second", self.second, 0, 59)?;
        validate_range("month", self.month, 1, 12)?;
        validate_range("year", self.year, 2000, 2099)?;
        validate_range("weekday", self.weekday, 1, 7)?;
        let max_day = days_in_month(self.year, self.month);
        validate_range("day", self.day, 1, max_day)?;
        Ok(())
    }

    fn into_proto(self) -> pb_system::DateTime {
        pb_system::DateTime {
            hour: self.hour,
            minute: self.minute,
            second: self.second,
            day: self.day,
            month: self.month,
            year: self.year,
            weekday: self.weekday,
        }
    }
}

fn validate_range(label: &str, value: u32, min: u32, max: u32) -> Result<()> {
    if value < min || value > max {
        return Err(FlipperError::Session(format!(
            "Invalid datetime {label}: expected {min}..={max}, got {value}",
        )));
    }
    Ok(())
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 31,
    }
}

fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

async fn shutdown_live_owner(
    owner: &Arc<std::sync::Mutex<Option<ConnectionHandle>>>,
    output_gate: &Arc<std::sync::Mutex<CliOutputGate>>,
) {
    let handle = {
        let mut gate = output_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        gate.invalidate();
        owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    };
    if let Some(handle) = handle {
        let _ = handle.shutdown().await;
    }
}

fn monitor_connection_owner(
    handle: ConnectionHandle,
    owner: Arc<std::sync::Mutex<Option<ConnectionHandle>>>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    ble_cancel_tx: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    app: AppHandle,
) {
    tauri::async_runtime::spawn(async move {
        handle.wait_until_closed().await;
        let _lifecycle = lifecycle.lock().await;
        let fatal_owner = retire_connection_owner(&owner, &handle);
        if fatal_owner {
            if handle.transport_kind() == TransportKind::Ble {
                let cancel = ble_cancel_tx
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                if let Some(cancel) = cancel {
                    let _ = cancel.send(());
                }
            }
            let _ = app.emit(
                "flipper-disconnected",
                "The device connection closed unexpectedly",
            );
        }
    });
}

fn next_ble_session(generation: &std::sync::atomic::AtomicU64) -> u64 {
    let next = generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    if next == 0 {
        generation.store(1, Ordering::Release);
        1
    } else {
        next
    }
}

async fn shutdown_ble_session(state: &AppState) {
    // Invalidate before cancellation so a completed old task can never acquire
    // lifecycle later and mutate a replacement connection.
    next_ble_session(&state.ble_session_generation);
    let cancel = state
        .ble_cancel_tx
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let completion = state
        .ble_session_completion
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(cancel) = cancel {
        let _ = cancel.send(());
    }
    if let Some(completion) = completion {
        match tokio::time::timeout(std::time::Duration::from_secs(3), completion.completed).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                tracing::debug!(
                    session_id = completion.id,
                    "BLE task completion sender dropped"
                );
            }
            Err(_) => {
                tracing::warn!(
                    session_id = completion.id,
                    "timed out waiting for BLE task teardown"
                );
            }
        }
    }
}

/// Flipper Zero's USB VID/PID — the device exposes itself as a STM32 virtual
/// COM port. Used as the cross-platform identifier so we don't auto-connect to
/// random Bluetooth virtual ports, modems, or vendor serial dongles.
const FLIPPER_USB_VID: u16 = 0x0483;
const FLIPPER_USB_PID: u16 = 0x5740;

/// List serial ports, marking Flipper Zero ports via USB VID/PID. On macOS we
/// additionally drop non-Flipper ports entirely (the `usbmodemflip*` naming
/// gives us a stable filter), since the picker on macOS is Flipper-only and
/// surfacing the system's other tty devices is just noise. On Windows / Linux
/// we keep every port in the list but only mark the Flipper as connectable —
/// auto-connect logic on the frontend keys off `is_flipper`, so a stray COM
/// port can't trigger a connection retry loop.
#[tauri::command]
pub fn list_ports() -> Result<Vec<PortInfo>> {
    // Note: list_ports is kept synchronous because serialport::available_ports()
    // is typically fast (~10-50ms). If this becomes a bottleneck, we can move it
    // to spawn_blocking later.
    let ports = serialport::available_ports()?;
    Ok(ports
        .into_iter()
        .filter_map(|p| {
            let (vid, pid, manufacturer) = match &p.port_type {
                serialport::SerialPortType::UsbPort(usb) => {
                    (Some(usb.vid), Some(usb.pid), usb.manufacturer.clone())
                }
                _ => (None, None, None),
            };
            let is_flipper = matches!((vid, pid), (Some(FLIPPER_USB_VID), Some(FLIPPER_USB_PID)));

            // Belt-and-braces fallback: some macOS USB stacks return the port
            // without a populated VID/PID on first enumeration, so accept the
            // historical name-based match as a fallback there.
            let is_flipper = is_flipper
                || (cfg!(target_os = "macos")
                    && p.port_name.to_lowercase().contains("usbmodemflip"));

            // On macOS, hide non-Flipper ports outright to keep the picker
            // clean. On Windows / Linux we keep them visible but un-flipped
            // — the user can still see what's plugged in, but auto-connect
            // won't target them.
            if cfg!(target_os = "macos") && !is_flipper {
                return None;
            }

            Some(PortInfo {
                name: p.port_name,
                is_flipper,
                vid,
                pid,
                manufacturer,
            })
        })
        .collect())
}

/// Open a connection to the Flipper Zero on the given port.
#[tauri::command]
pub async fn connect(
    port: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<DeviceInfo> {
    let lifecycle = Arc::clone(&state.connection_lifecycle);
    let _lifecycle = lifecycle.lock().await;
    shutdown_live_owner(&state.connection_owner, &state.cli_output_gate).await;
    shutdown_ble_session(&state).await;
    let (info, client) = tauri::async_runtime::spawn_blocking(move || {
        tracing::info!(%port, "connect: starting RPC session");
        let mut client = match session::open_session(&port) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(%port, error = %e, "connect: open_session failed");
                return Err(e);
            }
        };
        let info_map = session::get_device_info(&mut client)?;
        tracing::info!(
            %port,
            hardware = info_map.get("hardware_name").map(|s| s.as_str()).unwrap_or("?"),
            firmware = info_map.get("software_version").map(|s| s.as_str()).unwrap_or("?"),
            "connect: connected",
        );

        let info = device_info_from_handshake(port, &info_map, TransportKind::Serial)?;
        Ok((info, client))
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))??;

    let owner =
        ConnectionHandle::spawn(client).map_err(crate::commands::client::map_actor_error)?;
    *state
        .connection_owner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(owner.clone());
    monitor_connection_owner(
        owner,
        Arc::clone(&state.connection_owner),
        Arc::clone(&state.connection_lifecycle),
        Arc::clone(&state.ble_cancel_tx),
        app,
    );
    Ok(info)
}

/// List discoverable Flipper Zero devices over BLE.
#[tauri::command]
pub async fn list_ble_devices() -> Result<Vec<scanner::BleDevice>> {
    tauri::async_runtime::spawn_blocking(scanner::list_ble_devices_blocking)
        .await
        .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Start a live BLE scan that emits `ble-scan-device` events as Flipper
/// peripherals are seen, until the matching `stop_ble_scan` is called. Calling
/// this while a scan is already running is a no-op.
#[tauri::command]
pub async fn start_ble_scan(app: AppHandle, state: State<'_, AppState>) -> Result<()> {
    let cancel = Arc::clone(&state.ble_scan_active);
    // swap returns the previous value — if true, a scan is already running and
    // we leave it alone (the second start would be racing the first on the same
    // adapter's event stream).
    if cancel.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let app_handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = scanner::live_scan_blocking(app_handle, cancel) {
            tracing::warn!("BLE live scan ended with error: {e}");
        }
    });
    Ok(())
}

/// Stop the live BLE scan started by `start_ble_scan`. Idempotent.
#[tauri::command]
pub async fn stop_ble_scan(state: State<'_, AppState>) -> Result<()> {
    state.ble_scan_active.store(false, Ordering::Relaxed);
    Ok(())
}

/// Open a BLE connection to the Flipper Zero identified by `id` (from `list_ble_devices`).
#[tauri::command]
pub async fn connect_ble_device(
    id: String,
    name: Option<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<DeviceInfo> {
    let lifecycle = Arc::clone(&state.connection_lifecycle);
    let _lifecycle = lifecycle.lock().await;
    shutdown_live_owner(&state.connection_owner, &state.cli_output_gate).await;
    shutdown_ble_session(&state).await;
    let ble_cancel_tx = Arc::clone(&state.ble_cancel_tx);
    let ble_session_generation = Arc::clone(&state.ble_session_generation);
    let ble_session_completion = Arc::clone(&state.ble_session_completion);
    let session_id = next_ble_session(&state.ble_session_generation);

    let connection_app = app.clone();
    let startup_generation = Arc::clone(&ble_session_generation);
    let (info, client, cancel, completed) = tauri::async_runtime::spawn_blocking(move || {
        let connection = connect_ble(id, connection_app, session_id)?;
        let mut client = connection.client;
        let startup = session::get_device_info(&mut client).and_then(|info_map| {
            device_info_from_handshake(
                name.unwrap_or_else(|| "BLE".into()),
                &info_map,
                TransportKind::Ble,
            )
        });
        let info = match startup {
            Ok(info) => info,
            Err(error) => {
                // Startup is atomic: a connection that cannot complete its
                // first RPC or validate required handshake facts is cancelled
                // and fully torn down, never published.
                next_ble_session(&startup_generation);
                let _ = connection.cancel.send(());
                let _ = crate::flipper::ble::runtime::BLE_RT.block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(3), connection.completed)
                        .await
                });
                return Err(error);
            }
        };
        Ok((info, client, connection.cancel, connection.completed))
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))??;

    let owner = match ConnectionHandle::spawn(client) {
        Ok(owner) => owner,
        Err(error) => {
            next_ble_session(&ble_session_generation);
            let _ = cancel.send(());
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), completed).await;
            return Err(crate::commands::client::map_actor_error(error));
        }
    };
    *state
        .connection_owner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(owner.clone());
    *ble_cancel_tx
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(cancel);
    *ble_session_completion
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(BleSessionCompletion {
        id: session_id,
        completed,
    });
    monitor_connection_owner(
        owner,
        Arc::clone(&state.connection_owner),
        Arc::clone(&state.connection_lifecycle),
        Arc::clone(&state.ble_cancel_tx),
        app,
    );
    Ok(info)
}

/// Close the current connection to the Flipper Zero.
#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>) -> Result<()> {
    let lifecycle = Arc::clone(&state.connection_lifecycle);
    let _lifecycle = lifecycle.lock().await;
    shutdown_live_owner(&state.connection_owner, &state.cli_output_gate).await;
    shutdown_ble_session(&state).await;
    Ok(())
}

/// Return the kind of the active connection ("serial" | "ble"), or `None` if not connected.
#[tauri::command]
pub async fn connection_kind(state: State<'_, AppState>) -> Result<Option<TransportKind>> {
    let lifecycle = Arc::clone(&state.connection_lifecycle);
    let _lifecycle = lifecycle.lock().await;
    Ok(connection_handle(&state.connection_owner)
        .ok()
        .map(|owner| owner.transport_kind()))
}

/// Get the full device info map from the Flipper — every key/value pair
/// the firmware exposes (hardware_*, firmware_*, radio_*, etc.). Much richer
/// than the subset we squeeze into [`DeviceInfo`] on connect.
#[tauri::command]
pub async fn device_info_all(state: State<'_, AppState>) -> Result<HashMap<String, String>> {
    with_connection(
        Arc::clone(&state.connection_owner),
        session::get_device_info,
    )
    .await
}

/// Get power/battery info from the Flipper.
/// Returns a key-value map (e.g. "charge", "voltage", "current", "temperature").
#[tauri::command]
pub async fn power_info(state: State<'_, AppState>) -> Result<HashMap<String, String>> {
    with_connection(Arc::clone(&state.connection_owner), session::get_power_info).await
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::{next_ble_session, shutdown_ble_session, shutdown_live_owner};
    use crate::commands::client::retire_connection_owner;
    use crate::flipper::client::FlipperClient;
    use crate::flipper::connection_actor::ConnectionHandle;
    use crate::flipper::transport::{Transport, TransportKind};
    use crate::state::{AppState, BleSessionCompletion, CliOutputGate};

    struct DropProbeTransport(Arc<AtomicBool>);

    impl Drop for DropProbeTransport {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    impl Transport for DropProbeTransport {
        fn read_exact(&mut self, _buffer: &mut [u8]) -> io::Result<()> {
            Err(io::ErrorKind::TimedOut.into())
        }

        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::TimedOut.into())
        }

        fn write_all(&mut self, _bytes: &[u8]) -> io::Result<()> {
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn set_timeout(&mut self, _duration: Duration) -> io::Result<()> {
            Ok(())
        }

        fn unread(&mut self, _bytes: &[u8]) {}

        fn kind(&self) -> TransportKind {
            TransportKind::Serial
        }
    }

    #[tokio::test]
    async fn device_cleanup_invalidates_cli_output_and_drops_live_actor_owner() {
        let dropped = Arc::new(AtomicBool::new(false));
        let owner = ConnectionHandle::spawn(FlipperClient::new(Box::new(DropProbeTransport(
            Arc::clone(&dropped),
        ))))
        .unwrap();
        let slot = Arc::new(Mutex::new(Some(owner)));
        let gate = Arc::new(Mutex::new(CliOutputGate::default()));
        let generation = gate.lock().unwrap().begin_session();

        shutdown_live_owner(&slot, &gate).await;

        assert!(slot.lock().unwrap().is_none());
        assert!(!gate.lock().unwrap().is_current(generation));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn fatal_owner_retirement_is_exactly_once_and_rejects_stale_actor() {
        let first = ConnectionHandle::spawn(FlipperClient::new(Box::new(DropProbeTransport(
            Arc::new(AtomicBool::new(false)),
        ))))
        .unwrap();
        let replacement = ConnectionHandle::spawn(FlipperClient::new(Box::new(
            DropProbeTransport(Arc::new(AtomicBool::new(false))),
        )))
        .unwrap();
        let slot = Mutex::new(Some(first.clone()));

        assert!(retire_connection_owner(&slot, &first));
        assert!(!retire_connection_owner(&slot, &first));

        *slot.lock().unwrap() = Some(replacement.clone());
        assert!(!retire_connection_owner(&slot, &first));
        assert!(slot
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|owner| owner.same_connection(&replacement)));

        first.shutdown().await.unwrap();
        replacement.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ble_replacement_invalidates_cancels_and_awaits_owned_task() {
        let state = AppState::new();
        let session_id = next_ble_session(&state.ble_session_generation);
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        *state.ble_cancel_tx.lock().unwrap() = Some(cancel_tx);
        *state.ble_session_completion.lock().unwrap() = Some(BleSessionCompletion {
            id: session_id,
            completed: completed_rx,
        });
        let cleaned = Arc::new(AtomicBool::new(false));
        let task_cleaned = Arc::clone(&cleaned);
        tokio::spawn(async move {
            let _ = cancel_rx.await;
            task_cleaned.store(true, Ordering::Release);
            let _ = completed_tx.send(());
        });

        shutdown_ble_session(&state).await;

        assert_ne!(
            state.ble_session_generation.load(Ordering::Acquire),
            session_id
        );
        assert!(state.ble_cancel_tx.lock().unwrap().is_none());
        assert!(state.ble_session_completion.lock().unwrap().is_none());
        assert!(cleaned.load(Ordering::Acquire));
    }
}

/// Ping the device and return the round-trip latency in milliseconds.
#[tauri::command]
pub async fn ping(state: State<'_, AppState>) -> Result<u32> {
    with_connection(Arc::clone(&state.connection_owner), |client| {
        let started = std::time::Instant::now();
        session::ping(client)?;
        Ok(started.elapsed().as_millis().min(u32::MAX as u128) as u32)
    })
    .await
}

/// Sync the Flipper Zero RTC to a host-provided local date/time.
#[tauri::command]
pub async fn sync_clock(datetime: DeviceDateTime, state: State<'_, AppState>) -> Result<()> {
    with_connection(Arc::clone(&state.connection_owner), move |client| {
        datetime.validate()?;
        session::set_datetime(client, datetime.into_proto())
    })
    .await
}

/// Reboot the Flipper Zero.
/// mode: 0 = OS (normal reboot), 1 = DFU, 2 = UPDATE
#[tauri::command]
pub async fn reboot(mode: i32, state: State<'_, AppState>) -> Result<()> {
    let handle = connection_handle(&state.connection_owner)?;
    let result = execute_connection(&handle, move |client| session::reboot(client, mode)).await;
    let lifecycle = Arc::clone(&state.connection_lifecycle);
    let _lifecycle = lifecycle.lock().await;
    if retire_connection_owner(&state.connection_owner, &handle) {
        let _ = handle.shutdown().await;
        if handle.transport_kind() == TransportKind::Ble {
            let cancel = state
                .ble_cancel_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(cancel) = cancel {
                let _ = cancel.send(());
            }
        }
    }
    result
}
