//! Per-invocation subrequest accounting (Workers Paid optimization).
//!
//! Every `fetch()` issued by this isolate — site downloads and Durable
//! Object permit calls — counts against the platform's
//! 1,000-subrequest-per-invocation hard limit. D1, Queue, and
//! other RPC bindings do not consume subrequests.
//!
//! [`SubrequestBudget`] is a cheap shared counter cloned into the HTTP
//! client, the object store, and the rate limiter. [`WorkerBudget`] combines
//! the counter with the wall-clock deadline so a job yields at a section
//! boundary *before* the hard limit can abort an in-flight section — the
//! same contract [`JOB_TIME_BUDGET`] already provides for time.
//!
//! [`JOB_TIME_BUDGET`]: crate::executor::JOB_TIME_BUDGET

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use narou_rs::application::JobQueue;
use narou_rs::downloader::SectionBudget;

/// Hard platform limit: 1,000 subrequests per invocation.
pub const SUBREQUEST_HARD_LIMIT: u64 = 1_000;

/// Soft yield point for one job. Leaves headroom for the ledger writes,
/// queue ack, and any non-download work sharing the invocation; a section
/// costs roughly 2–3 subrequests (permit + fetch + store), so ~280 sections
/// fit in one invocation.
pub const JOB_SUBREQUEST_BUDGET: u64 = 850;

/// Shared subrequest counter for one invocation.
///
/// Cloning shares the same counter; every instrumented client increments it
/// before issuing a `fetch`. The counter is intentionally not reset between
/// jobs in a batch — the platform limit applies to the whole invocation.
#[derive(Debug, Clone, Default)]
pub struct SubrequestBudget {
    used: Arc<AtomicU64>,
}

impl SubrequestBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one outgoing `fetch`. Called by instrumented clients only.
    pub fn record(&self) {
        self.used.fetch_add(1, Ordering::Relaxed);
    }

    /// Subrequests consumed so far in this invocation.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Remaining headroom against `limit`.
    pub fn remaining(&self, limit: u64) -> u64 {
        limit.saturating_sub(self.used())
    }
}

/// Section-boundary budget combining wall-clock and subrequest limits.

/// Persist a resume checkpoint every this many completed sections, so a
/// crashed invocation restarts near the last finished section instead of
/// re-scanning the whole novel.
pub const CHECKPOINT_EVERY_SECTIONS: u64 = 100;

/// Ledger handle wrapped for the `Send` bound on [`SectionBudget`].
///
/// wasm32 is single-threaded, so the wrapper can never be dereferenced on a
/// foreign thread; the bound exists only because the trait is shared with
/// the native runtime.
type SendLedger = send_wrapper::SendWrapper<std::sync::Arc<crate::ledger::D1JobLedger>>;

struct CheckpointSink {
    ledger: SendLedger,
    job_id: narou_rs::application::JobId,
    execution_token: String,
    novel_id: Option<narou_rs::platform::NovelId>,
    last_saved_index: u64,
}

impl CheckpointSink {
    /// Spawn a detached ledger write for the current section cursor.
    /// Best-effort: a stale execution token is ignored by the ledger.
    fn save(&mut self, next_section_index: u64) {
        if next_section_index < self.last_saved_index + CHECKPOINT_EVERY_SECTIONS {
            return;
        }
        self.last_saved_index = next_section_index;
        let ledger = self.ledger.clone();
        let checkpoint = narou_rs::application::WorkerExecutionCheckpoint::planned(
            self.job_id.clone(),
            self.novel_id,
        )
        .advance(
            narou_rs::application::ExecutionPhase::Fetching,
            Some(next_section_index),
        );
        let job_id = self.job_id.clone();
        let token = self.execution_token.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = ledger.save_checkpoint(&job_id, &token, &checkpoint).await;
        });
    }
}
///
/// `should_yield` is checked only before a section starts, so whichever
/// limit trips first produces a resumable `Partial` outcome instead of a
/// hard platform abort.
pub struct WorkerBudget<'a> {
    deadline_ms: f64,
    subrequests: &'a SubrequestBudget,
    subrequest_limit: u64,
    sink: Option<CheckpointSink>,
}

impl<'a> WorkerBudget<'a> {
    pub fn new(duration: Duration, subrequests: &'a SubrequestBudget) -> Self {
        Self {
            deadline_ms: js_sys::Date::now() + duration.as_secs_f64() * 1_000.0,
            subrequests,
            subrequest_limit: JOB_SUBREQUEST_BUDGET,
            sink: None,
        }
    }

    /// Attach periodic checkpoint persistence for the running job.
    pub fn with_checkpoints(
        mut self,
        ledger: std::sync::Arc<crate::ledger::D1JobLedger>,
        job_id: narou_rs::application::JobId,
        execution_token: String,
        novel_id: Option<narou_rs::platform::NovelId>,
    ) -> Self {
        self.sink = Some(CheckpointSink {
            ledger: send_wrapper::SendWrapper::new(ledger),
            job_id,
            execution_token,
            novel_id,
            last_saved_index: 0,
        });
        self
    }

    /// Which limit tripped, for the `Partial` reason string. Returns `None`
    /// while both budgets have headroom.
    pub fn exceeded_reason(&self) -> Option<&'static str> {
        if js_sys::Date::now() >= self.deadline_ms {
            Some("wall-clock")
        } else if self.subrequests.used() >= self.subrequest_limit {
            Some("subrequest")
        } else {
            None
        }
    }
}

impl SectionBudget for WorkerBudget<'_> {
    fn should_yield(&mut self, next_section_index: usize) -> bool {
        if let Some(sink) = &mut self.sink {
            sink.save(next_section_index as u64);
        }
        self.exceeded_reason().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_is_shared_across_clones() {
        let budget = SubrequestBudget::new();
        let clone = budget.clone();
        budget.record();
        clone.record();
        clone.record();
        assert_eq!(budget.used(), 3);
        assert_eq!(clone.remaining(10), 7);
    }

    #[test]
    fn remaining_saturates_at_zero() {
        let budget = SubrequestBudget::new();
        for _ in 0..5 {
            budget.record();
        }
        assert_eq!(budget.remaining(3), 0);
    }

    #[test]
    fn job_budget_stays_below_hard_limit() {
        assert!(JOB_SUBREQUEST_BUDGET < SUBREQUEST_HARD_LIMIT);
    }
}
