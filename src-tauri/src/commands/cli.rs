use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, State};

use crate::error::{FlipperError, Result};
use crate::flipper::transport::TransportKind;
use crate::flipper::{cli, client::FlipperClient, gui};
use crate::state::{AppState, ConnectionMode};

const SCREEN_READER_QUIESCE: Duration = Duration::from_millis(150);
const CLI_INTERRUPT: &[u8] = &[0x03];

/// Enter CLI mode: stop the RPC session and start a reader thread that
/// emits `"cli-output"` events for every chunk of text the Flipper sends.
#[tauri::command]
pub async fn cli_start(state: State<'_, AppState>, app: AppHandle) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let screen_stream_active = Arc::clone(&state.screen_stream_active);
    let input_event_tx = Arc::clone(&state.input_event_tx);
    let cli_reader_active = Arc::clone(&state.cli_reader_active);

    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        // Claim the transition before touching the stream. Screen cleanup and
        // CLI mount happen concurrently in React; marking CLI here prevents a
        // late screen_stream_stop/start or telemetry RPC from writing protobuf
        // bytes while the serial port is changing protocols.
        {
            let mut mode = mode_mutex.lock().unwrap();
            if *mode == ConnectionMode::Cli {
                return Ok(());
            }
            *mode = ConnectionMode::Cli;
        }

        let transition_result = (|| -> Result<()> {
            // Always quiesce the reader and send the stop-stream RPC ourselves.
            // The component cleanup may already have cleared the active flag,
            // but it now observes ConnectionMode::Cli and deliberately leaves
            // protocol cleanup to this transition owner.
            let was_streaming = screen_stream_active.swap(false, Ordering::SeqCst);
            *input_event_tx.lock().unwrap() = None;
            if was_streaming {
                tracing::info!("CLI: stopping active screen stream before entering CLI");
            }
            std::thread::sleep(SCREEN_READER_QUIESCE);

            let mut guard = client_mutex.lock().unwrap();
            let client = guard.as_mut().ok_or(FlipperError::NotConnected)?;
            if client.kind() == TransportKind::Ble {
                return Err(FlipperError::BleUnsupported);
            }
            client
                .transport
                .set_timeout(crate::flipper::SERIAL_TIMEOUT_NORMAL)?;
            // Safe even when no stream was active: firmware answers with a
            // non-streaming terminal response, which this helper drains.
            gui::stop_screen_stream(client)?;
            cli::enter_cli_mode(client)
        })();

        if let Err(error) = transition_result {
            let mut mode = mode_mutex.lock().unwrap();
            *mode = ConnectionMode::Rpc;
            return Err(error);
        }

        // Activate the reader thread
        cli_reader_active.store(true, Ordering::Relaxed);

        let active = Arc::clone(&cli_reader_active);
        let client_mutex = Arc::clone(&client_mutex);
        std::thread::spawn(move || {
            cli_reader_loop(active, client_mutex, app);
        });

        Ok(())
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Send a text command to the Flipper CLI.
/// The command is written as raw bytes followed by `\r`.
#[tauri::command]
pub async fn cli_send(input: String, state: State<'_, AppState>) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        {
            let mode = mode_mutex.lock().unwrap();
            if *mode != ConnectionMode::Cli {
                return Err(FlipperError::Session("Not in CLI mode".into()));
            }
        }

        let mut guard = client_mutex.lock().unwrap();
        let client = guard.as_mut().ok_or(FlipperError::NotConnected)?;
        let cmd = format!("{}\r", input);
        client.transport.write_all(cmd.as_bytes())?;
        client.transport.flush()?;
        Ok(())
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Interrupt the command currently running in the Flipper CLI.
///
/// A terminal sends Ctrl+C as the single ETX byte (`0x03`), without the
/// carriage return used for submitted text commands.
#[tauri::command]
pub async fn cli_interrupt(state: State<'_, AppState>) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);

    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        {
            let mode = mode_mutex.lock().unwrap();
            if *mode != ConnectionMode::Cli {
                return Err(FlipperError::Session("Not in CLI mode".into()));
            }
        }

        let mut guard = client_mutex.lock().unwrap();
        let client = guard.as_mut().ok_or(FlipperError::NotConnected)?;
        client.transport.write_all(CLI_INTERRUPT)?;
        client.transport.flush()?;
        Ok(())
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Leave CLI mode: stop the reader thread and re-enter RPC mode.
/// Kept async because exit_cli_mode involves serial I/O that can take a few seconds.
#[tauri::command]
pub async fn cli_stop(state: State<'_, AppState>) -> Result<()> {
    let client_mutex = Arc::clone(&state.client);
    let mode_mutex = Arc::clone(&state.mode);
    let cli_reader_active = Arc::clone(&state.cli_reader_active);

    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        // Signal the reader thread to stop
        cli_reader_active.store(false, Ordering::SeqCst);

        // Check if we're actually in CLI mode
        {
            let mode = mode_mutex.lock().unwrap();
            if *mode != ConnectionMode::Cli {
                return Ok(());
            }
        }

        // Re-enter RPC mode
        let exit_result = {
            let mut guard = client_mutex.lock().unwrap();
            let client = match guard.as_mut() {
                Some(c) => c,
                None => {
                    let mut mode = mode_mutex.lock().unwrap();
                    *mode = ConnectionMode::Rpc;
                    return Ok(());
                }
            };

            match cli::exit_cli_mode(client) {
                Ok(()) => Ok(()),
                Err(e) => {
                    tracing::error!("CLI: exit_cli_mode failed: {}, tearing down connection", e);
                    *guard = None;
                    Err(e)
                }
            }
        };

        // Always reset mode to Rpc
        {
            let mut mode = mode_mutex.lock().unwrap();
            *mode = ConnectionMode::Rpc;
        }

        exit_result
    })
    .await
    .map_err(|e| FlipperError::Internal(e.to_string()))?
}

/// Background loop that reads from the serial port and emits text as events.
fn cli_reader_loop(
    active: Arc<AtomicBool>,
    client_mutex: Arc<Mutex<Option<FlipperClient>>>,
    app: AppHandle,
) {
    let mut buf = [0u8; 1024];
    let mut decoder = CliOutputDecoder::default();

    loop {
        if !active.load(Ordering::Relaxed) {
            break;
        }

        let result = {
            let mut guard = client_mutex.lock().unwrap();
            if let Some(ref mut client) = *guard {
                match client.transport.read(&mut buf) {
                    Ok(n) if n > 0 => Some(Ok(n)),
                    Ok(_) => None,
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => None,
                    Err(e) => Some(Err(e)),
                }
            } else {
                break;
            }
        };

        match result {
            Some(Ok(n)) => {
                let text = decoder.push(&buf[..n]);
                if !text.is_empty() {
                    let _ = app.emit("cli-output", &text);
                }
            }
            Some(Err(_)) => {
                let _ = app.emit("cli-output", "\r\n[serial error — device disconnected]\r\n");
                active.store(false, Ordering::Relaxed);
                break;
            }
            None => {}
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Incrementally decodes the byte stream and removes terminal control
/// sequences that WebKit cannot render. Serial reads may end in the middle of
/// either a UTF-8 character or an ANSI escape, so both states must survive
/// between calls.
#[derive(Default)]
struct CliOutputDecoder {
    utf8_pending: Vec<u8>,
    escape_state: EscapeState,
    pending_carriage_return: bool,
}

#[derive(Clone, Copy, Default)]
enum EscapeState {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    ControlString,
    ControlStringEscape,
}

impl CliOutputDecoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        let decoded = self.decode_utf8(bytes);
        self.sanitize_terminal_text(&decoded)
    }

    fn decode_utf8(&mut self, bytes: &[u8]) -> String {
        self.utf8_pending.extend_from_slice(bytes);
        let mut output = String::new();

        loop {
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(valid) => {
                    output.push_str(valid);
                    self.utf8_pending.clear();
                    break;
                }
                Err(error) => {
                    let valid_len = error.valid_up_to();
                    let invalid_len = error.error_len();

                    if valid_len > 0 {
                        // `valid_up_to` guarantees this prefix is valid UTF-8.
                        output.push_str(
                            std::str::from_utf8(&self.utf8_pending[..valid_len])
                                .expect("validated UTF-8 prefix"),
                        );
                        self.utf8_pending.drain(..valid_len);
                    }

                    match invalid_len {
                        Some(len) => {
                            output.push(char::REPLACEMENT_CHARACTER);
                            self.utf8_pending.drain(..len);
                        }
                        // The remaining bytes are a valid, incomplete UTF-8
                        // sequence. Keep them for the next serial read.
                        None => break,
                    }
                }
            }
        }

        output
    }

    fn sanitize_terminal_text(&mut self, text: &str) -> String {
        let mut output = String::with_capacity(text.len());

        for character in text.chars() {
            match self.escape_state {
                EscapeState::Ground => match character {
                    '\u{1b}' => self.escape_state = EscapeState::Escape,
                    '\u{9b}' => self.escape_state = EscapeState::Csi,
                    '\r' => self.pending_carriage_return = true,
                    '\n' => {
                        self.pending_carriage_return = false;
                        output.push('\n');
                    }
                    '\t' => {
                        self.flush_carriage_return(&mut output);
                        output.push('\t');
                    }
                    // Bells, backspaces, deletes, and other control characters
                    // have no faithful representation in a text-only view and
                    // otherwise appear as missing-glyph boxes.
                    control if control.is_control() => {}
                    printable => {
                        self.flush_carriage_return(&mut output);
                        output.push(printable);
                    }
                },
                EscapeState::Escape => {
                    self.escape_state = match character {
                        '[' => EscapeState::Csi,
                        ']' => EscapeState::Osc,
                        'P' | 'X' | '^' | '_' => EscapeState::ControlString,
                        // Other ESC sequences are either two-byte controls or
                        // have intermediate bytes. Dropping the entire sequence
                        // is preferable to displaying its payload as text.
                        '\u{20}'..='\u{2f}' => EscapeState::Escape,
                        _ => EscapeState::Ground,
                    };
                }
                EscapeState::Csi => {
                    if ('\u{40}'..='\u{7e}').contains(&character) {
                        self.escape_state = EscapeState::Ground;
                    }
                }
                EscapeState::Osc => match character {
                    '\u{7}' => self.escape_state = EscapeState::Ground,
                    '\u{1b}' => self.escape_state = EscapeState::OscEscape,
                    _ => {}
                },
                EscapeState::OscEscape => {
                    self.escape_state = if character == '\\' {
                        EscapeState::Ground
                    } else if character == '\u{1b}' {
                        EscapeState::OscEscape
                    } else {
                        EscapeState::Osc
                    };
                }
                EscapeState::ControlString => {
                    if character == '\u{1b}' {
                        self.escape_state = EscapeState::ControlStringEscape;
                    }
                }
                EscapeState::ControlStringEscape => {
                    self.escape_state = if character == '\\' {
                        EscapeState::Ground
                    } else if character == '\u{1b}' {
                        EscapeState::ControlStringEscape
                    } else {
                        EscapeState::ControlString
                    };
                }
            }
        }

        output
    }

    fn flush_carriage_return(&mut self, output: &mut String) {
        if self.pending_carriage_return {
            // A bare carriage return means "return to column zero" in a real
            // terminal. Rendering it as a line boundary keeps progress/status
            // updates readable in this text-only terminal view.
            output.push('\n');
            self.pending_carriage_return = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CliOutputDecoder;

    #[test]
    fn decoder_preserves_utf8_split_across_serial_reads() {
        let mut decoder = CliOutputDecoder::default();

        assert_eq!(decoder.push(b"box: \xe2\x94"), "box: ");
        assert_eq!(decoder.push(b"\x80 done\r\n"), "\u{2500} done\n");
    }

    #[test]
    fn decoder_strips_ansi_sequences_split_across_serial_reads() {
        let mut decoder = CliOutputDecoder::default();

        assert_eq!(decoder.push(b"plain \x1b[3"), "plain ");
        assert_eq!(decoder.push(b"1mred\x1b[0m text"), "red text");
        assert_eq!(decoder.push(b"\x1b]0;title\x1b"), "");
        assert_eq!(decoder.push(b"\\visible"), "visible");
    }

    #[test]
    fn decoder_removes_non_rendering_controls_and_normalizes_carriage_returns() {
        let mut decoder = CliOutputDecoder::default();

        assert_eq!(decoder.push(b"one\r"), "one");
        assert_eq!(decoder.push(b"two\x08\x07\r\nthree"), "\ntwo\nthree");
    }
}
