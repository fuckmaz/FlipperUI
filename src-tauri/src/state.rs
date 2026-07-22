use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use crate::flipper::connection_actor::ConnectionHandle;
use crate::operation::OperationRegistry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionMode {
    Rpc,
    ScreenStreaming,
    Cli,
}

impl fmt::Display for ConnectionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc => formatter.write_str("RPC"),
            Self::ScreenStreaming => formatter.write_str("screen streaming"),
            Self::Cli => formatter.write_str("CLI"),
        }
    }
}

/// Serializes CLI output identity changes with synchronous frontend emission.
/// Code that checks a generation and emits an event must keep this guard for
/// both operations, otherwise reconnect can invalidate the task between the
/// check and `AppHandle::emit`.
#[derive(Debug, Default)]
pub struct CliOutputGate {
    generation: u64,
    disconnect_emitted: bool,
}

impl CliOutputGate {
    pub(crate) fn begin_session(&mut self) -> u64 {
        self.advance();
        self.disconnect_emitted = false;
        self.generation
    }

    pub(crate) fn invalidate(&mut self) {
        self.advance();
        self.disconnect_emitted = false;
    }

    pub(crate) fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    pub(crate) fn current_generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn claim_disconnect(&mut self, generation: u64) -> bool {
        if self.generation != generation || self.disconnect_emitted {
            return false;
        }
        self.disconnect_emitted = true;
        true
    }

    fn advance(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.generation = 1;
        }
    }
}

/// Completion ownership for the BLE notification task. The cancel sender is
/// kept in the historical `ble_cancel_tx` slot because synchronous fatal-error
/// paths already use it; replacement/disconnect additionally take this
/// receiver and wait (with a bound) for peripheral/Rx teardown.
pub struct BleSessionCompletion {
    pub id: u64,
    pub completed: oneshot::Receiver<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BleTaskDisposition {
    Current,
    Stale,
}

pub(crate) fn classify_ble_task(current_session: u64, task_session: u64) -> BleTaskDisposition {
    if current_session == task_session {
        BleTaskDisposition::Current
    } else {
        BleTaskDisposition::Stale
    }
}

pub struct AppState {
    /// Live single-owner connection actor. After connection establishment the
    /// FlipperClient exists only on this actor's dedicated blocking thread.
    pub connection_owner: Arc<Mutex<Option<ConnectionHandle>>>,
    /// Serializes client/actor ownership handoff with connect, reconnect, and
    /// disconnect without holding a blocking mutex guard across `.await`.
    pub connection_lifecycle: Arc<tokio::sync::Mutex<()>>,
    /// Linearizes CLI generation/owner changes with frontend output emission.
    pub cli_output_gate: Arc<Mutex<CliOutputGate>>,
    /// Monotonic, bounded identity/cancellation registry shared by every
    /// long-running frontend operation.
    pub operations: Arc<OperationRegistry>,
    /// Identifies the active frontend screen-frame forwarder. The actor owns
    /// the wire stream; this generation only prevents stale UI tasks from
    /// forwarding frames after stop/restart.
    pub screen_stream_generation: Arc<AtomicU64>,
    /// Cancel sender for the BLE notification task (only set when the active
    /// connection is BLE). Sending on it unblocks the task so it can disconnect
    /// the peripheral cleanly. `None` for serial connections.
    pub ble_cancel_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    /// Monotonic BLE task identity. A notification task may mutate AppState or
    /// emit a disconnect only while this still matches its session id.
    pub ble_session_generation: Arc<AtomicU64>,
    /// Awaitable completion for the current BLE notification task.
    pub ble_session_completion: Arc<Mutex<Option<BleSessionCompletion>>>,
    /// Live BLE discovery scan flag — set true while a `start_ble_scan` task is
    /// pumping events; cleared by `stop_ble_scan` (or the task itself when it
    /// exits). Doubles as a "scan running" guard so a second start_ble_scan is a
    /// no-op instead of starting two competing scans on the same adapter.
    pub ble_scan_active: Arc<AtomicBool>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection_owner: Arc::new(Mutex::new(None)),
            connection_lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            cli_output_gate: Arc::new(Mutex::new(CliOutputGate::default())),
            operations: Arc::new(OperationRegistry::default()),
            screen_stream_generation: Arc::new(AtomicU64::new(0)),
            ble_cancel_tx: Arc::new(Mutex::new(None)),
            ble_session_generation: Arc::new(AtomicU64::new(0)),
            ble_session_completion: Arc::new(Mutex::new(None)),
            ble_scan_active: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_ble_task, BleTaskDisposition};

    #[test]
    fn stale_ble_task_is_rejected_after_session_replacement() {
        let old = 41;
        let replacement = 42;
        assert_eq!(
            classify_ble_task(replacement, old),
            BleTaskDisposition::Stale
        );
        assert_eq!(
            classify_ble_task(replacement, replacement),
            BleTaskDisposition::Current
        );
    }
}
