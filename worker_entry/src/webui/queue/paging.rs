//! Cursor paging for `GET /api/get_pending_tasks` (Worker queue modal).
//!
//! The modal lists the active job queue; reading every active row into one
//! JSON body does not scale past a few hundred jobs. Paging is a
//! `(created_at, job_id)` keyset (not `OFFSET`) so rows inserted in the same
//! ledger batch — which share one `created_at` — are never skipped or
//! repeated, and rows that leave the active set while the client walks pages
//! cannot shift later pages. `next_cursor = null` always means "complete":
//! nothing is silently dropped.
//!
//! This module holds only pure helpers plus the two SQL statements, so host
//! tests can execute the production SQL and the cursor codec against SQLite.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Page size when the request omits `limit`.
pub const DEFAULT_PAGE_SIZE: usize = 200;
/// Hard cap for a client-supplied `limit`.
pub const MAX_PAGE_SIZE: usize = 1000;

/// Clamp a raw `limit` query parameter. Missing, empty, zero and unparsable
/// values fall back to [`DEFAULT_PAGE_SIZE`]; larger ones cap at
/// [`MAX_PAGE_SIZE`].
pub fn clamp_limit(raw: Option<&str>) -> usize {
    match raw.map(str::trim).and_then(|value| value.parse::<usize>().ok()) {
        Some(limit) if limit > 0 => limit.min(MAX_PAGE_SIZE),
        _ => DEFAULT_PAGE_SIZE,
    }
}

/// Opaque, URL-safe cursor for the `(created_at, job_id)` keyset.
pub fn encode_cursor(created_at: &str, job_id: &str) -> String {
    URL_SAFE_NO_PAD.encode(format!("{created_at}\n{job_id}"))
}

/// Inverse of [`encode_cursor`]. `None` for a malformed cursor; callers treat
/// that as "start from the beginning" rather than dropping rows.
pub fn decode_cursor(raw: &str) -> Option<(String, String)> {
    let decoded = URL_SAFE_NO_PAD.decode(raw.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (created_at, job_id) = text.split_once('\n')?;
    if created_at.is_empty() || job_id.is_empty() {
        return None;
    }
    Some((created_at.to_string(), job_id.to_string()))
}

/// One FIFO page of active rows.
///
/// Empty cursor strings start from the beginning (stored timestamps are
/// non-empty canonical RFC3339), so the first and later pages share one
/// statement. Binds in order: cursor `created_at` (twice), `job_id`, `limit`.
pub const ACTIVE_PAGE_SQL: &str = r#"
SELECT job_id, kind, target, options, status, created_at
FROM worker_jobs
WHERE status IN ('pending', 'retryable', 'running')
  AND (created_at > ? OR (created_at = ? AND job_id > ?))
ORDER BY created_at ASC, job_id ASC
LIMIT ?
"#;

/// Server-side totals for the modal: `lane` 0 = waiting (pending/retryable),
/// 1 = running. Counted in the database so a page never carries the whole
/// queue just to show a count.
pub const ACTIVE_TOTALS_SQL: &str = r#"
SELECT CASE WHEN status IN ('pending', 'retryable') THEN 0 ELSE 1 END AS lane,
       COUNT(*) AS n
FROM worker_jobs
WHERE status IN ('pending', 'retryable', 'running')
GROUP BY lane
"#;

#[cfg(test)]
mod tests {
    use super::{MAX_PAGE_SIZE, clamp_limit, decode_cursor, encode_cursor};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn limit_defaults_and_caps() {
        assert_eq!(clamp_limit(None), 200);
        assert_eq!(clamp_limit(Some("")), 200);
        assert_eq!(clamp_limit(Some("0")), 200);
        assert_eq!(clamp_limit(Some("abc")), 200);
        assert_eq!(clamp_limit(Some(" 25 ")), 25);
        assert_eq!(clamp_limit(Some("1000000")), MAX_PAGE_SIZE);
    }

    #[test]
    fn cursor_round_trips_and_rejects_malformed_values() {
        let created = "2026-10-12T00:00:00.000000000+00:00";
        let cursor = encode_cursor(created, "jabc");
        assert_eq!(
            decode_cursor(&cursor),
            Some((created.to_string(), "jabc".to_string()))
        );
        assert_eq!(decode_cursor("not base64!"), None);
        assert_eq!(decode_cursor(&URL_SAFE_NO_PAD.encode("no-separator")), None);
        assert_eq!(decode_cursor(&URL_SAFE_NO_PAD.encode("\njob")), None);
        assert_eq!(decode_cursor(&URL_SAFE_NO_PAD.encode("created\n")), None);
    }
}
