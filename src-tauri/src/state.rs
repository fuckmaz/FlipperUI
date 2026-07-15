use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{mpsc, Arc, Mutex};

use tokio::sync::oneshot;

use crate::flipper::client::FlipperClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionMode {
    Rpc,
    Cli,
}

/// Shared slot holding the current screen-stream input-event sender (if the
/// reader thread is running). Cleared by the reader when it exits and by
/// connect/disconnect for safety.
pub type InputEventTx = Arc<Mutex<Option<mpsc::Sender<(i32, i32)>>>>;

/// Firmware flashing has its own operation identity so raw/concurrent flash
/// invokes cannot overlap or have their cancellation stolen by an unrelated
/// file transfer advancing the global transfer generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirmwareOperationPhase {
    Reversible,
    Cancelled,
    Committing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirmwareCancelOutcome {
    Cancelled,
    TooLate,
    NoActiveOperation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirmwareCommitOutcome {
    Started,
    Cancelled,
    NotActive,
}

#[derive(Debug, Default)]
pub struct FirmwareOperationState {
    next_id: u64,
    active_id: Option<u64>,
    phase: Option<FirmwareOperationPhase>,
}

impl FirmwareOperationState {
    pub(crate) fn begin(&mut self) -> Option<u64> {
        if self.active_id.is_some() {
            return None;
        }
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        self.active_id = Some(self.next_id);
        self.phase = Some(FirmwareOperationPhase::Reversible);
        Some(self.next_id)
    }

    /// Linearization point for cancellation versus the irreversible updater
    /// request. Holding the operation mutex means either this transition wins
    /// and `begin_commit` must refuse to stage, or the commit transition wins
    /// and cancellation reports that it is too late.
    pub(crate) fn cancel_active(&mut self) -> FirmwareCancelOutcome {
        match (self.active_id, self.phase) {
            (None, _) => FirmwareCancelOutcome::NoActiveOperation,
            (Some(_), Some(FirmwareOperationPhase::Reversible)) => {
                self.phase = Some(FirmwareOperationPhase::Cancelled);
                FirmwareCancelOutcome::Cancelled
            }
            (Some(_), Some(FirmwareOperationPhase::Cancelled)) => FirmwareCancelOutcome::Cancelled,
            (Some(_), Some(FirmwareOperationPhase::Committing)) => FirmwareCancelOutcome::TooLate,
            (Some(_), None) => FirmwareCancelOutcome::NoActiveOperation,
        }
    }

    pub(crate) fn is_cancelled(&self, operation_id: u64) -> bool {
        self.active_id == Some(operation_id)
            && self.phase == Some(FirmwareOperationPhase::Cancelled)
    }

    pub(crate) fn begin_commit(&mut self, operation_id: u64) -> FirmwareCommitOutcome {
        if self.active_id != Some(operation_id) {
            return FirmwareCommitOutcome::NotActive;
        }
        match self.phase {
            Some(FirmwareOperationPhase::Reversible) => {
                self.phase = Some(FirmwareOperationPhase::Committing);
                FirmwareCommitOutcome::Started
            }
            Some(FirmwareOperationPhase::Cancelled) => FirmwareCommitOutcome::Cancelled,
            Some(FirmwareOperationPhase::Committing) | None => FirmwareCommitOutcome::NotActive,
        }
    }

    pub(crate) fn finish(&mut self, operation_id: u64) {
        if self.active_id == Some(operation_id) {
            self.active_id = None;
            self.phase = None;
        }
    }
}

pub struct AppState {
    /// The connected Flipper client. Wrapped in Arc so background threads
    /// can share access without holding a reference to the full AppState.
    pub client: Arc<Mutex<Option<FlipperClient>>>,
    pub mode: Arc<Mutex<ConnectionMode>>,
    /// Signals the CLI reader thread to stop.
    pub cli_reader_active: Arc<AtomicBool>,
    /// Monotonic id for the active file transfer. Cancel requests target the
    /// generation that was active when the user clicked cancel, so a stale
    /// cancel cannot abort the next transfer.
    pub transfer_generation: Arc<AtomicU64>,
    pub transfer_cancelled_generation: Arc<AtomicU64>,
    /// Single-flight/cancellation state dedicated to firmware updates.
    pub firmware_operation: Arc<Mutex<FirmwareOperationState>>,
    /// Signals the screen stream reader thread to stop.
    pub screen_stream_active: Arc<AtomicBool>,
    /// Signals an in-progress SubGhz library scan to abort.
    pub subghz_scan_cancelled: Arc<AtomicBool>,
    /// Signals an in-progress Infrared library scan to abort.
    pub ir_scan_cancelled: Arc<AtomicBool>,
    /// Signals an in-progress App library scan to abort.
    pub apps_scan_cancelled: Arc<AtomicBool>,
    /// Signals an in-progress NFC library scan to abort.
    pub nfc_scan_cancelled: Arc<AtomicBool>,
    /// Signals an in-progress 125 kHz RFID library scan to abort.
    pub rfid_scan_cancelled: Arc<AtomicBool>,
    /// Signals an in-progress BadUSB library scan to abort.
    pub badusb_scan_cancelled: Arc<AtomicBool>,
    /// Channel for sending input events through the screen reader thread,
    /// avoiding mutex contention between send_input_event and the reader loop.
    /// `Arc` so both the Tauri command handler and the reader thread can hold
    /// a reference — the reader clears this slot when it exits.
    pub input_event_tx: InputEventTx,
    /// Cancel sender for the BLE notification task (only set when the active
    /// connection is BLE). Sending on it unblocks the task so it can disconnect
    /// the peripheral cleanly. `None` for serial connections.
    pub ble_cancel_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
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
            client: Arc::new(Mutex::new(None)),
            mode: Arc::new(Mutex::new(ConnectionMode::Rpc)),
            cli_reader_active: Arc::new(AtomicBool::new(false)),
            transfer_generation: Arc::new(AtomicU64::new(0)),
            transfer_cancelled_generation: Arc::new(AtomicU64::new(0)),
            firmware_operation: Arc::new(Mutex::new(FirmwareOperationState::default())),
            screen_stream_active: Arc::new(AtomicBool::new(false)),
            subghz_scan_cancelled: Arc::new(AtomicBool::new(false)),
            ir_scan_cancelled: Arc::new(AtomicBool::new(false)),
            apps_scan_cancelled: Arc::new(AtomicBool::new(false)),
            nfc_scan_cancelled: Arc::new(AtomicBool::new(false)),
            rfid_scan_cancelled: Arc::new(AtomicBool::new(false)),
            badusb_scan_cancelled: Arc::new(AtomicBool::new(false)),
            input_event_tx: Arc::new(Mutex::new(None)),
            ble_cancel_tx: Arc::new(Mutex::new(None)),
            ble_scan_active: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use super::{FirmwareCancelOutcome, FirmwareCommitOutcome, FirmwareOperationState};

    #[test]
    fn firmware_operation_is_single_flight_and_cancellation_is_operation_scoped() {
        let mut state = FirmwareOperationState::default();
        let first = state.begin().unwrap();
        assert!(state.begin().is_none());
        assert!(!state.is_cancelled(first));
        assert_eq!(state.cancel_active(), FirmwareCancelOutcome::Cancelled);
        assert!(state.is_cancelled(first));

        state.finish(first);
        let second = state.begin().unwrap();
        assert_ne!(first, second);
        assert!(!state.is_cancelled(second));
        assert!(!state.is_cancelled(first));
    }

    #[test]
    fn stale_finish_does_not_clear_a_new_firmware_operation() {
        let mut state = FirmwareOperationState::default();
        let first = state.begin().unwrap();
        state.finish(first);
        let second = state.begin().unwrap();
        state.finish(first);
        assert!(state.begin().is_none());
        assert_eq!(state.cancel_active(), FirmwareCancelOutcome::Cancelled);
        assert!(state.is_cancelled(second));
    }

    #[test]
    fn commit_winner_makes_later_cancellation_too_late() {
        let mut state = FirmwareOperationState::default();
        let operation_id = state.begin().unwrap();
        assert_eq!(
            state.begin_commit(operation_id),
            FirmwareCommitOutcome::Started
        );
        assert_eq!(state.cancel_active(), FirmwareCancelOutcome::TooLate);
        assert!(!state.is_cancelled(operation_id));
    }

    #[test]
    fn cancellation_winner_prevents_commit() {
        let mut state = FirmwareOperationState::default();
        let operation_id = state.begin().unwrap();
        assert_eq!(state.cancel_active(), FirmwareCancelOutcome::Cancelled);
        assert_eq!(
            state.begin_commit(operation_id),
            FirmwareCommitOutcome::Cancelled
        );
        assert!(state.is_cancelled(operation_id));
    }

    #[test]
    fn concurrent_cancel_and_commit_have_exactly_one_winner() {
        for _ in 0..100 {
            let state = Arc::new(Mutex::new(FirmwareOperationState::default()));
            let operation_id = state.lock().unwrap().begin().unwrap();
            let barrier = Arc::new(Barrier::new(3));

            let commit_state = Arc::clone(&state);
            let commit_barrier = Arc::clone(&barrier);
            let commit = thread::spawn(move || {
                commit_barrier.wait();
                commit_state.lock().unwrap().begin_commit(operation_id)
            });

            let cancel_state = Arc::clone(&state);
            let cancel_barrier = Arc::clone(&barrier);
            let cancel = thread::spawn(move || {
                cancel_barrier.wait();
                cancel_state.lock().unwrap().cancel_active()
            });

            barrier.wait();
            let commit_outcome = commit.join().unwrap();
            let cancel_outcome = cancel.join().unwrap();
            assert!(matches!(
                (commit_outcome, cancel_outcome),
                (
                    FirmwareCommitOutcome::Started,
                    FirmwareCancelOutcome::TooLate
                ) | (
                    FirmwareCommitOutcome::Cancelled,
                    FirmwareCancelOutcome::Cancelled
                )
            ));
        }
    }
}
