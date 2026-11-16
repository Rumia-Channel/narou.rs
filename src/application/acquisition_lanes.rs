//! Durable scheduling for independently isolated acquisition workers.
//!
//! Queue deliveries are fenced by a reservation token. Execution ownership is
//! still the job ledger's lease, so a cancelled or crashed invocation cannot
//! release a successor's lane or bypass per-novel exclusion.

use super::jobs::{JobId, worker_ledger::{self, Statement, novel_id_expression}};

pub const LANE_COUNT: u8 = 4;
pub const ENVELOPE_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaneAssignment {
    pub lane: u8,
    pub job_id: JobId,
    pub site_key: String,
    pub dispatch_token: String,
    pub lease_until: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AcquisitionEnvelope {
    pub version: u32,
    pub job_id: JobId,
    pub lane: u8,
    pub dispatch_token: String,
}

impl AcquisitionEnvelope {
    pub fn new(assignment: &LaneAssignment) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            job_id: assignment.job_id.clone(),
            lane: assignment.lane,
            dispatch_token: assignment.dispatch_token.clone(),
        }
    }

    pub fn is_valid(&self) -> bool {
        self.version == ENVELOPE_VERSION && self.lane < LANE_COUNT
            && !self.job_id.as_str().is_empty() && !self.dispatch_token.is_empty()
    }
}

fn statement(sql: impl Into<String>, binds: &[&str]) -> Statement {
    Statement { sql: sql.into(), binds: binds.iter().map(|value| (*value).to_owned()).collect() }
}

fn lane_value(lane: u8) -> &'static str {
    match lane { 0 => "0", 1 => "1", 2 => "2", 3 => "3", _ => "-1" }
}

fn same_novel(left: &str, right: &str) -> String {
    format!("({left} = {right} OR ({}) = ({}))", novel_id_expression(left), novel_id_expression(right))
}

/// Read admission metadata through the same alias index used by claim exclusion.
/// Full novel decoding belongs to the claimed execution, not to scheduling.
pub fn lookup_novel(target: &str) -> Statement {
    statement(format!(
        "SELECT id, ncode, toc_url FROM novels WHERE id = ({})",
        novel_id_expression("?1"),
    ), &[target])
}

pub fn set_identity(now: &str, job_id: &str, target: &str, site: &str) -> Statement {
    statement(
        "UPDATE worker_jobs SET normalized_target = ?3, site_key = ?4, admission_waiting = 1, \
         wait_reason = 'lane', updated_at = ?1 WHERE job_id = ?2 \
         AND kind IN ('download', 'update', 'author_check') AND status IN ('pending', 'retryable') \
         AND NOT EXISTS (SELECT 1 FROM worker_acquisition_lanes WHERE job_id = ?2)",
        &[now, job_id, target, site],
    )
}

/// Reclassify a queued job before releasing its old assignment. Both this
/// statement and `release` run in one batch, leaving no old-identity gap.
pub fn reclassify(now: &str, envelope: &AcquisitionEnvelope, target: &str, site: &str) -> Statement {
    statement(
        "UPDATE worker_jobs SET normalized_target = ?5, site_key = ?6, admission_waiting = 1, \
         wait_reason = 'lane', updated_at = ?1 \
         WHERE job_id = ?2 AND status IN ('pending', 'retryable') AND execution_token IS NULL \
           AND EXISTS (SELECT 1 FROM worker_acquisition_lanes WHERE lane = ?3 AND job_id = ?2 AND dispatch_token = ?4)",
        &[now, envelope.job_id.as_str(), lane_value(envelope.lane), &envelope.dispatch_token, target, site],
    )
}

/// Reserve one free lane and one eligible family in one SQLite statement.
/// Non-vacant reservations remain owners until explicit release or recovery;
/// merely observing an expired timestamp must not let a competing dispatcher
/// skip recovery accounting.
pub fn reserve(now: &str, lease_until: &str, token: &str) -> Statement {
    let candidate = "COALESCE(candidate.normalized_target, candidate.target)";
    let owner = "COALESCE(other.normalized_target, other.target)";
    let reserved = "COALESCE(reserved.normalized_target, reserved.target)";
    statement(format!(
        "WITH candidate AS MATERIALIZED ( \
           SELECT candidate.job_id, candidate.site_key FROM worker_jobs AS candidate \
           WHERE candidate.kind IN ('download', 'update', 'author_check') \
             AND candidate.status IN ('pending', 'retryable') \
             AND candidate.site_key IS NOT NULL AND candidate.normalized_target IS NOT NULL \
             AND (candidate.lease_until IS NULL OR candidate.lease_until <= ?1) \
             AND candidate.recovery_attempts <= {max_recovery} \
             AND NOT EXISTS (SELECT 1 FROM worker_acquisition_lanes AS lane \
               JOIN worker_jobs AS reserved ON reserved.job_id = lane.job_id \
               WHERE lane.job_id = candidate.job_id OR lane.site_key = candidate.site_key \
                 OR {reservation_conflict}) \
             AND NOT EXISTS (SELECT 1 FROM worker_jobs AS other \
               WHERE other.job_id != candidate.job_id AND other.lease_until > ?1 \
                 AND (other.status = 'running' OR (other.status = 'permanent' AND other.execution_token IS NOT NULL)) \
                 AND (other.normalized_target IS NULL OR {execution_conflict} \
                   OR (other.kind IN ('download', 'update', 'author_check') \
                     AND (other.site_key IS NULL OR other.site_key = candidate.site_key)))) \
           ORDER BY candidate.created_at, candidate.job_id LIMIT 1 \
         ), free AS (SELECT lane FROM worker_acquisition_lanes WHERE job_id IS NULL ORDER BY lane LIMIT 1) \
         UPDATE worker_acquisition_lanes SET job_id = candidate.job_id, site_key = candidate.site_key, \
           dispatch_token = ?3, lease_until = ?2 FROM candidate, free \
         WHERE worker_acquisition_lanes.lane = free.lane AND worker_acquisition_lanes.job_id IS NULL",
        max_recovery = worker_ledger::MAX_RECOVERY_ATTEMPTS,
        reservation_conflict = same_novel(reserved, candidate),
        execution_conflict = same_novel(owner, candidate),
    ), &[now, lease_until, token])
}

pub fn mark_reserved(now: &str, token: &str) -> Statement {
    statement(
        "UPDATE worker_jobs SET admission_waiting = 0, wait_reason = 'dispatch', updated_at = ?1 \
         WHERE status IN ('pending', 'retryable') AND job_id IN \
           (SELECT job_id FROM worker_acquisition_lanes WHERE dispatch_token = ?2)",
        &[now, token],
    )
}

/// Shared exclusion for conversion and acquisition claims. Reservations count
/// before delivery, closing the gap between dispatch and execution admission.
pub fn guard_reservations(mut claim: Statement, job_id: &str, target: &str) -> Statement {
    let job_bind = claim.binds.len() + 1;
    let target_bind = job_bind + 1;
    let candidate = format!("?{target_bind}");
    let reserved = "COALESCE(reserved.normalized_target, reserved.target)";
    claim.sql.push_str(&format!(
        " AND NOT EXISTS (SELECT 1 FROM worker_acquisition_lanes AS lane \
         JOIN worker_jobs AS reserved ON reserved.job_id = lane.job_id \
         WHERE lane.job_id != ?{job_bind} AND {conflict})",
        conflict = same_novel(reserved, &candidate),
    ));
    claim.binds.extend([job_id.to_owned(), target.to_owned()]);
    claim
}

pub fn claim(now: &str, lease_until: &str, envelope: &AcquisitionEnvelope, target: &str, site: &str) -> Statement {
    let mut claim = guard_reservations(worker_ledger::claim_fresh(
        now, lease_until, &envelope.dispatch_token, envelope.job_id.as_str(), target, Some(site),
    ), envelope.job_id.as_str(), target);
    let offset = claim.binds.len();
    claim.sql.push_str(&format!(
        " AND EXISTS (SELECT 1 FROM worker_acquisition_lanes AS lane \
         WHERE lane.lane = ?{} AND lane.job_id = worker_jobs.job_id \
           AND lane.dispatch_token = ?{} AND lane.lease_until > ?{} AND lane.site_key = ?{})",
        offset + 1, offset + 2, offset + 3, offset + 4,
    ));
    claim.binds.extend([
        envelope.lane.to_string(), envelope.dispatch_token.clone(), now.to_owned(), site.to_owned(),
    ]);
    claim
}

pub fn renew_claim(lease_until: &str, envelope: &AcquisitionEnvelope) -> Statement {
    statement(
        "UPDATE worker_acquisition_lanes SET lease_until = ?1 \
         WHERE lane = ?2 AND job_id = ?3 AND dispatch_token = ?4 \
         AND EXISTS (SELECT 1 FROM worker_jobs WHERE job_id = ?3 AND status = 'running' AND execution_token = ?4)",
        &[lease_until, lane_value(envelope.lane), envelope.job_id.as_str(), &envelope.dispatch_token],
    )
}

/// Do not call until the invocation's physical work has stopped. A failed
/// handler that left a running job is deliberately retained for crash recovery.
pub fn finish_job(now: &str, envelope: &AcquisitionEnvelope) -> Statement {
    statement(
        "UPDATE worker_jobs SET execution_token = NULL, \
           admission_waiting = CASE WHEN status IN ('pending', 'retryable') THEN 1 ELSE 0 END, \
           wait_reason = CASE WHEN status IN ('pending', 'retryable') THEN 'lane' ELSE NULL END, updated_at = ?1 \
         WHERE job_id = ?3 AND status != 'running' \
           AND NOT (status = 'permanent' AND execution_token IS NOT NULL) \
           AND (execution_token IS NULL OR execution_token = ?4) \
           AND EXISTS (SELECT 1 FROM worker_acquisition_lanes WHERE lane = ?2 AND job_id = ?3 AND dispatch_token = ?4)",
        &[now, lane_value(envelope.lane), envelope.job_id.as_str(), &envelope.dispatch_token],
    )
}

pub fn release(envelope: &AcquisitionEnvelope) -> Statement {
    statement(
        "UPDATE worker_acquisition_lanes SET job_id = NULL, site_key = NULL, dispatch_token = NULL, lease_until = NULL \
         WHERE lane = ?1 AND job_id = ?2 AND dispatch_token = ?3 \
           AND NOT EXISTS (SELECT 1 FROM worker_jobs WHERE job_id = ?2 \
             AND (status = 'running' OR (status = 'permanent' AND execution_token IS NOT NULL)))",
        &[lane_value(envelope.lane), envelope.job_id.as_str(), &envelope.dispatch_token],
    )
}

/// Batch before pending cancellation. Never-claimed reservations have no
/// physical owner and must not wait for delivery (which may already be lost).
pub fn release_pending(job_id: Option<&str>) -> Statement {
    let filter = if job_id.is_some() { " AND job.job_id = ?1" } else { "" };
    Statement {
        sql: format!(
            "UPDATE worker_acquisition_lanes SET job_id = NULL, site_key = NULL, dispatch_token = NULL, lease_until = NULL \
             WHERE EXISTS (SELECT 1 FROM worker_jobs AS job \
               WHERE job.job_id = worker_acquisition_lanes.job_id AND job.status IN ('pending', 'retryable') \
                 AND job.execution_token IS NULL{filter})"
        ),
        binds: job_id.into_iter().map(str::to_owned).collect(),
    }
}

pub fn park_capacity(now: &str, envelope: &AcquisitionEnvelope) -> Statement {
    statement(
        "UPDATE worker_jobs SET wait_reason = 'capacity', updated_at = ?1 \
         WHERE job_id = ?3 AND status IN ('pending', 'retryable') \
           AND EXISTS (SELECT 1 FROM worker_acquisition_lanes WHERE lane = ?2 AND job_id = ?3 AND dispatch_token = ?4)",
        &[now, lane_value(envelope.lane), envelope.job_id.as_str(), &envelope.dispatch_token],
    )
}

/// Only a lost delivery or expired physical execution spends recovery budget.
/// A recorded provider failure already spent its business attempt; retain that
/// backoff instead of turning it into an immediately runnable pending row.
pub fn recover_jobs(now: &str) -> Statement {
    statement(format!(
        "UPDATE worker_jobs AS job SET \
           status = CASE WHEN status IN ('pending', 'running') AND recovery_attempts >= {max_recovery} THEN 'permanent' \
             WHEN status = 'running' THEN 'pending' ELSE status END, \
           last_error = CASE WHEN status IN ('pending', 'running') AND recovery_attempts >= {max_recovery} \
             THEN 'acquisition execution recovery exhausted without checkpoint progress' ELSE last_error END, \
           recovery_attempts = CASE WHEN status IN ('pending', 'running') AND recovery_attempts < {max_recovery} \
             THEN recovery_attempts + 1 ELSE recovery_attempts END, \
           admission_waiting = CASE WHEN status = 'permanent' \
             OR (status IN ('pending', 'running') AND recovery_attempts >= {max_recovery}) THEN 0 ELSE 1 END, \
           wait_reason = CASE WHEN status = 'permanent' \
             OR (status IN ('pending', 'running') AND recovery_attempts >= {max_recovery}) THEN NULL ELSE 'lane' END, \
           lease_until = CASE WHEN status IN ('running', 'permanent') THEN NULL ELSE lease_until END, \
           execution_token = NULL, updated_at = ?1 \
         WHERE status IN ('pending', 'running', 'retryable', 'permanent') \
           AND (status NOT IN ('running', 'permanent') OR lease_until IS NULL OR lease_until <= ?1) \
           AND EXISTS (SELECT 1 FROM worker_acquisition_lanes AS lane \
             WHERE lane.job_id = job.job_id AND lane.lease_until <= ?1 \
               AND (job.execution_token IS NULL OR job.execution_token = lane.dispatch_token))",
        max_recovery = worker_ledger::MAX_RECOVERY_ATTEMPTS,
    ), &[now])
}

pub fn recover_lanes(now: &str) -> Statement {
    statement(
        "UPDATE worker_acquisition_lanes SET job_id = NULL, site_key = NULL, dispatch_token = NULL, lease_until = NULL \
         WHERE lease_until <= ?1 AND NOT EXISTS (SELECT 1 FROM worker_jobs AS job \
           WHERE job.job_id = worker_acquisition_lanes.job_id AND job.lease_until > ?1 \
             AND (job.status = 'running' OR (job.status = 'permanent' AND job.execution_token IS NOT NULL)))",
        &[now],
    )
}

#[cfg(all(test, feature = "native-runtime"))]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    const NOW: &str = "2026-11-16T00:00:00Z";
    const LEASE: &str = "2026-11-16T00:15:00Z";
    const LATER: &str = "2026-11-16T00:16:00Z";

    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE worker_jobs (job_id TEXT PRIMARY KEY, kind TEXT NOT NULL, target TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending', site_key TEXT, normalized_target TEXT, admission_waiting INTEGER NOT NULL DEFAULT 0, execution_token TEXT, lease_until TEXT, attempts INTEGER NOT NULL DEFAULT 0, recovery_attempts INTEGER NOT NULL DEFAULT 0, checkpoint_json TEXT, last_error TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL); CREATE TABLE novels(id INTEGER PRIMARY KEY); CREATE TABLE novel_target_aliases(target TEXT, novel_id INTEGER);").unwrap();
        db.execute_batch(include_str!("../../worker_entry/migrations/0020_acquisition_lanes.sql")).unwrap();
        db
    }

    fn apply(db: &Connection, statement: Statement) -> usize {
        db.execute(&statement.sql, rusqlite::params_from_iter(statement.binds)).unwrap()
    }

    fn insert(db: &Connection, id: &str, target: &str, site: &str) {
        db.execute("INSERT INTO worker_jobs(job_id,kind,target,normalized_target,site_key,created_at,updated_at) VALUES (?1,'update',?2,?2,?3,?4,?4)", params![id,target,site,NOW]).unwrap();
    }

    fn reserve_one(db: &Connection, token: &str) -> Option<AcquisitionEnvelope> {
        apply(db, reserve(NOW, LEASE, token));
        apply(db, mark_reserved(NOW, token));
        db.query_row("SELECT lane,job_id FROM worker_acquisition_lanes WHERE dispatch_token = ?", [token], |row| {
            Ok(AcquisitionEnvelope { version: ENVELOPE_VERSION, job_id: JobId(row.get(1)?), lane: row.get(0)?, dispatch_token: token.into() })
        }).ok()
    }

    #[test]
    fn free_lanes_admit_distinct_families_without_hash_affinity() {
        let db = database();
        for (id, site) in [("a1","a"),("a2","a"),("b","b"),("c","c"),("d","d"),("e","e")] {
            insert(&db,id,id,site);
        }
        let mut assignments = Vec::new();
        for index in 0..LANE_COUNT {
            assignments.push(reserve_one(&db, &format!("token-{index}")).unwrap());
        }
        let targets: Vec<_> = assignments.iter().map(|a| a.job_id.as_str()).collect();
        assert_eq!(targets, ["a1","b","c","d"]);
        assert!(reserve_one(&db,"full").is_none());
        apply(&db, claim(NOW, LEASE, &assignments[0], "a1", "a"));
        db.execute("UPDATE worker_jobs SET status='succeeded',execution_token=NULL WHERE job_id='a1'", []).unwrap();
        apply(&db, finish_job(NOW,&assignments[0]));
        apply(&db, release(&assignments[0]));
        assert_eq!(reserve_one(&db,"next").unwrap().job_id.as_str(), "a2");
    }

    #[test]
    fn stale_delivery_cannot_execute_or_release_reassigned_lane() {
        let db = database();
        insert(&db,"first","1","a");
        let old = reserve_one(&db,"old").unwrap();
        apply(&db,recover_jobs(LATER));
        apply(&db,recover_lanes(LATER));
        apply(&db,reserve(LATER,"2026-11-16T00:31:00Z","new"));
        assert_eq!(apply(&db,claim(LATER,LEASE,&old,"1","a")),0);
        assert_eq!(apply(&db,release(&old)),0);
        let token: String = db.query_row("SELECT dispatch_token FROM worker_acquisition_lanes WHERE lane=0",[],|row|row.get(0)).unwrap();
        assert_eq!(token,"new");
    }

    #[test]
    fn reservation_excludes_conversion_before_acquisition_delivery() {
        let db = database();
        insert(&db,"download","https://example.test/book","a");
        db.execute("INSERT INTO novel_target_aliases VALUES ('https://example.test/book',42)",[]).unwrap();
        reserve_one(&db,"owner").unwrap();
        insert(&db,"convert","42","other");
        db.execute("UPDATE worker_jobs SET kind='convert' WHERE job_id='convert'",[]).unwrap();
        let conversion=worker_ledger::claim_fresh(NOW,LEASE,"convert-token","convert","42",None);
        assert_eq!(apply(&db,guard_reservations(conversion,"convert","42")),0);
    }

    #[test]
    fn live_cancelled_invocation_keeps_its_lane_and_site() {
        let db = database();
        insert(&db,"owner","1","a");
        insert(&db,"successor","2","a");
        let owner=reserve_one(&db,"token").unwrap();
        assert_eq!(apply(&db,claim(NOW,LEASE,&owner,"1","a")),1);
        db.execute("UPDATE worker_jobs SET status='permanent' WHERE job_id='owner'",[]).unwrap();
        assert_eq!(apply(&db,finish_job(NOW,&owner)),0);
        assert_eq!(apply(&db,release(&owner)),0);
        apply(&db,recover_jobs(NOW));
        apply(&db,recover_lanes(NOW));
        assert!(reserve_one(&db,"blocked").is_none());
        apply(&db,recover_jobs(LATER));
        apply(&db,recover_lanes(LATER));
        apply(&db,reserve(LATER,"2026-11-16T00:31:00Z","released"));
        let target:String=db.query_row("SELECT job_id FROM worker_acquisition_lanes WHERE dispatch_token='released'",[],|r|r.get(0)).unwrap();
        assert_eq!(target,"successor");
    }

    #[test]
    fn crash_recovery_preserves_progress_and_bounds_unproductive_restarts() {
        let db = database();
        insert(&db,"crashed","1","a");
        let owner=reserve_one(&db,"token").unwrap();
        apply(&db,claim(NOW,LEASE,&owner,"1","a"));
        db.execute("UPDATE worker_jobs SET attempts=2,checkpoint_json='saved',recovery_attempts=2 WHERE job_id='crashed'",[]).unwrap();
        apply(&db,recover_jobs(LATER));
        apply(&db,recover_lanes(LATER));
        let state:(String,i64,i64,String)=db.query_row("SELECT status,attempts,recovery_attempts,checkpoint_json FROM worker_jobs WHERE job_id='crashed'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(state,("pending".into(),2,3,"saved".into()));
        apply(&db,reserve(LATER,"2026-11-16T00:31:00Z","retry"));
        apply(&db,recover_jobs("2026-11-16T00:32:00Z"));
        apply(&db,recover_lanes("2026-11-16T00:32:00Z"));
        let status:String=db.query_row("SELECT status FROM worker_jobs WHERE job_id='crashed'",[],|r|r.get(0)).unwrap();
        assert_eq!(status,"permanent");
    }

    #[test]
    fn retry_release_retains_backoff_and_does_not_spend_recovery_budget() {
        let db = database();
        insert(&db,"retry","1","a");
        let owner=reserve_one(&db,"token").unwrap();
        apply(&db,claim(NOW,LEASE,&owner,"1","a"));
        db.execute("UPDATE worker_jobs SET status='retryable',attempts=1,lease_until='2026-11-16T00:20:00Z' WHERE job_id='retry'",[]).unwrap();
        apply(&db,finish_job(NOW,&owner));
        apply(&db,release(&owner));
        assert!(reserve_one(&db,"too-early").is_none());
        let state:(i64,i64,String)=db.query_row("SELECT attempts,recovery_attempts,lease_until FROM worker_jobs WHERE job_id='retry'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(state,(1,0,"2026-11-16T00:20:00Z".into()));
    }

    #[test]
    fn yaml_reclassification_joins_the_current_family_and_fences_old_deliveries() {
        let db = database();
        insert(&db,"a","1","alpha");
        insert(&db,"b","2","beta");
        let old = reserve_one(&db,"old-alpha").unwrap();
        let beta = reserve_one(&db,"beta-owner").unwrap();
        apply(&db,reclassify(NOW,&old,"1","beta"));
        apply(&db,release(&old));
        assert!(reserve_one(&db,"must-wait-for-beta").is_none());
        db.execute("UPDATE worker_jobs SET status='succeeded',execution_token=NULL WHERE job_id='b'",[]).unwrap();
        apply(&db,finish_job(NOW,&beta));
        apply(&db,release(&beta));
        let current = reserve_one(&db,"new-beta").unwrap();
        assert_eq!(current.job_id.as_str(),"a");
        assert_eq!(apply(&db,reclassify(NOW,&old,"1","alpha")),0);
        assert_eq!(apply(&db,release(&old)),0);
        assert_eq!(apply(&db,claim(NOW,LEASE,&current,"1","beta")),1);
    }

    #[test]
    fn cancelling_pending_assignments_frees_capacity_without_evicting_live_owners() {
        let db = database();
        for (id, site) in [("a1", "a"), ("a2", "a"), ("b", "b"), ("c", "c")] {
            insert(&db, id, id, site);
        }
        let pending = reserve_one(&db, "pending").unwrap();
        let running = reserve_one(&db, "running").unwrap();
        reserve_one(&db, "untouched").unwrap();
        assert_eq!(apply(&db, claim(NOW, LEASE, &running, "b", "b")), 1);
        db.execute_batch("BEGIN").unwrap();
        assert_eq!(apply(&db, release_pending(Some("a1"))), 1);
        db.execute("UPDATE worker_jobs SET status='permanent' WHERE job_id='a1'", []).unwrap();
        db.execute_batch("COMMIT").unwrap();
        let replacement = reserve_one(&db, "replacement").unwrap();
        assert_eq!(replacement.job_id.as_str(), "a2");
        assert_eq!(apply(&db, release(&pending)), 0);
        assert_eq!(apply(&db, claim(NOW, LEASE, &pending, "a1", "a")), 0);
        db.execute_batch("BEGIN").unwrap();
        assert_eq!(apply(&db, release_pending(None)), 2);
        db.execute("UPDATE worker_jobs SET status='permanent' WHERE status='pending'", []).unwrap();
        db.execute_batch("COMMIT").unwrap();
        db.execute("UPDATE worker_jobs SET status='permanent' WHERE job_id='b'", []).unwrap();
        assert_eq!(apply(&db, release_pending(None)), 0);
        let owner: String = db.query_row("SELECT job_id FROM worker_acquisition_lanes WHERE job_id IS NOT NULL", [], |row| row.get(0)).unwrap();
        assert_eq!(owner, "b");
    }
}
