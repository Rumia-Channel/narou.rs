//! Execute the Worker's queue paging SQL and cursor codec against SQLite.
//!
//! `GET /api/get_pending_tasks` pages the active queue with a
//! `(created_at, job_id)` keyset. Rows written by one ledger batch share
//! `created_at`, so the tie breaker is what keeps a client from skipping or
//! repeating a job while it walks `next_cursor` to the end.
//!
//! The Worker crate itself is wasm-only, so the production module is included
//! here directly (same pattern as `tests/worker_storage_probe.rs`).

#[path = "../worker_entry/src/webui/queue/paging.rs"]
mod paging;

use paging::{ACTIVE_PAGE_SQL, ACTIVE_TOTALS_SQL};

fn connection() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE worker_jobs (
            job_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            target TEXT NOT NULL,
            options TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL
        );",
    )
    .unwrap();
    conn
}

fn insert(conn: &rusqlite::Connection, job_id: &str, status: &str, created_at: &str) {
    conn.execute(
        "INSERT INTO worker_jobs VALUES (?1, 'update', '1', '[]', ?2, ?3)",
        rusqlite::params![job_id, status, created_at],
    )
    .unwrap();
}

fn page(
    conn: &rusqlite::Connection,
    cursor: Option<(&str, &str)>,
    limit: usize,
) -> Vec<(String, String)> {
    let (created_at, job_id) = cursor.unwrap_or(("", ""));
    let mut statement = conn.prepare(ACTIVE_PAGE_SQL).unwrap();
    let rows = statement
        .query_map(
            rusqlite::params![created_at, created_at, job_id, limit as i64],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(5)?)),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
}

fn job_ids(rows: &[(String, String)]) -> Vec<String> {
    rows.iter().map(|(job_id, _)| job_id.clone()).collect()
}

fn totals(conn: &rusqlite::Connection) -> [i64; 2] {
    let mut statement = conn.prepare(ACTIVE_TOTALS_SQL).unwrap();
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut counts = [0i64; 2];
    for (lane, n) in rows {
        counts[usize::try_from(lane).expect("lane is 0 or 1")] = n;
    }
    counts
}

#[test]
fn pages_walk_every_active_job_exactly_once_with_stable_ties() {
    let conn = connection();
    let tied = "2026-10-12T00:00:00.000000000Z";
    for index in 0..25 {
        insert(&conn, &format!("batch-{index:02}"), "pending", tied);
    }
    insert(&conn, "older", "pending", "2026-10-11T00:00:00.000000000Z");
    insert(&conn, "running", "running", tied);
    insert(&conn, "terminal", "succeeded", "2026-10-10T00:00:00.000000000Z");

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<(String, String)> = None;
    for _ in 0..100 {
        let rows = page(&conn, cursor.as_ref().map(|(at, id)| (at.as_str(), id.as_str())), 7);
        if rows.is_empty() {
            break;
        }
        let full = rows.len() == 7;
        seen.extend(job_ids(&rows));
        cursor = rows.last().map(|(job_id, at)| (at.clone(), job_id.clone()));
        if !full {
            break;
        }
    }

    let mut expected = vec!["older".to_string()];
    expected.extend((0..25).map(|index| format!("batch-{index:02}")));
    expected.push("running".to_string());
    assert_eq!(seen, expected);
    let mut deduped = seen.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), seen.len(), "page boundaries must not repeat a row");

    // A cursor at the very end returns an empty page and a client stops.
    assert!(page(&conn, Some((tied, "running")), 7).is_empty());
}

#[test]
fn totals_count_waiting_and_running_in_the_database() {
    let conn = connection();
    for index in 0..3 {
        insert(&conn, &format!("pending-{index}"), "pending", "2026-10-12T00:00:00Z");
    }
    for index in 0..2 {
        insert(&conn, &format!("retryable-{index}"), "retryable", "2026-10-12T00:00:00Z");
    }
    for index in 0..4 {
        insert(&conn, &format!("running-{index}"), "running", "2026-10-12T00:00:00Z");
    }
    insert(&conn, "done", "permanent", "2026-10-12T00:00:00Z");
    assert_eq!(totals(&conn), [5, 4]);
    assert_eq!(page(&conn, None, 100).len(), 9);
}

