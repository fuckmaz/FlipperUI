use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, State};
use tokio::sync::broadcast;

use crate::commands::client::{connection_handle, map_actor_error};
use crate::error::{FlipperError, Result};
use crate::flipper::connection_actor::{CliOutputEvent, ConnectionState};
use crate::state::{AppState, CliOutputGate};

const CLI_DISCONNECTED: &str = "\r\n[serial error — device disconnected]\r\n";

fn cli_lag_warning(skipped: u64) -> String {
    format!("\r\n[CLI output skipped {skipped} chunks — decoder reset]\r\n")
}

fn emit_cli_disconnect_once(
    app: &AppHandle,
    gate: &Arc<Mutex<CliOutputGate>>,
    generation: u64,
) -> bool {
    let mut gate = gate.lock().unwrap();
    if !gate.claim_disconnect(generation) {
        return false;
    }
    // Generation validation, both emits, and matching-state cleanup are one
    // synchronous critical section. Reconnect cannot slip between them.
    let _ = app.emit("cli-output", CLI_DISCONNECTED);
    true
}

fn emit_cli_text_if_current(
    app: &AppHandle,
    gate: &Arc<Mutex<CliOutputGate>>,
    generation: u64,
    text: &str,
) -> bool {
    let gate = gate.lock().unwrap();
    if !gate.is_current(generation) {
        return false;
    }
    let _ = app.emit("cli-output", text);
    true
}

/// Enter actor-owned CLI mode after acknowledged legacy screen quiescence.
#[tauri::command]
pub async fn cli_start(state: State<'_, AppState>, app: AppHandle) -> Result<()> {
    let lifecycle = Arc::clone(&state.connection_lifecycle);
    let _lifecycle = lifecycle.lock().await;

    let handle = connection_handle(&state.connection_owner)?;
    if handle.state() == ConnectionState::Cli {
        return Ok(());
    }
    if handle.state() != ConnectionState::Rpc {
        return Err(FlipperError::Session(format!(
            "CLI operation is not allowed while connection is {}",
            handle.state()
        )));
    }

    let generation = state.cli_output_gate.lock().unwrap().begin_session();
    let output = handle.subscribe_cli_output();
    tauri::async_runtime::spawn(forward_cli_output(
        output,
        app.clone(),
        Arc::clone(&state.cli_output_gate),
        generation,
    ));

    if let Err(error) = handle.enter_cli().await {
        if handle.state() == ConnectionState::Rpc {
            state.cli_output_gate.lock().unwrap().invalidate();
        } else if handle.state() == ConnectionState::Disconnected {
            emit_cli_disconnect_once(&app, &state.cli_output_gate, generation);
        }
        return Err(map_actor_error(error));
    }

    Ok(())
}

/// Send a text command to the Flipper CLI.
/// The command is written as raw bytes followed by `\r`.
#[tauri::command]
pub async fn cli_send(input: String, state: State<'_, AppState>) -> Result<()> {
    let handle = connection_handle(&state.connection_owner)?;
    handle.cli_send(&input).await.map_err(map_actor_error)
}

/// Interrupt the command currently running in the Flipper CLI.
///
/// A terminal sends Ctrl+C as the single ETX byte (`0x03`), without the
/// carriage return used for submitted text commands.
#[tauri::command]
pub async fn cli_interrupt(state: State<'_, AppState>) -> Result<()> {
    let handle = connection_handle(&state.connection_owner)?;
    handle.cli_interrupt().await.map_err(map_actor_error)
}

/// Leave actor-owned CLI mode, verify RPC recovery, and reclaim the client.
/// Kept async because the acknowledged handoff can take a few seconds.
#[tauri::command]
pub async fn cli_stop(state: State<'_, AppState>, app: AppHandle) -> Result<()> {
    let lifecycle = Arc::clone(&state.connection_lifecycle);
    let _lifecycle = lifecycle.lock().await;
    let generation = state.cli_output_gate.lock().unwrap().current_generation();
    let Ok(handle) = connection_handle(&state.connection_owner) else {
        state.cli_output_gate.lock().unwrap().invalidate();
        return Ok(());
    };

    if handle.state() == ConnectionState::Rpc {
        state.cli_output_gate.lock().unwrap().invalidate();
        return Ok(());
    }

    if let Err(error) = handle.exit_cli().await {
        tracing::error!(error = %error, "CLI actor exit failed; dropping uncertain connection");
        let _ = handle.shutdown().await;
        // The forwarder may win this claim on Closed; either way the gate
        // permits one terminal marker and one global disconnect only.
        emit_cli_disconnect_once(&app, &state.cli_output_gate, generation);
        return Err(map_actor_error(error));
    }
    state.cli_output_gate.lock().unwrap().invalidate();
    Ok(())
}

async fn forward_cli_output(
    mut output: broadcast::Receiver<CliOutputEvent>,
    app: AppHandle,
    gate: Arc<Mutex<CliOutputGate>>,
    expected_generation: u64,
) {
    let mut forwarder = CliOutputForwarder::default();

    loop {
        match output.recv().await {
            Ok(event) => {
                let action = forwarder.event(event);
                if let Some(text) = action.text {
                    if !emit_cli_text_if_current(&app, &gate, expected_generation, &text) {
                        break;
                    }
                }
                if action.end {
                    debug_assert_eq!(
                        classify_cli_terminal(true, true, false),
                        CliForwardTerminal::NormalEnd
                    );
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                forwarder.reset_decoder();
                let warning = cli_lag_warning(skipped);
                if !emit_cli_text_if_current(&app, &gate, expected_generation, &warning) {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                let current = gate.lock().unwrap().is_current(expected_generation);
                if classify_cli_terminal(current, false, true)
                    == CliForwardTerminal::UnexpectedClosed
                {
                    emit_cli_disconnect_once(&app, &gate, expected_generation);
                }
                break;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliForwardTerminal {
    Stale,
    NormalEnd,
    UnexpectedClosed,
}

fn classify_cli_terminal(
    generation_current: bool,
    normal_session_end: bool,
    output_closed: bool,
) -> CliForwardTerminal {
    if !generation_current {
        CliForwardTerminal::Stale
    } else if normal_session_end || !output_closed {
        CliForwardTerminal::NormalEnd
    } else {
        CliForwardTerminal::UnexpectedClosed
    }
}

#[derive(Default)]
struct CliOutputForwarder {
    active_session: Option<u64>,
    decoder: CliOutputDecoder,
}

struct CliForwardAction {
    text: Option<String>,
    end: bool,
}

impl CliOutputForwarder {
    fn event(&mut self, event: CliOutputEvent) -> CliForwardAction {
        match event {
            CliOutputEvent::SessionStarted { session_id } => {
                self.active_session = Some(session_id);
                self.reset_decoder();
                CliForwardAction {
                    text: None,
                    end: false,
                }
            }
            CliOutputEvent::Data { session_id, bytes } => {
                if self.active_session.is_none() {
                    // Recovery after an explicitly reported Lagged boundary is
                    // safe because every data chunk carries its actor session.
                    self.active_session = Some(session_id);
                }
                let text = (self.active_session == Some(session_id))
                    .then(|| self.decoder.push(&bytes))
                    .filter(|text| !text.is_empty());
                CliForwardAction { text, end: false }
            }
            CliOutputEvent::SessionEnded { session_id } => CliForwardAction {
                text: None,
                end: self.active_session.is_none() || self.active_session == Some(session_id),
            },
        }
    }

    fn reset_decoder(&mut self) {
        self.decoder = CliOutputDecoder::default();
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
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;

    use super::{
        classify_cli_terminal, cli_lag_warning, map_actor_error, CliForwardTerminal,
        CliOutputDecoder, CliOutputForwarder,
    };
    use crate::error::FlipperError;
    use crate::flipper::connection_actor::{CliOutputEvent, ConnectionActorError, ConnectionState};
    use crate::state::CliOutputGate;

    #[test]
    fn terminal_classifier_distinguishes_stale_normal_and_unexpected_close() {
        assert_eq!(
            classify_cli_terminal(false, false, true),
            CliForwardTerminal::Stale
        );
        assert_eq!(
            classify_cli_terminal(true, true, false),
            CliForwardTerminal::NormalEnd
        );
        assert_eq!(
            classify_cli_terminal(true, false, true),
            CliForwardTerminal::UnexpectedClosed
        );
    }

    #[test]
    fn output_check_and_emit_gate_serializes_reconnect_invalidation() {
        let gate = Arc::new(Mutex::new(CliOutputGate::default()));
        let generation = gate.lock().unwrap().begin_session();
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let emitted = Arc::new(Mutex::new(Vec::new()));

        let output_gate = Arc::clone(&gate);
        let output = Arc::clone(&emitted);
        let worker = thread::spawn(move || {
            let guard = output_gate.lock().unwrap();
            assert!(guard.is_current(generation));
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            output.lock().unwrap().push(generation);
        });

        locked_rx.recv().unwrap();
        assert!(gate.try_lock().is_err());
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        gate.lock().unwrap().invalidate();
        assert_eq!(*emitted.lock().unwrap(), vec![generation]);
        assert!(!gate.lock().unwrap().is_current(generation));
    }

    #[test]
    fn disconnect_claim_is_exactly_once_per_current_generation() {
        let mut gate = CliOutputGate::default();
        let first = gate.begin_session();
        assert!(gate.claim_disconnect(first));
        assert!(!gate.claim_disconnect(first));
        let second = gate.begin_session();
        assert!(!gate.claim_disconnect(first));
        assert!(gate.claim_disconnect(second));
        assert!(!gate.claim_disconnect(second));
    }

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

    #[test]
    fn output_forwarder_resets_decoder_and_rejects_stale_session_bytes() {
        let mut forwarder = CliOutputForwarder::default();
        assert!(
            !forwarder
                .event(CliOutputEvent::SessionStarted { session_id: 7 })
                .end
        );
        assert!(forwarder
            .event(CliOutputEvent::Data {
                session_id: 7,
                bytes: b"partial \xe2\x94".to_vec(),
            })
            .text
            .is_some_and(|text| text == "partial "));

        forwarder.event(CliOutputEvent::SessionStarted { session_id: 8 });
        assert!(forwarder
            .event(CliOutputEvent::Data {
                session_id: 7,
                bytes: b"stale".to_vec(),
            })
            .text
            .is_none());
        assert!(forwarder
            .event(CliOutputEvent::Data {
                session_id: 8,
                bytes: b"fresh".to_vec(),
            })
            .text
            .is_some_and(|text| text == "fresh"));
        assert!(
            !forwarder
                .event(CliOutputEvent::SessionEnded { session_id: 7 })
                .end
        );
        assert!(
            forwarder
                .event(CliOutputEvent::SessionEnded { session_id: 8 })
                .end
        );
    }

    #[test]
    fn lag_reset_makes_loss_visible_and_prevents_split_utf8_corruption() {
        let mut forwarder = CliOutputForwarder::default();
        forwarder.event(CliOutputEvent::SessionStarted { session_id: 1 });
        let partial = forwarder.event(CliOutputEvent::Data {
            session_id: 1,
            bytes: b"\xe2\x94".to_vec(),
        });
        assert!(partial.text.is_none());

        forwarder.reset_decoder();
        assert_eq!(
            cli_lag_warning(12),
            "\r\n[CLI output skipped 12 chunks — decoder reset]\r\n"
        );
        assert!(forwarder
            .event(CliOutputEvent::Data {
                session_id: 1,
                bytes: b"\x80safe".to_vec(),
            })
            .text
            .is_some_and(|text| text == "�safe"));
    }

    #[test]
    fn actor_errors_preserve_stable_public_connection_meanings() {
        assert!(matches!(
            map_actor_error(ConnectionActorError::CliRequiresSerial),
            FlipperError::BleUnsupported
        ));
        assert!(matches!(
            map_actor_error(ConnectionActorError::Closed),
            FlipperError::NotConnected
        ));
        let locked = map_actor_error(ConnectionActorError::ModeRejected {
            current: ConnectionState::Rpc,
        });
        assert!(matches!(
            &locked,
            FlipperError::ConnectionLocked { current } if current == "RPC"
        ));
        let public = locked.command_error();
        assert_eq!(public.code, "operation_locked");
        assert!(public.retryable);
        assert_eq!(
            public
                .details
                .as_ref()
                .and_then(|details| details.get("currentMode")),
            Some(&"RPC".to_owned())
        );
    }
}
