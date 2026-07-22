use std::sync::atomic::Ordering;
use std::sync::Arc;

use base64::Engine;
use tauri::{AppHandle, Emitter, State};

use crate::commands::client::{connection_handle, map_actor_error};
use crate::error::{FlipperError, Result};
use crate::flipper::connection_actor::ConnectionState;
use crate::flipper::gui;
use crate::pb::main::Content;
use crate::state::AppState;

/// Start actor-owned screen streaming and forward the actor's coalescing frame
/// subscription to the frontend. The connection actor remains the only reader
/// and input writer for the complete lifetime of this mode.
#[tauri::command]
pub async fn screen_stream_start(state: State<'_, AppState>, app: AppHandle) -> Result<()> {
    let lifecycle = Arc::clone(&state.connection_lifecycle);
    let _lifecycle = lifecycle.lock().await;
    let handle = connection_handle(&state.connection_owner)?;
    if handle.state() == ConnectionState::ScreenStreaming {
        return Ok(());
    }
    if handle.state() != ConnectionState::Rpc {
        return Err(FlipperError::Session(format!(
            "Screen streaming is not allowed while connection is {}",
            handle.state()
        )));
    }

    let frames = handle.subscribe_screen_frames();
    handle
        .start_screen_stream()
        .await
        .map_err(map_actor_error)?;

    let generation = next_screen_generation(&state.screen_stream_generation);
    tauri::async_runtime::spawn(forward_screen_frames(
        frames,
        app,
        Arc::clone(&state.screen_stream_generation),
        generation,
    ));
    Ok(())
}

/// Forward one event from the frontend's complete input lifecycle unchanged.
/// The actor validates the key/type and serializes acknowledgement reads with
/// frame delivery, including LONG/REPEAT and the final RELEASE.
#[tauri::command(rename_all = "snake_case")]
pub async fn send_input_event(key: i32, input_type: i32, state: State<'_, AppState>) -> Result<()> {
    let handle = connection_handle(&state.connection_owner)?;
    handle
        .send_screen_input(key, input_type)
        .await
        .map_err(map_actor_error)
}

/// Stop the stream with the actor's acknowledged transition. No fixed sleep,
/// secondary reader, or transport handoff participates in this operation.
#[tauri::command]
pub async fn screen_stream_stop(state: State<'_, AppState>) -> Result<()> {
    let lifecycle = Arc::clone(&state.connection_lifecycle);
    let _lifecycle = lifecycle.lock().await;
    next_screen_generation(&state.screen_stream_generation);
    let handle = match connection_handle(&state.connection_owner) {
        Ok(handle) => handle,
        Err(FlipperError::NotConnected) => return Ok(()),
        Err(error) => return Err(error),
    };
    match handle.state() {
        ConnectionState::ScreenStreaming => {
            handle.stop_screen_stream().await.map_err(map_actor_error)
        }
        ConnectionState::Rpc | ConnectionState::Disconnected => Ok(()),
        current => Err(FlipperError::Session(format!(
            "Screen stop is not allowed while connection is {current}"
        ))),
    }
}

fn next_screen_generation(generation: &std::sync::atomic::AtomicU64) -> u64 {
    let next = generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    if next == 0 {
        generation.store(1, Ordering::Release);
        1
    } else {
        next
    }
}

async fn forward_screen_frames(
    mut frames: tokio::sync::watch::Receiver<Option<crate::pb::Main>>,
    app: AppHandle,
    active_generation: Arc<std::sync::atomic::AtomicU64>,
    expected_generation: u64,
) {
    while frames.changed().await.is_ok() {
        if active_generation.load(Ordering::Acquire) != expected_generation {
            break;
        }
        let Some(message) = frames.borrow_and_update().clone() else {
            continue;
        };
        let Some(Content::GuiScreenFrame(frame)) = message.content else {
            continue;
        };
        let rgba = gui::xbm_to_rgba(&frame.data, 0x000000, 0xFF8300);
        let encoded = base64::engine::general_purpose::STANDARD.encode(rgba);
        let _ = app.emit("screen-frame", encoded);
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use prost::Message;

    use crate::flipper::client::FlipperClient;
    use crate::flipper::gui;
    use crate::flipper::transport::{Transport, TransportKind};
    use crate::pb;
    use crate::pb::main::Content;

    struct RecordingTransport {
        writes: Arc<Mutex<Vec<u8>>>,
    }

    impl Transport for RecordingTransport {
        fn read_exact(&mut self, _buffer: &mut [u8]) -> io::Result<()> {
            Err(io::ErrorKind::UnexpectedEof.into())
        }

        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }

        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.writes.lock().unwrap().extend_from_slice(bytes);
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

    fn decode_writes(mut bytes: &[u8]) -> Vec<pb::Main> {
        let mut messages = Vec::new();
        while !bytes.is_empty() {
            let mut length = 0_usize;
            let mut shift = 0;
            let mut prefix_len = 0;
            loop {
                let byte = bytes[prefix_len];
                prefix_len += 1;
                length |= usize::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            let end = prefix_len + length;
            messages.push(pb::Main::decode(&bytes[prefix_len..end]).unwrap());
            bytes = &bytes[end..];
        }
        messages
    }

    #[test]
    fn screen_input_forwards_every_button_lifecycle_event_exactly_once() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut client = FlipperClient::new(Box::new(RecordingTransport {
            writes: Arc::clone(&writes),
        }));

        for key in 0..6 {
            for input_type in 0..5 {
                gui::send_input_event(&mut client, key, input_type).unwrap();
            }
        }

        let messages = decode_writes(&writes.lock().unwrap());
        assert_eq!(messages.len(), 30);
        for (index, message) in messages.into_iter().enumerate() {
            let key = (index / 5) as i32;
            let input_type = (index % 5) as i32;
            assert!(matches!(
                message.content,
                Some(Content::GuiSendInputEventRequest(request))
                    if request.key == key && request.r#type == input_type
            ));
        }
    }
}
