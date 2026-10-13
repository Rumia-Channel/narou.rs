//! Portable progress reporting capability.
//!
//! The trait is defined here so the shared downloader (and any other core
//! module) can report progress without depending on the native terminal
//! implementations (`indicatif` progress bars, WebSocket event emission)
//! which live in `crate::progress` under the `native-runtime` feature.

/// Progress reporting sink injected into long-running core operations.
///
/// Implementations supply terminal or WebSocket transports; Web transports
/// share [`ProgressState`] so totals, positions and completion have one owner.
pub trait ProgressReporter: Send + Sync {
    fn set_length(&self, len: u64);
    fn set_position(&self, pos: u64);
    fn inc(&self, delta: u64);
    fn set_message(&self, msg: &str);
    fn finish_with_message(&self, msg: &str);
    fn println(&self, msg: &str);
}

/// Progress reporter that discards all output.
pub struct NoProgress;

impl ProgressReporter for NoProgress {
    fn set_length(&self, _len: u64) {}
    fn set_position(&self, _pos: u64) {}
    fn inc(&self, _delta: u64) {}
    fn set_message(&self, _msg: &str) {}
    fn finish_with_message(&self, _msg: &str) {}
    fn println(&self, msg: &str) {
        eprintln!("{}", msg);
    }
}

/// Identity of one operation. `scope` groups operations for job cleanup;
/// `operation` distinguishes concurrent novels, phases and execution attempts.
#[derive(Debug)]
pub struct ProgressIdentity {
    pub topic: String,
    pub scope: String,
    pub operation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressPhase {
    Init,
    Step,
    Clear,
}

#[derive(Debug, Default)]
pub struct ProgressSnapshot {
    pub current: u64,
    pub total: u64,
    pub sequence: u64,
    pub message: String,
    finished: bool,
}

impl ProgressSnapshot {
    pub fn percent(&self) -> Option<f64> {
        (self.total > 0).then(|| {
            self.current.min(self.total) as f64 / self.total as f64 * 100.0
        })
    }
}

static NEXT_OPERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Shared Web progress lifecycle. A transport is called while the state lock
/// is held, so parallel producers cannot publish an older snapshot last.
pub struct ProgressState {
    identity: ProgressIdentity,
    snapshot: parking_lot::Mutex<ProgressSnapshot>,
}

impl ProgressState {
    /// `instance` is supplied by the adapter: native process nonce or Worker
    /// execution token. It prevents ids being reused after a subprocess/isolate
    /// restart without introducing a platform clock or random source here.
    pub fn new(topic: &str, scope: &str, instance: impl std::fmt::Display) -> Self {
        let next = NEXT_OPERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            identity: ProgressIdentity {
                topic: topic.to_owned(),
                scope: scope.to_owned(),
                operation: format!("{instance}-{next}"),
            },
            snapshot: parking_lot::Mutex::new(ProgressSnapshot::default()),
        }
    }

    pub fn init(&self, emit: impl FnOnce(ProgressPhase, &ProgressIdentity, &ProgressSnapshot)) {
        let snapshot = self.snapshot.lock();
        if !snapshot.finished {
            emit(ProgressPhase::Init, &self.identity, &snapshot);
        }
    }

    fn change(
        &self,
        change: impl FnOnce(&mut ProgressSnapshot) -> bool,
        emit: impl FnOnce(ProgressPhase, &ProgressIdentity, &ProgressSnapshot),
    ) {
        let mut snapshot = self.snapshot.lock();
        if snapshot.finished || !change(&mut snapshot) {
            return;
        }
        snapshot.sequence = snapshot.sequence.saturating_add(1);
        emit(ProgressPhase::Step, &self.identity, &snapshot);
    }

    pub fn set_length(&self, total: u64, emit: impl FnOnce(ProgressPhase, &ProgressIdentity, &ProgressSnapshot)) {
        self.change(|snapshot| {
            if snapshot.total == total { return false; }
            snapshot.total = total;
            true
        }, emit);
    }

    pub fn set_position(&self, current: u64, emit: impl FnOnce(ProgressPhase, &ProgressIdentity, &ProgressSnapshot)) {
        self.change(|snapshot| {
            if snapshot.current == current { return false; }
            snapshot.current = current;
            true
        }, emit);
    }

    pub fn inc(&self, delta: u64, emit: impl FnOnce(ProgressPhase, &ProgressIdentity, &ProgressSnapshot)) {
        self.change(|snapshot| {
            let current = snapshot.current.saturating_add(delta);
            if snapshot.current == current { return false; }
            snapshot.current = current;
            true
        }, emit);
    }

    pub fn set_message(&self, message: &str, emit: impl FnOnce(ProgressPhase, &ProgressIdentity, &ProgressSnapshot)) {
        self.change(|snapshot| {
            if snapshot.message == message { return false; }
            snapshot.message.clear();
            snapshot.message.push_str(message);
            true
        }, emit);
    }

    pub fn finish(&self, emit: impl FnOnce(ProgressPhase, &ProgressIdentity, &ProgressSnapshot)) {
        let mut snapshot = self.snapshot.lock();
        if snapshot.finished { return; }
        snapshot.finished = true;
        snapshot.sequence = snapshot.sequence.saturating_add(1);
        emit(ProgressPhase::Clear, &self.identity, &snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_totals_and_shrinking_work_emit_the_current_fraction() {
        let progress = ProgressState::new("download", "job", "attempt");
        let mut events = Vec::new();
        let mut emit = |phase, _: &ProgressIdentity, snapshot: &ProgressSnapshot| {
            events.push((phase, snapshot.current, snapshot.total, snapshot.percent()));
        };
        progress.init(&mut emit);
        progress.inc(2, &mut emit);
        progress.set_length(4, &mut emit);
        progress.set_length(1, &mut emit);
        assert_eq!(events, vec![
            (ProgressPhase::Init, 0, 0, None),
            (ProgressPhase::Step, 2, 0, None),
            (ProgressPhase::Step, 2, 4, Some(50.0)),
            (ProgressPhase::Step, 2, 1, Some(100.0)),
        ]);
    }

    #[test]
    fn completion_is_idempotent_and_does_not_resurrect_or_fake_full_work() {
        let progress = ProgressState::new("download", "job", "attempt");
        let mut events = Vec::new();
        let mut emit = |phase, _: &ProgressIdentity, snapshot: &ProgressSnapshot| {
            events.push((phase, snapshot.current, snapshot.total, snapshot.sequence));
        };
        progress.set_length(4, &mut emit);
        progress.inc(1, &mut emit);
        progress.finish(&mut emit);
        progress.finish(&mut emit);
        progress.inc(1, &mut emit);
        assert_eq!(events, vec![
            (ProgressPhase::Step, 0, 4, 1),
            (ProgressPhase::Step, 1, 4, 2),
            (ProgressPhase::Clear, 1, 4, 3),
        ]);
    }

    #[test]
    fn concurrent_operations_in_one_job_have_independent_identities() {
        let first = ProgressState::new("update", "job", "attempt");
        let second = ProgressState::new("update", "job", "attempt");
        assert_eq!(first.identity.scope, second.identity.scope);
        assert_ne!(first.identity.operation, second.identity.operation);
    }
}
