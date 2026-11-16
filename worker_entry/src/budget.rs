//! Per-invocation subrequest accounting (Workers Paid optimization).
//!
//! Tracks instrumented HTTP and Durable Object requests, not every binding
//! operation. D1 and Queue requests also count toward Cloudflare's platform
//! limit; this counter does not claim to account for them.
//!
//! Workers Paid currently defaults to 10,000 subrequests per invocation,
//! configurable per Worker: <https://developers.cloudflare.com/workers/platform/limits/#subrequests>.
//! The 850-request job yield point is an intentionally conservative local
//! policy, not a hard platform limit. [`WorkerBudget`] combines it with the
//! wall-clock deadline to yield before sections and uncached images.
//!
//! [`JOB_TIME_BUDGET`]: crate::executor::JOB_TIME_BUDGET

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use narou_rs::application::JobQueue;
use narou_rs::downloader::SectionBudget;
use narou_rs::application::novel_access::AccessOrigin;
use narou_rs::error::Result;


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

struct CheckpointSink<'a> {
    ledger: SendLedger,
    job_id: &'a narou_rs::application::JobId,
    owner: Arc<narou_rs::platform::JobExecutionOwner>,
    novel_id: Option<narou_rs::platform::NovelId>,
    write_origin: Option<AccessOrigin>,
    creation_reservation: Option<(narou_rs::platform::NovelId, i64)>,
    last_saved_index: u64,
    latest_boundary: Option<u64>,
}

impl CheckpointSink<'_> {
    fn checkpoint(&self) -> narou_rs::application::WorkerExecutionCheckpoint {
        let mut checkpoint = narou_rs::application::WorkerExecutionCheckpoint::planned((*self.job_id).clone(), self.novel_id);
        checkpoint.write_origin = self.write_origin.clone();
        checkpoint.creation_reservation = self.creation_reservation;
        if let Some(next) = self.latest_boundary {
            checkpoint.phase = narou_rs::application::ExecutionPhase::Fetching;
            checkpoint.next_section_index = Some(next);
        }
        checkpoint
    }

    /// Only remember the latest boundary. Persistence is awaited at the next
    /// async boundary, never spawned into an unbounded receiver/task backlog.
    fn save(&mut self, next_section_index: u64) {
        self.latest_boundary = Some(next_section_index);
    }

    async fn flush(&mut self) -> Result<()> {
        let Some(next) = self.latest_boundary else { return Ok(()) };
        if next < self.last_saved_index.saturating_add(CHECKPOINT_EVERY_SECTIONS) {
            return Ok(());
        }
        let checkpoint = self.checkpoint();
        self.ledger.save_checkpoint(self.job_id, self.owner.execution_token(), &checkpoint).await?;
        self.last_saved_index = next;
        Ok(())
    }
}

/// Budget combining wall-clock and subrequest limits at resumable boundaries.
///
/// Checks before sections and uncached images keep long illustrated sections
/// resumable. The downloader saves the image index before returning `Partial`;
/// the checkpoint retains the unfinished section rather than skipping its images.
pub struct WorkerBudget<'a> {
    deadline_ms: f64,
    subrequests: &'a SubrequestBudget,
    subrequest_limit: u64,
    sink: Option<CheckpointSink<'a>>,
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
        job_id: &'a narou_rs::application::JobId,
        owner: Arc<narou_rs::platform::JobExecutionOwner>,
        novel_id: Option<narou_rs::platform::NovelId>,
    ) -> Self {
        self.sink = Some(CheckpointSink {
            ledger: send_wrapper::SendWrapper::new(ledger),
            job_id,
            owner,
            novel_id,
            write_origin: None,
            creation_reservation: None,
            last_saved_index: 0,
            latest_boundary: None,
        });
        self
    }

    pub fn with_resume_checkpoint(mut self, checkpoint: Option<&narou_rs::application::WorkerExecutionCheckpoint>) -> Self {
        if let (Some(sink), Some(checkpoint)) = (&mut self.sink, checkpoint) {
            sink.write_origin = checkpoint.write_origin.clone();
            sink.creation_reservation = checkpoint.creation_reservation;
            // An unproven legacy cursor is never blessed by the first origin.
            // Numeric-only planned creation ownership has no cursor to adopt.
            sink.latest_boundary = checkpoint.write_origin.as_ref().and(checkpoint.next_section_index);
            sink.last_saved_index = sink.latest_boundary.unwrap_or(0);
        }
        self
    }

    /// Wait for periodic writes before replacing or finalizing the cursor.
    pub(crate) async fn drain_checkpoint_writes(&mut self) -> Result<()> {
        match &mut self.sink {
            Some(sink) => sink.flush().await,
            None => Ok(()),
        }
    }

    /// Persist the latest completed boundary before recording a retryable error.
    /// A failure before any boundary preserves the incoming resume checkpoint.
    pub async fn save_retry_progress(&mut self) -> Result<()> {
        self.drain_checkpoint_writes().await?;
        let Some(sink) = &self.sink else {
            return Ok(());
        };
        if sink.latest_boundary.is_none() { return Ok(()); }
        let checkpoint = sink.checkpoint();
        sink.ledger
            .save_checkpoint(sink.job_id, sink.owner.execution_token(), &checkpoint)
            .await
    }

    pub fn write_origin(&self) -> Option<&AccessOrigin> {
        self.sink.as_ref().and_then(|sink| sink.write_origin.as_ref())
    }

    pub fn creation_reservation(&self) -> Option<(narou_rs::platform::NovelId, i64)> {
        self.sink.as_ref().and_then(|sink| sink.creation_reservation)
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
    fn job_owner(&self) -> Option<Arc<narou_rs::platform::JobExecutionOwner>> {
        self.sink.as_ref().map(|sink| Arc::clone(&sink.owner))
    }

    fn allocate_creation_id<'a>(&'a mut self, novels: &'a dyn narou_rs::platform::NovelRepository) -> narou_rs::platform::PlatformFuture<'a, Result<(narou_rs::platform::NovelIdReservation, bool)>> {
        Box::pin(async move {
            let Some(sink) = &mut self.sink else {
                return Err(narou_rs::error::NarouError::Platform("durable worker allocation sink is missing".into()));
            };
            let reservation = novels.allocate_id_for_job(&sink.owner).await?;
            let identity = (reservation.id(), reservation.generation());
            if sink.creation_reservation.is_some_and(|saved| saved != identity) {
                return Err(narou_rs::error::NarouError::Platform("worker creation allocation changed its durable generation".into()));
            }
            sink.creation_reservation = Some(identity);
            Ok((reservation, true))
        })
    }

    fn restore_creation_id<'a>(&'a mut self, novels: &'a dyn narou_rs::platform::NovelRepository, id: narou_rs::platform::NovelId, generation: i64) -> narou_rs::platform::PlatformFuture<'a, Result<narou_rs::platform::NovelIdReservation>> {
        Box::pin(async move {
            let Some(sink) = &mut self.sink else {
                return Err(narou_rs::error::NarouError::Platform("durable worker reservation restore sink is missing".into()));
            };
            if sink.creation_reservation != Some((id, generation)) {
                return Err(narou_rs::error::NarouError::Platform("worker reservation restore does not match its durable checkpoint".into()));
            }
            let reservation = novels.resume_id_for_job(&sink.owner, id, generation).await?;
            if (reservation.id(), reservation.generation()) != (id, generation) {
                return Err(narou_rs::error::NarouError::Platform("worker reservation restore changed its original generation".into()));
            }
            Ok(reservation)
        })
    }

    fn set_access_origin<'a>(&'a mut self, origin: &'a AccessOrigin, creation_reservation: Option<(narou_rs::platform::NovelId, i64)>) -> narou_rs::platform::PlatformFuture<'a, Result<bool>> {
        Box::pin(async move {
            let Some(sink) = &mut self.sink else {
                return Err(narou_rs::error::NarouError::Platform("durable worker checkpoint sink is missing".into()));
            };
            if sink.write_origin.as_ref().is_some_and(|saved| saved != origin) {
                return Err(narou_rs::error::NarouError::Platform("worker continuation writer origin changed".into()));
            }
            if sink.creation_reservation.is_some() && sink.creation_reservation != creation_reservation {
                return Err(narou_rs::error::NarouError::Platform("worker continuation creation reservation changed".into()));
            }
            if sink.write_origin.is_some() && sink.creation_reservation.is_none() && creation_reservation.is_some() {
                return Err(narou_rs::error::NarouError::Platform("worker continuation has no original creation reservation".into()));
            }
            let mut checkpoint = sink.checkpoint();
            checkpoint.write_origin = Some(origin.clone());
            checkpoint.creation_reservation = creation_reservation;
            if let Err(error) = sink.ledger.save_checkpoint(sink.job_id, sink.owner.execution_token(), &checkpoint).await {
                // A response can be lost after the primary commit. Only an
                // exact read-back confirms the ownership acknowledgement.
                match sink.ledger.execution_checkpoint(sink.job_id).await {
                    Ok(Some(saved)) if saved == checkpoint => {
                        sink.ledger.ensure_execution_active(sink.job_id, sink.owner.execution_token()).await?;
                    },
                    _ => return Err(error),
                }
            }
            sink.write_origin = Some(origin.clone());
            sink.creation_reservation = creation_reservation;
            Ok(true)
        })
    }

    fn should_yield(&mut self, next_section_index: usize) -> bool {
        if let Some(sink) = &mut self.sink {
            sink.save(next_section_index as u64);
        }
        self.exceeded_reason().is_some()
    }

    fn check_active(&mut self) -> narou_rs::platform::PlatformFuture<'_, Result<()>> {
        Box::pin(async move {
            narou_rs::application::messages::ready().await;
            if let Some(sink) = &mut self.sink {
                sink.flush().await?;
                sink.ledger.ensure_execution_active(sink.job_id, sink.owner.execution_token()).await?;
            }
            Ok(())
        })
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

}
