use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::error::{FlipperError, Result};

/// Stable names for cancellable, long-running work exposed over IPC.
///
/// Names are deliberately more specific than broad categories: a cancellation
/// for an old Sub-GHz scan must never cancel a later NFC scan (or vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationName {
    Transfer,
    SubghzScan,
    InfraredScan,
    NfcScan,
    RfidScan,
    BadusbScan,
    AppsScan,
    LibraryPrewalk,
    Firmware,
    Export,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationPhase {
    Reversible,
    Cancelled,
    Committing,
}

struct OperationRecord {
    name: OperationName,
    phase: OperationPhase,
    cancelled: Arc<AtomicBool>,
}

struct RegistryInner {
    next_id: u64,
    active: HashMap<u64, OperationRecord>,
}

/// Explicit upper bound for queued/running frontend jobs. The connection actor
/// has its own bounded queue; this earlier boundary gives callers a stable
/// device-busy response before unbounded operation metadata can accumulate.
pub const MAX_ACTIVE_OPERATIONS: usize = 32;

pub struct OperationRegistry {
    inner: Mutex<RegistryInner>,
    capacity: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    Requested,
    AlreadyRequested,
    TooLate,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    Started,
    Cancelled,
    Stale,
}

/// RAII ownership of one registry entry. Dropping a completed, failed, or
/// rejected command retires exactly that operation ID.
pub struct OperationLease {
    registry: Arc<OperationRegistry>,
    operation_id: u64,
    name: OperationName,
    cancelled: Arc<AtomicBool>,
}

impl OperationLease {
    pub fn id(&self) -> u64 {
        self.operation_id
    }

    pub fn name(&self) -> OperationName {
        self.name
    }

    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn begin_commit(&self) -> CommitOutcome {
        self.registry.begin_commit(self.name, self.operation_id)
    }
}

impl Drop for OperationLease {
    fn drop(&mut self) {
        self.registry.finish(self.name, self.operation_id);
    }
}

impl Default for OperationRegistry {
    fn default() -> Self {
        Self::with_capacity(MAX_ACTIVE_OPERATIONS)
    }
}

impl OperationRegistry {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                next_id: 0,
                active: HashMap::new(),
            }),
            capacity,
        }
    }

    /// Begin named work. Each public operation name is single-flight so the
    /// frontend cannot silently queue duplicate scans/transfers behind itself.
    pub fn begin(self: &Arc<Self>, name: OperationName) -> Result<OperationLease> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| FlipperError::Internal("operation registry is poisoned".into()))?;
        if inner.active.len() >= self.capacity
            || inner.active.values().any(|record| record.name == name)
        {
            return Err(FlipperError::ConnectionBusy);
        }

        inner.next_id = inner.next_id.wrapping_add(1);
        if inner.next_id == 0 {
            inner.next_id = 1;
        }
        let operation_id = inner.next_id;
        let cancelled = Arc::new(AtomicBool::new(false));
        inner.active.insert(
            operation_id,
            OperationRecord {
                name,
                phase: OperationPhase::Reversible,
                cancelled: Arc::clone(&cancelled),
            },
        );
        drop(inner);

        Ok(OperationLease {
            registry: Arc::clone(self),
            operation_id,
            name,
            cancelled,
        })
    }

    pub fn cancel(&self, name: OperationName, operation_id: u64) -> CancelOutcome {
        let Ok(mut inner) = self.inner.lock() else {
            return CancelOutcome::Unknown;
        };
        let last_issued_id = inner.next_id;
        match inner.active.get_mut(&operation_id) {
            Some(record) if record.name != name => CancelOutcome::Stale,
            Some(record) => match record.phase {
                OperationPhase::Reversible => {
                    record.phase = OperationPhase::Cancelled;
                    record.cancelled.store(true, Ordering::Release);
                    CancelOutcome::Requested
                }
                OperationPhase::Cancelled => CancelOutcome::AlreadyRequested,
                OperationPhase::Committing => CancelOutcome::TooLate,
            },
            None if operation_id > 0 && operation_id <= last_issued_id => CancelOutcome::Stale,
            None => CancelOutcome::Unknown,
        }
    }

    pub fn begin_commit(&self, name: OperationName, operation_id: u64) -> CommitOutcome {
        let Ok(mut inner) = self.inner.lock() else {
            return CommitOutcome::Stale;
        };
        match inner.active.get_mut(&operation_id) {
            Some(record) if record.name == name => match record.phase {
                OperationPhase::Reversible => {
                    record.phase = OperationPhase::Committing;
                    CommitOutcome::Started
                }
                OperationPhase::Cancelled => CommitOutcome::Cancelled,
                OperationPhase::Committing => CommitOutcome::Stale,
            },
            _ => CommitOutcome::Stale,
        }
    }

    fn finish(&self, name: OperationName, operation_id: u64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner
            .active
            .get(&operation_id)
            .is_some_and(|record| record.name == name)
        {
            inner.active.remove(&operation_id);
        }
    }
}

/// Convert cancellation outcomes shared by transfer/scan commands into the
/// stable command-error contract. Firmware exposes the richer `TooLate`
/// status directly because its commit barrier is visible to the user.
pub fn require_cancelled(outcome: CancelOutcome) -> Result<()> {
    match outcome {
        CancelOutcome::Requested | CancelOutcome::AlreadyRequested => Ok(()),
        CancelOutcome::TooLate => Err(FlipperError::InvalidInput(
            "operation has crossed its commit barrier".into(),
        )),
        CancelOutcome::Stale => Err(FlipperError::InvalidInput(
            "stale operation ID; cancellation was rejected".into(),
        )),
        CancelOutcome::Unknown => Err(FlipperError::InvalidInput(
            "unknown operation ID; cancellation was rejected".into(),
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub completed: u64,
    pub total: u64,
    pub percent: u32,
}

/// Normalizes transport callbacks into a monotonic progress stream. Empty
/// operations start at 0 and finish at 100, avoiding divide-by-zero while
/// preserving visible start/completion boundaries.
#[derive(Debug, Default)]
pub struct ProgressTracker {
    completed: u64,
    total: u64,
    percent: u32,
}

impl ProgressTracker {
    pub fn update(&mut self, completed: u64, total: u64) -> ProgressSnapshot {
        self.snapshot(completed, total, false)
    }

    pub fn finish(&mut self, completed: u64, total: u64) -> ProgressSnapshot {
        self.snapshot(completed, total, true)
    }

    fn snapshot(&mut self, completed: u64, total: u64, finished: bool) -> ProgressSnapshot {
        self.total = self.total.max(total);
        let completed = self.completed.max(completed);
        self.completed = if self.total == 0 {
            completed
        } else {
            completed.min(self.total)
        };
        let calculated = if finished {
            100
        } else if self.total == 0 {
            0
        } else {
            self.completed
                .saturating_mul(100)
                .checked_div(self.total)
                .unwrap_or(0) as u32
        };
        self.percent = self.percent.max(calculated.min(100));
        ProgressSnapshot {
            completed: self.completed,
            total: self.total,
            percent: self.percent,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    #[test]
    fn stale_cancellation_cannot_cancel_a_replacement() {
        let registry = Arc::new(OperationRegistry::with_capacity(2));
        let first = registry.begin(OperationName::Transfer).unwrap();
        let first_id = first.id();
        drop(first);
        let second = registry.begin(OperationName::Transfer).unwrap();

        assert_eq!(
            registry.cancel(OperationName::Transfer, first_id),
            CancelOutcome::Stale
        );
        assert!(!second.is_cancelled());
    }

    #[test]
    fn registry_rejects_duplicate_names_and_capacity_overflow_as_busy() {
        let registry = Arc::new(OperationRegistry::with_capacity(2));
        let _transfer = registry.begin(OperationName::Transfer).unwrap();
        assert!(matches!(
            registry.begin(OperationName::Transfer),
            Err(FlipperError::ConnectionBusy)
        ));
        let _scan = registry.begin(OperationName::NfcScan).unwrap();
        assert!(matches!(
            registry.begin(OperationName::Firmware),
            Err(FlipperError::ConnectionBusy)
        ));
    }

    #[test]
    fn progress_is_monotonic_and_zero_byte_work_has_start_and_finish() {
        let mut tracker = ProgressTracker::default();
        let snapshots = [
            tracker.update(0, 100),
            tracker.update(60, 100),
            tracker.update(40, 100),
            tracker.finish(100, 100),
        ];
        assert_eq!(snapshots.map(|snapshot| snapshot.percent), [0, 60, 60, 100]);

        let mut empty = ProgressTracker::default();
        assert_eq!(empty.update(0, 0).percent, 0);
        assert_eq!(empty.finish(0, 0).percent, 100);
    }

    #[test]
    fn cancel_and_commit_have_one_linearized_winner() {
        for _ in 0..100 {
            let registry = Arc::new(OperationRegistry::with_capacity(1));
            let lease = registry.begin(OperationName::Firmware).unwrap();
            let operation_id = lease.id();
            let barrier = Arc::new(Barrier::new(3));

            let commit_registry = Arc::clone(&registry);
            let commit_barrier = Arc::clone(&barrier);
            let commit = thread::spawn(move || {
                commit_barrier.wait();
                commit_registry.begin_commit(OperationName::Firmware, operation_id)
            });

            let cancel_registry = Arc::clone(&registry);
            let cancel_barrier = Arc::clone(&barrier);
            let cancel = thread::spawn(move || {
                cancel_barrier.wait();
                cancel_registry.cancel(OperationName::Firmware, operation_id)
            });

            barrier.wait();
            assert!(matches!(
                (commit.join().unwrap(), cancel.join().unwrap()),
                (CommitOutcome::Started, CancelOutcome::TooLate)
                    | (CommitOutcome::Cancelled, CancelOutcome::Requested)
            ));
        }
    }
}
