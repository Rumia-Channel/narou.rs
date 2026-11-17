//! Acquisition lane dispatch.
//!
//! One heavy acquisition runs per isolate: the writer never executes an
//! acquisition itself. Every acquisition row carries a canonical identity —
//! the site family of the effective YAML definition plus the normalized
//! target — before it can be assigned, and the dispatcher moves at most one
//! ready row per free lane.
//!
//! The dispatcher never executes anything. It only reads durable rows and
//! sends version-3 envelopes to a lane queue, so it is safe to run from the
//! writer (after an enqueue) and from a lane (after a job drained).

use narou_rs::application::acquisition_lanes::{AcquisitionEnvelope, LANE_COUNT};
use narou_rs::application::{
    classify_failure, JobFailureClass, JobLedgerStatus, JobTarget, QueuedJob,
};
use narou_rs::downloader::Downloader;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::error::{NarouError, Result};
use worker::console_log;

use crate::composition::WorkerRuntime;

/// Bounded page of legacy rows whose identity is still missing.
const HYDRATE_LIMIT: usize = 16;

/// The canonical identity of one acquisition target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcquisitionIdentity {
    /// The YAML-resolved target stored with the row.
    pub normalized_target: String,
    /// The site family that owns the target's upstream series.
    pub site_key: String,
}

/// Resolve the canonical identity of one acquisition target.
///
/// The identity comes from the effective YAML definitions and the stored
/// novel metadata only: no downloader is built and no upstream request is
/// made, so an enqueue never pays the heavy admission cost. The YAML
/// precedence itself is the core's rule, shared with the downloader.
pub(crate) async fn resolve_identity(
    runtime: &WorkerRuntime,
    target: &JobTarget,
) -> Result<AcquisitionIdentity> {
    let settings = runtime.effective_site_settings().await?;
    resolve_identity_with(&settings, runtime, target).await
}

/// Resolve the identity with an already loaded definition snapshot.
///
/// A page of plans shares one snapshot, so the definitions are read once
/// instead of once per plan.
pub(crate) async fn resolve_identity_with(
    settings: &[SiteSetting],
    runtime: &WorkerRuntime,
    target: &JobTarget,
) -> Result<AcquisitionIdentity> {
    let target_text = target.as_str();
    if matches!(target, JobTarget::All) {
        return Err(NarouError::InvalidTarget(
            "auto-update must be planned into discrete per-novel jobs".to_string(),
        ));
    }
    // Use only indexed admission metadata. Full record decoding remains inside
    // the claim so corruption follows the normal bounded attempt settlement.
    let record = runtime.ledger.acquisition_novel(&target_text).await?;
    let site_key = match &record {
        // 仮想小説は上流を持たない no-op 更新。実在サイトと混ざらないよう
        // 小説ごとの family を割り当てる (小説単位の排他は正本に任せる)。
        Some(record) if record.ncode.as_deref().is_some_and(narou_rs::virtual_novel::is_virtual_ncode) => {
            format!("virtual:{}", record.id)
        }
        _ => Downloader::site_key_for_known_target(settings, &target_text, record.as_ref().map(|record| (record.id, record.toc_url.as_str())))?,
    };
    let normalized_target = match record {
        Some(record) => record.id.to_string(),
        None => match target {
            JobTarget::Url(url) => narou_rs::application::target_lookup::first_matching_toc_url(settings, url)
                .unwrap_or_else(|| url.clone()),
            _ => target_text,
        },
    };
    Ok(AcquisitionIdentity {
        normalized_target,
        site_key,
    })
}

/// Assign ready acquisition rows to free lanes and deliver them.
///
/// A failed delivery keeps the reservation: the row stays owned until the
/// lane lease expires, so a lost delivery is recovered instead of losing the
/// job.
pub(crate) async fn dispatch(runtime: &WorkerRuntime) -> Result<()> {
    hydrate_unresolved(runtime).await?;
    runtime.ledger.recover_acquisition_lanes().await?;
    for _ in 0..LANE_COUNT {
        let Some(assignment) = runtime.ledger.reserve_acquisition_lane().await? else {
            break;
        };
        let Some(queue) = runtime.lane_queue(assignment.lane) else {
            // A lane without a producer binding cannot be delivered. The
            // dispatcher fails closed instead of accepting work it can never
            // hand over; the reservation stays owned for crash recovery.
            return Err(NarouError::Platform(format!(
                "acquisition lane {} has no queue binding",
                assignment.lane
            )));
        };
        let envelope = AcquisitionEnvelope::new(&assignment);
        if let Err(error) = queue.send(envelope).await {
            console_log!(
                "acquisition lane {} delivery failed: {error}; keeping the reservation for recovery",
                assignment.lane
            );
            break;
        }
        console_log!(
            "acquisition lane {} assigned job {}",
            assignment.lane,
            assignment.job_id
        );
    }
    Ok(())
}

/// Give legacy rows the identity the dispatcher needs.
///
/// A row without an identity can never be reserved. An unresolvable target
/// is therefore settled with the durable error semantics the executor would
/// have written, instead of waiting forever.
async fn hydrate_unresolved(runtime: &WorkerRuntime) -> Result<()> {
    let rows = runtime.ledger.unresolved_acquisitions(HYDRATE_LIMIT).await?;
    if rows.is_empty() {
        return Ok(());
    }
    // The definitions are read once for the whole page.
    let settings = runtime.effective_site_settings().await?;
    for row in &rows {
        hydrate_one(runtime, &settings, row).await?;
    }
    Ok(())
}

/// Hydrate or settle one legacy row.
async fn hydrate_one(
    runtime: &WorkerRuntime,
    settings: &[SiteSetting],
    row: &QueuedJob,
) -> Result<()> {
    match resolve_identity_with(settings, runtime, &row.job.target).await {
        Ok(identity) => {
            runtime
                .ledger
                .set_acquisition_identity(
                    &row.job_id,
                    &identity.normalized_target,
                    &identity.site_key,
                )
                .await?;
        }
        Err(error) => {
            // A transient failure keeps the row unresolved: the next dispatch
            // tick tries again.
            let status = match classify_failure(&error) {
                JobFailureClass::Retryable => return Ok(()),
                JobFailureClass::Permanent => JobLedgerStatus::Permanent,
                JobFailureClass::Blocked => JobLedgerStatus::Blocked,
            };
            let reason = error.to_string();
            runtime
                .ledger
                .settle_acquisition_resolution_error(&row.job_id, status, &reason)
                .await?;
            console_log!("acquisition job {} cannot be resolved: {reason}", row.job_id);
            crate::consumer::broadcast_failure(runtime, runtime.push(), &row.job_id, &reason).await;
        }
    }
    Ok(())
}
