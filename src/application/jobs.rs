//! Job planning service: deterministic job plans, no execution.
//!
//! The web UI's queue page and the auto-update scheduler need to describe
//! work (download, update, convert, send, mail, backup) as data before it is
//! handed to the queue. This service owns that planning logic: target
//! normalization, deduplication, and validation. It deliberately contains
//! no queue, process, or async-execution APIs — the queue layer consumes
//! [`JobPlan`] values and does the actual work.

use std::collections::HashSet;

use crate::application::error::ApplicationError;
use crate::platform::NovelId;

/// The kind of work a job performs. Mirrors the queue's `JobType` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum JobKind {
    Download,
    Update,
    Convert,
    AutoUpdate,
    Send,
    Mail,
    Backup,
}

/// A normalized job target.
///
/// Numeric ids are used directly; ncodes are kept as strings because
/// resolving an ncode to an id requires a repository lookup, which this pure
/// planning service does not perform. The queue layer resolves ncodes at
/// enqueue time. Auto-update is a targetless job and uses [`JobTarget::All`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JobTarget {
    Id(NovelId),
    Ncode(String),
    All,
}

impl JobTarget {
    /// The canonical string form used for deduplication and display.
    pub fn as_str(&self) -> String {
        match self {
            Self::Id(id) => id.0.to_string(),
            Self::Ncode(ncode) => ncode.clone(),
            Self::All => "*".to_string(),
        }
    }
}

/// A request to plan one or more jobs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JobRequest {
    pub kind: JobKind,
    /// Target novels. Each target is either a numeric id or an ncode
    /// (`n1234ab`); mixed forms are normalized and deduplicated. Auto-update
    /// accepts an empty list because it operates on the whole library.
    pub targets: Vec<String>,
    /// Extra kind-specific options (e.g. `--force` for update).
    pub options: Vec<String>,
}

/// A planned job: one kind, one normalized target, and its options.
///
/// This is the unit the queue layer enqueues; it is deterministic — the same
/// request always produces the same plans in the same order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobPlan {
    pub kind: JobKind,
    pub target: JobTarget,
    pub options: Vec<String>,
}

/// The result of planning a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobPlanResult {
    /// The deduplicated, normalized plans in input order.
    pub plans: Vec<JobPlan>,
    /// Targets that are neither a valid id nor a valid ncode.
    pub invalid: Vec<String>,
    /// Duplicate targets that were dropped (kept for reporting).
    pub duplicates: Vec<String>,
}

/// Concrete job planning service.
///
/// Pure and deterministic: no clock, no repository, no queue. Target
/// normalization accepts numeric ids and ncode strings; anything else is
/// reported as invalid.
pub struct JobService;
impl JobService {
    /// Plan a request into deduplicated [`JobPlan`]s.
    pub fn plan(&self, request: &JobRequest) -> JobPlanResult {
        if request.kind == JobKind::AutoUpdate && request.targets.is_empty() {
            return JobPlanResult {
                plans: vec![JobPlan {
                    kind: request.kind,
                    target: JobTarget::All,
                    options: request.options.clone(),
                }],
                invalid: Vec::new(),
                duplicates: Vec::new(),
            };
        }

        let mut plans = Vec::new();
        let mut invalid = Vec::new();
        let mut duplicates = Vec::new();
        let mut seen: HashSet<JobTarget> = HashSet::new();

        for raw in &request.targets {
            let target = raw.trim();
            if target.is_empty() {
                continue;
            }
            match normalize_target(target) {
                Some(normalized) => {
                    if seen.insert(normalized.clone()) {
                        plans.push(JobPlan {
                            kind: request.kind,
                            target: normalized,
                            options: request.options.clone(),
                        });
                    } else {
                        duplicates.push(target.to_string());
                    }
                }
                None => invalid.push(target.to_string()),
            }
        }

        JobPlanResult {
            plans,
            invalid,
            duplicates,
        }
    }

    /// Validate a single target string without planning.
    pub fn validate_target(&self, target: &str) -> Result<JobTarget, ApplicationError> {
        normalize_target(target).ok_or_else(|| {
            ApplicationError::InvalidRequest(format!("invalid job target: {target:?}"))
        })
    }
}

/// Normalize a target string to a [`JobTarget`].
///
/// Accepts a plain numeric id (`123`) or an ncode (`n1234ab`, case
/// insensitive, normalized to lowercase). Returns `None` for anything else.
fn normalize_target(target: &str) -> Option<JobTarget> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(id) = trimmed.parse::<i64>() {
        return Some(JobTarget::Id(NovelId(id)));
    }
    let lower = trimmed.to_ascii_lowercase();
    if is_valid_ncode(&lower) {
        return Some(JobTarget::Ncode(lower));
    }
    None
}

/// True for a well-formed ncode: `n` followed by 4–8 alphanumerics
/// (narou ncodes are `n` + 4–8 chars).
fn is_valid_ncode(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('n') else {
        return false;
    };
    (4..=8).contains(&rest.len()) && rest.chars().all(|ch| ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> JobService {
        JobService
    }

    #[test]
    fn plan_deduplicates_and_normalizes_ids() {
        let result = service().plan(&JobRequest {
            kind: JobKind::Update,
            targets: vec!["3".into(), "1".into(), "3".into(), " 2 ".into()],
            options: vec!["--force".into()],
        });
        assert_eq!(
            result.plans,
            vec![
                JobPlan {
                    kind: JobKind::Update,
                    target: JobTarget::Id(NovelId(3)),
                    options: vec!["--force".into()],
                },
                JobPlan {
                    kind: JobKind::Update,
                    target: JobTarget::Id(NovelId(1)),
                    options: vec!["--force".into()],
                },
                JobPlan {
                    kind: JobKind::Update,
                    target: JobTarget::Id(NovelId(2)),
                    options: vec!["--force".into()],
                },
            ]
        );
        assert_eq!(result.duplicates, vec!["3".to_string()]);
        assert!(result.invalid.is_empty());
    }

    #[test]
    fn plan_normalizes_ncodes_and_deduplicates_case_insensitively() {
        let result = service().plan(&JobRequest {
            kind: JobKind::Download,
            targets: vec!["N1234AB".into(), "n1234ab".into(), "n1234ab".into()],
            options: Vec::new(),
        });
        assert_eq!(
            result.plans,
            vec![JobPlan {
                kind: JobKind::Download,
                target: JobTarget::Ncode("n1234ab".into()),
                options: Vec::new(),
            }]
        );
        // Both later occurrences are duplicates (reported per occurrence).
        assert_eq!(result.duplicates, vec!["n1234ab".to_string(), "n1234ab".to_string()]);
    }

    #[test]
    fn plan_reports_invalid_targets() {
        let result = service().plan(&JobRequest {
            kind: JobKind::Download,
            targets: vec!["1".into(), "not-a-target".into(), "".into()],
            options: Vec::new(),
        });
        assert_eq!(result.plans.len(), 1);
        assert_eq!(result.invalid, vec!["not-a-target".to_string()]);
    }

    #[test]
    fn validate_target_accepts_ids_and_ncodes() {
        assert_eq!(
            service().validate_target("42").unwrap(),
            JobTarget::Id(NovelId(42))
        );
        assert_eq!(
            service().validate_target("N1234AB").unwrap(),
            JobTarget::Ncode("n1234ab".into())
        );
        assert!(service().validate_target("").is_err());
        assert!(service().validate_target("n12").is_err());
    }

    #[test]
    fn plan_allows_targetless_auto_update() {
        let result = service().plan(&JobRequest {
            kind: JobKind::AutoUpdate,
            targets: Vec::new(),
            options: Vec::new(),
        });
        assert_eq!(
            result.plans,
            vec![JobPlan {
                kind: JobKind::AutoUpdate,
                target: JobTarget::All,
                options: Vec::new(),
            }]
        );
        assert!(result.invalid.is_empty());
        assert!(result.duplicates.is_empty());
    }

    #[test]
    fn empty_request_plans_nothing() {
        let result = service().plan(&JobRequest {
            kind: JobKind::Backup,
            targets: Vec::new(),
            options: Vec::new(),
        });
        assert!(result.plans.is_empty());
        assert!(result.invalid.is_empty());
        assert!(result.duplicates.is_empty());
    }
}
