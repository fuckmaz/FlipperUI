use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use tauri::{AppHandle, Emitter, State};

use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::gui;
use crate::state::{AppState, ConnectionMode, InputEventTx};

/// InputType constants matching the Flipper protobuf enum.
const PRESS: i32 = 0;
const RELEASE: i32 = 1;
const SHORT: i32 = 2;
const LONG: i32 = 3;

/// Bound per reader-loop iteration. We process exactly one event between
/// frame reads so a burst of inputs can't open a long write-only window —
/// on BLE that window was enough to corrupt framing and kill the stream.
/// The frontend enforces its own short wait-list so dropped events here are
/// rare; serializing strictly at the boundary keeps reads and writes paced.
const MAX_INPUTS_PER_ITER: usize = 1;

/// Start the screen stream. Spawns a background thread that reads frames
/// and emits them as `"screen-frame"` events (base64-encoded RGBA, 128x64).
#[tauri::command]
pub async fn screen_stream_start(state: State<'_, AppState>, app: AppHandle) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let screen_stream_active = Arc::clone(&state.screen_stream_active);
    let input_event_tx = Arc::clone(&state.input_event_tx);

    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        {
            let mode = mode_mutex.lock().unwrap();
            if *mode == ConnectionMode::Cli {
                return Err(FlipperError::CliModeActive);
            }
        }

        if screen_stream_active.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Start the stream and pick a reader timeout for the transport. Serial
        // uses a short window so the OS-level read releases the client mutex
        // between frames; BLE uses a longer window because waits release the
        // RxBuffer condvar (not the client mutex) and BLE bodies need ~100 ms
        // to fully arrive — a 100 ms timeout would expire mid-frame.
        {
            let mut guard = client_mutex.lock().unwrap();
            let client = guard.as_mut().ok_or(FlipperError::NotConnected)?;
            gui::start_screen_stream(client)?;
            let timeout = match client.transport.kind() {
                crate::flipper::transport::TransportKind::Ble => crate::flipper::BLE_TIMEOUT_SCREEN,
                crate::flipper::transport::TransportKind::Serial => {
                    crate::flipper::SERIAL_TIMEOUT_SCREEN
                }
            };
            client.transport.set_timeout(timeout)?;
        }

        // Create the input-event channel. `send_input_event` enqueues events
        // here; the reader thread dequeues and writes them between reads, so
        // the reader never contends on the client mutex with the writer.
        let (tx, rx) = mpsc::channel::<(i32, i32)>();
        *input_event_tx.lock().unwrap() = Some(tx);

        screen_stream_active.store(true, Ordering::Relaxed);

        let active = Arc::clone(&screen_stream_active);
        let client_mutex = Arc::clone(&client_mutex);
        let mode_mutex = Arc::clone(&mode_mutex);
        let input_tx_holder = Arc::clone(&input_event_tx);

        std::thread::spawn(move || {
            screen_reader_loop(active, client_mutex, mode_mutex, input_tx_holder, rx, app);
        });

        Ok(())
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Send a button input event to the Flipper.
/// key: 0=UP 1=DOWN 2=RIGHT 3=LEFT 4=OK 5=BACK
/// input_type: 0=PRESS 1=RELEASE 2=SHORT 3=LONG 4=REPEAT
// Tauri v2 defaults argument names to camelCase; `rename_all` keeps
// the frontend's existing snake_case calling convention.
#[tauri::command(rename_all = "snake_case")]
pub async fn send_input_event(key: i32, input_type: i32, state: State<'_, AppState>) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let screen_stream_active = Arc::clone(&state.screen_stream_active);
    let input_event_tx = Arc::clone(&state.input_event_tx);

    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        {
            let mode = mode_mutex.lock().unwrap();
            if *mode == ConnectionMode::Cli {
                return Err(FlipperError::CliModeActive);
            }
        }

        // While the screen stream reader is running, route events through its
        // channel so writes and reads are serialized on one thread. This
        // prevents keyboard auto-repeat from starving the reader and
        // overflowing the serial buffer, which was corrupting frame framing
        // and killing the stream. Clone the Sender out and drop the lock
        // before sending so we don't hold the input_event_tx mutex across
        // the channel send.
        if screen_stream_active.load(Ordering::Relaxed) {
            let tx = input_event_tx.lock().unwrap().clone();
            if let Some(tx) = tx {
                return tx
                    .send((key, input_type))
                    .map_err(|_| FlipperError::Internal("input channel closed".into()));
            }
        }

        // Fallback — stream is off, take the client mutex directly.
        let mut guard = client_mutex.lock().unwrap();
        let client = guard.as_mut().ok_or(FlipperError::NotConnected)?;
        write_input(client, key, input_type)
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Stop the screen stream.
#[tauri::command]
pub async fn screen_stream_stop(state: State<'_, AppState>) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let screen_stream_active = Arc::clone(&state.screen_stream_active);
    let input_event_tx = Arc::clone(&state.input_event_tx);

    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        if !screen_stream_active.load(Ordering::Relaxed) {
            return Ok(());
        }

        screen_stream_active.store(false, Ordering::Relaxed);

        // Drop the sender so any later `send_input_event` takes the fallback
        // path. The reader thread still holds its receiver; it will exit on
        // the next iteration when it sees `active == false`.
        *input_event_tx.lock().unwrap() = None;

        // Wait one full reader iteration so the thread releases the client
        // mutex before we send the stop command. Now safe because we're on
        // a spawn_blocking thread, not the Tauri main loop.
        std::thread::sleep(Duration::from_millis(150));

        {
            let mut guard = client_mutex.lock().unwrap();
            if let Some(ref mut client) = *guard {
                let _ = client
                    .transport
                    .set_timeout(crate::flipper::SERIAL_TIMEOUT_NORMAL);
                let _ = gui::stop_screen_stream(client);
            }
        }

        Ok(())
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// A SHORT or LONG tap expands to PRESS → (SHORT|LONG) → RELEASE — the bracketed
/// triplet qFlipper sends. Apps that only watch PRESS/RELEASE (games, custom
/// ViewPorts) need the surrounding press/release, while apps that act on the
/// SHORT/LONG type get it in between.
///
/// LONG is expanded here too. An RPC-injected `InputTypeLong` is delivered to
/// the focused app directly — the firmware does *not* re-derive long-press from
/// hold duration the way it does for the physical buttons, so there's no need
/// to space the events out over ~300 ms. Sending the triplet atomically (one
/// reader iteration, back-to-back writes) is what makes the long press reliably
/// register; a standalone LONG spread across the key lifecycle was being
/// dropped, which is why long-press appeared not to work.
fn write_input(client: &mut FlipperClient, key: i32, input_type: i32) -> Result<()> {
    if input_type == SHORT || input_type == LONG {
        gui::send_input_event(client, key, PRESS)?;
        gui::send_input_event(client, key, input_type)?;
        gui::send_input_event(client, key, RELEASE)?;
        Ok(())
    } else {
        gui::send_input_event(client, key, input_type)
    }
}

/// Background loop that reads screen frames and emits them, and also handles
/// queued input events. Running reads and writes on one thread prevents the
/// cross-thread mutex starvation that used to corrupt frame framing.
fn screen_reader_loop(
    active: Arc<AtomicBool>,
    client_mutex: Arc<Mutex<Option<FlipperClient>>>,
    mode_mutex: Arc<Mutex<ConnectionMode>>,
    input_tx_holder: InputEventTx,
    rx: mpsc::Receiver<(i32, i32)>,
    app: AppHandle,
) {
    use crate::flipper::framing::read_message;
    use crate::pb::main::Content;

    let mut fatal: Option<String> = None;

    'outer: loop {
        if !active.load(Ordering::Relaxed) {
            break;
        }

        // Step 1: drain queued input events (bounded) and write them. Holding
        // the client mutex only for this batch keeps the window short.
        let mut pending: Vec<(i32, i32)> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            pending.push(ev);
            if pending.len() >= MAX_INPUTS_PER_ITER {
                break;
            }
        }
        if !pending.is_empty() {
            let mut guard = client_mutex.lock().unwrap();
            let Some(client) = guard.as_mut() else {
                fatal = Some("client was torn down by another command".into());
                break 'outer;
            };
            for (key, input_type) in pending {
                if let Err(e) = write_input(client, key, input_type) {
                    fatal = Some(format!("input event write failed: {e}"));
                    break 'outer;
                }
            }
        }

        // Step 2: read one message. TimedOut is normal (no frame yet); any
        // other error is fatal and means framing is likely corrupt.
        let frame = {
            let mut guard = client_mutex.lock().unwrap();
            let Some(client) = guard.as_mut() else {
                // Another path (typically `with_client` after an RPC error)
                // tore down the client while we were between iterations. Log
                // it so the disconnect isn't a silent mystery.
                fatal = Some("client was torn down by another command".into());
                break 'outer;
            };
            match read_message(&mut *client.transport) {
                Ok(msg) => {
                    let data = if let Some(Content::GuiScreenFrame(frame)) = msg.content {
                        Some((frame.data, frame.orientation))
                    } else {
                        None
                    };
                    Ok(Some(data))
                }
                Err(FlipperError::Io(ref e)) if e.kind() == std::io::ErrorKind::TimedOut => {
                    Ok(None)
                }
                Err(e) => Err(e),
            }
        };

        match frame {
            Ok(Some(Some((data, _orientation)))) => {
                // Dark segments on an amber backlight: set bit → black,
                // unset bit → orange.
                let rgba = gui::xbm_to_rgba(&data, 0x000000, 0xFF8300);
                let b64 = base64::engine::general_purpose::STANDARD.encode(&rgba);
                let _ = app.emit("screen-frame", &b64);
            }
            Ok(Some(None)) => {
                // Empty ack (from start-stream or an input event) — not end of
                // stream; the reader exits only via the `active` flag.
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                fatal = Some(format!("screen stream read failed: {e}"));
                break;
            }
        }
    }

    // Reader is exiting — regardless of why, clear state it owns.
    active.store(false, Ordering::Relaxed);
    *input_tx_holder.lock().unwrap() = None;

    if let Some(reason) = fatal {
        // Framing is probably corrupt, so the serial session is unrecoverable.
        // Tear down the client and tell the frontend, so the UI reflects the
        // real state instead of showing "connected" over a dead link.
        tracing::warn!("screen reader exiting: {reason}");
        {
            let mut guard = client_mutex.lock().unwrap();
            *guard = None;
        }
        {
            let mut mode = mode_mutex.lock().unwrap();
            *mode = ConnectionMode::Rpc;
        }
        let _ = app.emit("flipper-disconnected", &reason);
    }
}
