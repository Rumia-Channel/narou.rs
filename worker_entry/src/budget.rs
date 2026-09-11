//! Per-invocation subrequest accounting (Workers Paid optimization).
//!
//! Every `fetch()` issued by this isolate — site downloads, Wasabi object
//! operations, and Durable Object permit calls — counts against the
//! platform's 1,000-subrequest-per-invocation hard limit. D1, Queue, and
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
///
/// `should_yield` is checked only before a section starts, so whichever
/// limit trips first produces a resumable `Partial` outcome instead of a
/// hard platform abort.
pub struct WorkerBudget<'a> {
    deadline_ms: f64,
    subrequests: &'a SubrequestBudget,
    subrequest_limit: u64,
}

impl<'a> WorkerBudget<'a> {
    pub fn new(duration: Duration, subrequests: &'a SubrequestBudget) -> Self {
        Self {
            deadline_ms: js_sys::Date::now() + duration.as_secs_f64() * 1_000.0,
            subrequests,
            subrequest_limit: JOB_SUBREQUEST_BUDGET,
        }
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
    fn should_yield(&mut self, _next_section_index: usize) -> bool {
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
