//! BLE connection handshake — resolve peripheral, subscribe to characteristics,
//! spin up the notification dispatch task, and hand a [`FlipperClient`] back.

use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use btleplug::api::{
    Central, CentralEvent, CharPropFlags, Characteristic, Peripheral as _, ValueNotification,
};
use btleplug::platform::Peripheral;
use futures::{Stream, StreamExt};
use tauri::{AppHandle, Manager};
use tokio::sync::oneshot;

use crate::error::{FlipperError, Result};
use crate::flipper::ble::runtime::{shared_adapter, BLE_RT};
use crate::flipper::ble::transport::{BleTransport, RxShared};
use crate::flipper::ble::{OVERFLOW_CHAR, RPC_STATE_CHAR, RX_CHAR, TX_CHAR};
use crate::flipper::client::FlipperClient;
use crate::flipper::connection_actor::ConnectionHandle;
use crate::state::{classify_ble_task, BleTaskDisposition};

fn map_btle_err(err: impl std::fmt::Display) -> FlipperError {
    let s = err.to_string();
    let lower = s.to_lowercase();
    if lower.contains("not paired")
        || lower.contains("authentication")
        || lower.contains("0x800700b7")
        || lower.contains("pairing")
    {
        FlipperError::BlePairingRequired(s)
    } else {
        FlipperError::Internal(format!("BLE error: {s}"))
    }
}

/// Decode a FLOW_CTRL payload into a free-bytes count. Flipper firmware has
/// shipped this as both `uint16_t` (older) and `uint32_t` (newer); accept both.
fn parse_flow_ctrl(bytes: &[u8]) -> Option<u32> {
    match bytes.len() {
        2 => Some(u16::from_le_bytes([bytes[0], bytes[1]]) as u32),
        4 => Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        _ => None,
    }
}

/// `PeripheralId` has no public `FromStr`, so we resolve the frontend-supplied
/// id by walking the adapter's peripheral cache and matching stringified ids.
async fn find_by_id(adapter: &btleplug::platform::Adapter, id: &str) -> Result<Option<Peripheral>> {
    let peripherals = adapter.peripherals().await.map_err(map_btle_err)?;
    for p in peripherals {
        if p.id().to_string() == id {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// Event-driven fallback scan. Runs an unfiltered scan (Flipper doesn't
/// advertise its Serial service UUID in the primary advertisement) and breaks
/// as soon as the target id turns up in a DeviceDiscovered/DeviceUpdated
/// event. Bounded at `SCAN_DEADLINE` so a Flipper that never advertises
/// produces a clear error instead of hanging.
async fn scan_for_id(adapter: &btleplug::platform::Adapter, target: &str) -> Result<Peripheral> {
    const SCAN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(6);

    let mut events = adapter.events().await.map_err(map_btle_err)?;
    adapter
        .start_scan(btleplug::api::ScanFilter::default())
        .await
        .map_err(map_btle_err)?;

    let deadline = tokio::time::Instant::now() + SCAN_DEADLINE;
    let outcome: Result<Peripheral> = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break Err(FlipperError::Internal(
                "BLE peripheral not found after scan".into(),
            ));
        }
        match tokio::time::timeout(remaining, events.next()).await {
            Ok(Some(CentralEvent::DeviceDiscovered(pid)))
            | Ok(Some(CentralEvent::DeviceUpdated(pid))) => {
                if pid.to_string() == target {
                    if let Some(p) = find_by_id(adapter, target).await? {
                        break Ok(p);
                    }
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                break Err(FlipperError::Internal(
                    "BLE peripheral not found after scan".into(),
                ));
            }
            Err(_) => {
                break Err(FlipperError::Internal(
                    "BLE peripheral not found after scan".into(),
                ));
            }
        }
    };

    let _ = adapter.stop_scan().await;
    outcome
}

fn find_char(peripheral: &Peripheral, uuid: uuid::Uuid) -> Result<Characteristic> {
    peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == uuid)
        .ok_or_else(|| {
            FlipperError::Internal(format!("BLE characteristic {uuid} not found on device"))
        })
}

/// Connect to a Flipper over BLE and return a ready-to-use client plus a
/// cancellation handle for the spawned notification task.
///
/// The returned `cancel_tx` is how the disconnect command asks the notification
/// task to exit cleanly. Send `()` on it; the task will drop its peripheral
/// reference and set the RX buffer to `closed`.
pub struct BleConnection {
    pub client: FlipperClient,
    pub cancel: oneshot::Sender<()>,
    pub completed: oneshot::Receiver<()>,
}

pub fn connect_ble(id: String, app: AppHandle, session_id: u64) -> Result<BleConnection> {
    BLE_RT.block_on(async move { connect_ble_async(id, app, session_id).await })
}

async fn connect_ble_async(id: String, app: AppHandle, session_id: u64) -> Result<BleConnection> {
    let adapter = shared_adapter().await?;
    tracing::info!("BLE connect: resolving peripheral id={}", id);

    // Fast path: the dialog's earlier scan populated the shared adapter's
    // peripheral cache, so resolution usually completes without another scan.
    let peripheral = match find_by_id(&adapter, &id).await? {
        Some(p) => {
            tracing::info!("BLE connect: resolved from cache");
            p
        }
        None => {
            tracing::info!("BLE connect: cache miss, scanning");
            scan_for_id(&adapter, &id).await?
        }
    };

    // A peripheral carried over from a previous attempt (or freshly paired by
    // the OS) can already be in the Connected state. Calling `connect()` on
    // macOS in that case waits indefinitely for a DidConnect callback that
    // never fires. Check first, and only connect if we actually need to.
    let already_connected = peripheral.is_connected().await.unwrap_or(false);
    if !already_connected {
        tracing::info!("BLE connect: calling peripheral.connect()");
        let connect_timeout = std::time::Duration::from_secs(15);
        match tokio::time::timeout(connect_timeout, peripheral.connect()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(map_btle_err(e)),
            Err(_) => {
                let _ = peripheral.disconnect().await;
                return Err(FlipperError::Internal(format!(
                    "BLE connect timed out after {:?} — is the Flipper paired in macOS Bluetooth settings and in range?",
                    connect_timeout
                )));
            }
        }
    } else {
        tracing::info!("BLE connect: peripheral already connected, skipping connect()");
    }

    tracing::info!("BLE connect: discovering services");
    peripheral.discover_services().await.map_err(map_btle_err)?;

    let tx_char = find_char(&peripheral, TX_CHAR)?;
    let rx_char = find_char(&peripheral, RX_CHAR)?;
    let overflow_char = find_char(&peripheral, OVERFLOW_CHAR)?;
    let rpc_state_char = find_char(&peripheral, RPC_STATE_CHAR)?;
    tracing::info!("BLE connect: characteristics resolved");

    // Subscribe to all notify-capable characteristics. The Flipper firmware
    // auto-starts an RPC session on subscription to the Serial service — no
    // text `start_rpc_session\r` handshake and no raw write to RPC_STATE.
    // (RPC_STATE is notify-only; writing to it on macOS CoreBluetooth triggers
    // an ATT error that tears the whole connection down.)
    for c in [&tx_char, &overflow_char, &rpc_state_char] {
        if c.properties.contains(CharPropFlags::NOTIFY)
            || c.properties.contains(CharPropFlags::INDICATE)
        {
            peripheral.subscribe(c).await.map_err(map_btle_err)?;
        }
    }
    tracing::info!("BLE connect: subscribed to notifications");

    let rx = RxShared::new();

    // Read the FLOW_CTRL characteristic once to seed our local free-bytes
    // estimate. Without this, the first write would block until the firmware
    // spontaneously emits a notification. If the char doesn't advertise READ,
    // or the read fails, fall back to a conservative guess that's smaller than
    // any realistic buffer — later notifications correct it.
    let initial_free = if overflow_char.properties.contains(CharPropFlags::READ) {
        match peripheral.read(&overflow_char).await {
            Ok(bytes) => parse_flow_ctrl(&bytes).unwrap_or(1024),
            Err(_) => 1024,
        }
    } else {
        1024
    };
    let overflow = Arc::new(AtomicU32::new(initial_free));

    // Acquire the notifications stream before publishing a usable transport.
    // A task that discovers notifications() is unavailable after the client is
    // visible creates a short-lived ghost connection and loses early bytes.
    let stream = match peripheral.notifications().await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = peripheral.disconnect().await;
            return Err(map_btle_err(error));
        }
    };

    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let (completed_tx, completed_rx) = oneshot::channel::<()>();

    // Spawn notification dispatch task on the BLE runtime. It owns the
    // peripheral's notifications stream and lives until either the stream
    // ends (peer disconnect) or cancel_rx fires (we initiated disconnect).
    let peripheral_for_task = peripheral.clone();
    let rx_for_task = Arc::clone(&rx);
    let overflow_for_task = Arc::clone(&overflow);
    let app_for_task = app.clone();

    BLE_RT.spawn(async move {
        run_notification_task(NotificationTask {
            peripheral: peripheral_for_task,
            rx: rx_for_task,
            overflow: overflow_for_task,
            stream,
            cancel_rx,
            completed_tx,
            app: app_for_task,
            session_id,
        })
        .await;
    });

    // Give the firmware a moment to emit the initial RPC_STATE=started
    // notification. If it never does, subsequent RPC reads will time out and
    // `get_device_info` will surface the error — so this is just a courtesy
    // settle, not a hard requirement.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let transport: Box<dyn crate::flipper::transport::Transport> =
        Box::new(BleTransport::new(peripheral, rx_char, rx, overflow));
    Ok(BleConnection {
        client: FlipperClient::new(transport),
        cancel: cancel_tx,
        completed: completed_rx,
    })
}

struct NotificationTask {
    peripheral: Peripheral,
    rx: Arc<RxShared>,
    overflow: Arc<AtomicU32>,
    stream: Pin<Box<dyn Stream<Item = ValueNotification> + Send>>,
    cancel_rx: oneshot::Receiver<()>,
    completed_tx: oneshot::Sender<()>,
    app: AppHandle,
    session_id: u64,
}

async fn run_notification_task(task: NotificationTask) {
    let NotificationTask {
        peripheral,
        rx,
        overflow,
        mut stream,
        mut cancel_rx,
        completed_tx,
        app,
        session_id,
    } = task;
    let reason: String = loop {
        tokio::select! {
            _ = &mut cancel_rx => {
                break "disconnect requested".into();
            }
            next = stream.next() => {
                let Some(n) = next else {
                    break "BLE notifications stream ended".into();
                };
                if n.uuid == TX_CHAR {
                    rx.push(&n.value);
                } else if n.uuid == OVERFLOW_CHAR {
                    if let Some(v) = parse_flow_ctrl(&n.value) {
                        overflow.store(v, Ordering::SeqCst);
                    }
                } else if n.uuid == RPC_STATE_CHAR {
                    // 0 = not_started; anything else is fine. We only treat
                    // a transition to "not_started" as a disconnect signal
                    // if it happens after we've already been running.
                    let code = n.value.first().copied().unwrap_or(0);
                    tracing::debug!("BLE RPC_STATE = {}", code);
                }
            }
        }
    };

    // Best-effort teardown — ignore errors, the peer may already be gone.
    let _ = peripheral.disconnect().await;
    rx.close(reason.clone());
    // Replacement/disconnect waits only for resources owned by this task.
    // Signal that first; global cleanup is separately serialized and guarded
    // by session identity below.
    let _ = completed_tx.send(());

    // Signal the single connection owner so subsequent commands surface
    // `NotConnected` immediately. The notification task never clears the
    // published connection slot or emits the global disconnect itself; the
    // actor monitor owns both actions and therefore makes fatal publication
    // exact-once.
    let Some(state) = app.try_state::<crate::state::AppState>() else {
        return;
    };
    let lifecycle = Arc::clone(&state.connection_lifecycle);
    let _lifecycle = lifecycle.lock().await;
    if classify_ble_task(
        state.ble_session_generation.load(Ordering::Acquire),
        session_id,
    ) == BleTaskDisposition::Stale
    {
        return;
    }

    // Recheck at the mutation boundary. The lifecycle lock makes this stable;
    // the second check documents and enforces the task-identity contract.
    if classify_ble_task(
        state.ble_session_generation.load(Ordering::Acquire),
        session_id,
    ) == BleTaskDisposition::Stale
    {
        return;
    }
    {
        let mut completion = state
            .ble_session_completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if completion
            .as_ref()
            .is_some_and(|owned| owned.id == session_id)
        {
            completion.take();
        }
    }
    state
        .ble_cancel_tx
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let owner = current_ble_owner_to_signal(&state.connection_owner);
    if let Some(owner) = owner {
        let _ = owner.shutdown().await;
    }
}

fn current_ble_owner_to_signal(
    owner: &std::sync::Mutex<Option<ConnectionHandle>>,
) -> Option<ConnectionHandle> {
    owner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|owner| owner.transport_kind() == crate::flipper::transport::TransportKind::Ble)
        .cloned()
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::current_ble_owner_to_signal;
    use crate::flipper::client::FlipperClient;
    use crate::flipper::connection_actor::ConnectionHandle;
    use crate::flipper::transport::{Transport, TransportKind};

    struct IdleBleTransport;

    impl Transport for IdleBleTransport {
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
            TransportKind::Ble
        }
    }

    #[tokio::test]
    async fn notification_teardown_signals_actor_without_clearing_owner_slot() {
        let handle = ConnectionHandle::spawn(FlipperClient::new(Box::new(IdleBleTransport)))
            .expect("BLE actor should start");
        let owner = Mutex::new(Some(handle.clone()));

        let signal = current_ble_owner_to_signal(&owner).expect("BLE actor should be signalled");
        assert!(owner
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|published| published.same_connection(&handle)));

        signal
            .shutdown()
            .await
            .expect("actor shutdown should finish");
        assert!(owner
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|published| published.same_connection(&handle)));
    }
}
