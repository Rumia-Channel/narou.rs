//! Portable SQL for novel queries, shared by the native (rusqlite) and Worker
//! (D1) storage adapters.
//!
//! Everything the two adapters must agree on lives here: the WHERE clauses,
//! the `json_each(?)` search terms, sort direction with its `n.id` tie-break,
//! the lookup/count/keyset statements, the materialized status expressions and
//! the batch step SQL. Each adapter supplies one conversion from the shared
//! [`NovelBind`] to its driver value, so the native side never builds an
//! intermediate bind vector; connections, transactions/batches, row structs
//! and the per-column NULL conventions (e.g. `login_session`) stay
//! adapter-local.

use crate::db::novel_codec::{fold, NovelBind, NOVEL_SELECT_SQL};
use crate::platform::{NovelFilter, NovelId, NovelQuery, NovelSort, NovelSortKey, SearchField};

/// Status text appended to `Status`/`Any` search terms. `frozen_novels` is a
/// projection of the freeze inventory on native and the freeze store itself on
/// the Worker, so both runtimes answer the same `凍結` searches.
pub const STATUS_SEARCH_EXPRESSION: &str = include_str!("sql/status_search_expression.sql");

/// Materialized `status_sort` value (完結 / 削除 / 中断). Deliberately without
/// 凍結: unfreezing must not reorder a list sorted by status.
pub const STATUS_SORT_EXPRESSION: &str = include_str!("sql/status_sort_expression.sql");

/// Delete a novel's tag rows before rewriting them (`apply_batch` upsert).
pub const DELETE_NOVEL_TAGS_SQL: &str = "DELETE FROM novel_tags WHERE novel_id = ?";

/// One indexed tag row, in record tag order.
pub const INSERT_NOVEL_TAG_SQL: &str =
    "INSERT INTO novel_tags (novel_id, position, tag, tag_fold) VALUES (?, ?, ?, ?)";

/// Remove a novel; the tag/freeze rows cascade from the schema.
pub const DELETE_NOVEL_SQL: &str = "DELETE FROM novels WHERE id = ?";

/// Extend the id sequence so an upserted record can never be handed out again.
pub const BUMP_SEQUENCE_SQL: &str =
    "UPDATE novel_id_sequence SET next_id = CASE WHEN next_id < ? THEN ? ELSE next_id END WHERE id = 1";

/// Claim a released ID, or a fresh high-water ID, inside a transaction/batch.
/// A claim receives the largest pool generation so the final SELECT can read
/// it back without relying on driver-specific INSERT result handling.
pub const ALLOCATE_STEP_SQL: &str = r#"
INSERT INTO novel_id_reservations (id, generation, available)
SELECT COALESCE(
    (SELECT r.id FROM novel_id_reservations r
     WHERE r.available = 1 AND NOT EXISTS (SELECT 1 FROM novels n WHERE n.id = r.id)
     ORDER BY r.id LIMIT 1),
    MAX(s.next_id, COALESCE((SELECT MAX(id) FROM novels), -1) + 1,
        COALESCE((SELECT MAX(id) FROM novel_id_reservations), -1) + 1)),
    COALESCE((SELECT MAX(generation) FROM novel_id_reservations), -1) + 1, 0
FROM novel_id_sequence s WHERE s.id = 1
ON CONFLICT(id) DO UPDATE SET generation = excluded.generation, available = 0
"#;

/// Preserve the high-water mark even when released IDs are later reused.
pub const ALLOCATE_BUMP_SQL: &str =
    "UPDATE novel_id_sequence SET next_id = MAX(next_id, COALESCE((SELECT MAX(id) FROM novel_id_reservations), -1) + 1) WHERE id = 1";

/// Read the claim made by this atomic batch; no other claim can interleave.
pub const ALLOCATE_READ_SQL: &str =
    "SELECT id, generation FROM novel_id_reservations WHERE EXISTS (SELECT 1 FROM novel_id_sequence WHERE id = 1) ORDER BY generation DESC LIMIT 1";

/// Stale releases and already-published records cannot reenter the pool.
pub const RELEASE_ID_SQL: &str =
    "UPDATE novel_id_reservations SET available = 1 WHERE id = ? AND generation = ? AND available = 0 AND NOT EXISTS (SELECT 1 FROM novels WHERE novels.id = novel_id_reservations.id)";

/// Retire ownership in the same transaction that publishes a record.
pub const COMMIT_ID_RESERVATION_SQL: &str =
    "DELETE FROM novel_id_reservations WHERE id = ?";

/// Rewrite one row's materialized status key after its tags changed.
pub fn refresh_status_sort_sql() -> String {
    format!("UPDATE novels AS n SET status_sort = {STATUS_SORT_EXPRESSION} WHERE n.id = ?")
}

/// The shared `novels` projection every lookup starts from.
pub fn select_sql() -> &'static str {
    NOVEL_SELECT_SQL
}

/// A clause (`WHERE …` / `ORDER BY …`) plus its binds in placeholder order.
pub struct WhereSql<B> {
    pub sql: String,
    pub binds: Vec<B>,
}

/// A complete statement plus its binds in placeholder order.
pub struct SqlQuery<B> {
    pub sql: String,
    pub binds: Vec<B>,
}

/// `get`: one row by primary key.
pub fn select_by_id_sql() -> String {
    format!("{} WHERE n.id = ? LIMIT 1", select_sql())
}

/// `find_by_toc_url`: folded toc_url, lowest id wins.
pub fn select_by_toc_url_sql() -> String {
    format!("{} WHERE n.toc_url_fold = ? ORDER BY n.id LIMIT 1", select_sql())
}

/// `find_by_title`: folded title, lowest id wins.
pub fn select_by_title_sql() -> String {
    format!("{} WHERE n.title_fold = ? ORDER BY n.id LIMIT 1", select_sql())
}

/// `find_by_ncode`: folded ncode, or the trailing `/`-separated URL segment
/// (records registered before the `ncode` column existed). Binds the folded
/// ncode three times.
pub fn select_by_ncode_sql() -> String {
    format!(
        "{} WHERE n.ncode_fold = ? OR substr(rtrim(n.toc_url_fold, '/'), -(length(?) + 1)) = '/' || ? ORDER BY n.id LIMIT 1",
        select_sql()
    )
}

/// `WHERE 1=1 …` for `filter`. `bind` converts each placeholder value to the
/// driver's type, in order; it is called once per placeholder.
pub fn build_where<B>(filter: &NovelFilter, mut bind: impl FnMut(NovelBind) -> B) -> WhereSql<B> {
    let mut builder = WhereSql {
        sql: "WHERE 1=1".to_string(),
        binds: Vec::new(),
    };
    if let Some(ids) = &filter.ids {
        if ids.is_empty() {
            // An explicitly empty id set is "match nothing", not "no filter".
            builder.sql.push_str(" AND 0=1");
        } else {
            let json = serde_json::to_string(&ids.iter().map(|id| id.0).collect::<Vec<_>>())
                .expect("serializing integer ids cannot fail");
            builder
                .sql
                .push_str(" AND n.id IN (SELECT CAST(value AS INTEGER) FROM json_each(?))");
            builder.binds.push(bind(NovelBind::Text(json)));
        }
    }
    if let Some(keyword) = &filter.keyword {
        let json = term_values_json(&[fold(keyword)]);
        builder.sql.push_str(
            " AND EXISTS (SELECT 1 FROM json_each(?) WHERE instr(n.title_fold, value) > 0 OR instr(n.author_fold, value) > 0)",
        );
        builder.binds.push(bind(NovelBind::Text(json)));
    }
    if let Some(site) = &filter.site {
        builder.sql.push_str(" AND n.sitename_fold = ?");
        builder.binds.push(bind(NovelBind::Text(fold(site))));
    }
    if let Some(domain) = &filter.domain {
        builder.sql.push_str(" AND n.domain_fold = ?");
        builder.binds.push(bind(NovelBind::Text(fold(domain))));
    }
    if let Some(tag) = &filter.tag {
        if filter.attribute_tags_enabled {
            builder.sql.push_str(" AND (EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = ?) OR n.attribute_tag = ?)");
            builder.binds.push(bind(NovelBind::Text(tag.clone())));
        } else {
            builder.sql.push_str(" AND EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = ?)");
        }
        builder.binds.push(bind(NovelBind::Text(tag.clone())));
    }
    if let Some(ncode) = &filter.ncode {
        builder.sql.push_str(" AND n.ncode_fold = ?");
        builder.binds.push(bind(NovelBind::Text(fold(ncode))));
    }
    if let Some(is_narou) = filter.is_narou {
        builder.sql.push_str(" AND n.is_narou = ?");
        builder.binds.push(bind(NovelBind::Int(is_narou as i64)));
    }
    if let Some(suspend) = filter.suspend {
        builder.sql.push_str(" AND n.suspend = ?");
        builder.binds.push(bind(NovelBind::Int(suspend as i64)));
    }
    if let Some(novel_type) = filter.novel_type {
        builder.sql.push_str(" AND n.novel_type = ?");
        builder.binds.push(bind(NovelBind::Int(i64::from(novel_type))));
    }
    if let Some(end) = filter.end {
        builder.sql.push_str(" AND n.end = ?");
        builder.binds.push(bind(NovelBind::Int(end as i64)));
    }
    for term in &filter.terms {
        if term.values.is_empty() {
            builder
                .sql
                .push_str(if term.negated { " AND 1=1" } else { " AND 0=1" });
            continue;
        }
        let values = term
            .values
            .iter()
            .map(|value| fold(value))
            .collect::<Vec<_>>();
        let json = term_values_json(&values);
        builder.sql.push_str(if term.negated {
            " AND NOT EXISTS (SELECT 1 FROM json_each(?) WHERE "
        } else {
            " AND EXISTS (SELECT 1 FROM json_each(?) WHERE "
        });
        builder.sql.push_str(&term_expression(term.field));
        if filter.attribute_tags_enabled && matches!(term.field, SearchField::Tag | SearchField::Any) {
            builder.sql.push_str(" OR instr(COALESCE(n.attribute_tag, ''), value) > 0");
        }
        builder.sql.push(')');
        builder.binds.push(bind(NovelBind::Text(json)));
    }
    if !filter.exclude_ids.is_empty() {
        let json = serde_json::to_string(&filter.exclude_ids)
            .expect("serializing integer ids cannot fail");
        builder.sql.push_str(
            " AND n.id NOT IN (SELECT CAST(value AS INTEGER) FROM json_each(?))",
        );
        builder.binds.push(bind(NovelBind::Text(json)));
    }
    if !filter.exclude_tags.is_empty() {
        builder.sql.push_str(
            " AND NOT EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag IN (SELECT value FROM json_each(?)))",
        );
        builder
            .binds
            .push(bind(NovelBind::Text(term_values_json(&filter.exclude_tags))));
        if filter.attribute_tags_enabled {
            builder.sql.push_str(" AND COALESCE(n.attribute_tag, '') NOT IN (SELECT value FROM json_each(?))");
            builder.binds.push(bind(NovelBind::Text(term_values_json(&filter.exclude_tags))));
        }
    }
    if !filter.exclude_authors.is_empty() {
        builder
            .sql
            .push_str(" AND n.author_fold NOT IN (SELECT value FROM json_each(?))");
        builder.binds.push(bind(NovelBind::Text(term_values_json(
            &filter.exclude_authors,
        ))));
    }
    if !filter.exclude_sites.is_empty() {
        builder.sql.push_str(
            " AND n.sitename_fold NOT IN (SELECT value FROM json_each(?)) \
             AND COALESCE(n.domain_fold, '') NOT IN (SELECT value FROM json_each(?))",
        );
        let json = term_values_json(&filter.exclude_sites);
        builder.binds.push(bind(NovelBind::Text(json.clone())));
        builder.binds.push(bind(NovelBind::Text(json)));
    }
    builder
}

/// `SELECT COUNT(*)` for `filter`. `count` is aliased for the Worker's serde
/// row struct; the native adapter reads the single column by position.
pub fn count_sql<B>(filter: &NovelFilter, bind: impl FnMut(NovelBind) -> B) -> SqlQuery<B> {
    let WhereSql { sql, binds } = build_where(filter, bind);
    SqlQuery {
        sql: format!("SELECT COUNT(*) AS count FROM novels n {sql}"),
        binds,
    }
}

/// Paginated `SELECT` for `query`. The limit/offset binds come last, after the
/// filter binds, because they are appended after the WHERE clause.
pub fn query_sql<B>(query: &NovelQuery, mut bind: impl FnMut(NovelBind) -> B) -> SqlQuery<B> {
    let WhereSql { mut sql, mut binds } = build_where(&query.filter, &mut bind);
    sql.push(' ');
    sql.push_str(&order_by_sql(query.sort));
    sql.push_str(" LIMIT ? OFFSET ?");
    binds.push(bind(NovelBind::Int(query.limit as i64)));
    binds.push(bind(NovelBind::Int(query.offset as i64)));
    SqlQuery {
        sql: format!("{} {}", select_sql(), sql),
        binds,
    }
}

/// Keyset scan over the matching ids, ascending. `after_id` continues after a
/// previously returned id (`None` starts at the beginning).
pub fn scan_ids_sql<B>(
    filter: &NovelFilter,
    after_id: Option<NovelId>,
    limit: usize,
    mut bind: impl FnMut(NovelBind) -> B,
) -> SqlQuery<B> {
    let WhereSql { mut sql, mut binds } = build_where(filter, &mut bind);
    if let Some(after_id) = after_id {
        sql.push_str(" AND n.id > ?");
        binds.push(bind(NovelBind::Int(after_id.0)));
    }
    sql.push_str(" ORDER BY n.id ASC LIMIT ?");
    binds.push(bind(NovelBind::Int(limit as i64)));
    SqlQuery {
        sql: format!("SELECT n.id FROM novels n {sql}"),
        binds,
    }
}

/// `ORDER BY …` including the `n.id` tie-break, so equally-keyed rows have one
/// order on every backend.
pub fn order_by_sql(sort: NovelSort) -> String {
    format!(
        "ORDER BY {} {}{}",
        sort_expression(sort.key),
        sort_direction(sort.key, sort.reverse),
        tie_break(sort.reverse)
    )
}

fn tie_break(reverse: bool) -> &'static str {
    if reverse { ", n.id DESC" } else { ", n.id ASC" }
}

fn term_expression(field: SearchField) -> String {
    match field {
        SearchField::Title => "instr(n.title_fold, value) > 0".to_string(),
        SearchField::Author => "instr(n.author_fold, value) > 0".to_string(),
        SearchField::Site => "instr(n.sitename_fold, value) > 0".to_string(),
        SearchField::Tag => "instr(n.tags_fold, value) > 0".to_string(),
        SearchField::Status => format!("instr({STATUS_SEARCH_EXPRESSION}, value) > 0"),
        SearchField::Any => format!(
            "instr(n.title_fold, value) > 0 OR instr(n.author_fold, value) > 0 OR instr(n.sitename_fold, value) > 0 OR instr(n.tags_fold, value) > 0 OR instr({STATUS_SEARCH_EXPRESSION}, value) > 0 OR instr(n.ncode_fold, value) > 0 OR (instr(value, '://') > 0 AND instr(n.toc_url_fold, rtrim(value, '/')) > 0) OR substr(rtrim(n.toc_url_fold, '/'), -(length(value) + 1)) = '/' || value OR CAST(n.id AS TEXT) = value"
        ),
    }
}

/// Folded term values as the JSON array the `json_each(?)` clauses match
/// against.
fn term_values_json(values: &[String]) -> String {
    serde_json::to_string(values).expect("serializing folded term values cannot fail")
}

/// SQL NULL ordering for a sort key, mirroring the native comparators:
/// `new_arrivals_date` uses `Option::cmp` (None < Some), every other optional
/// key uses `compare_optional` (Some < None).
pub fn sort_direction(key: NovelSortKey, reverse: bool) -> &'static str {
    match (key, reverse) {
        (NovelSortKey::NewArrivalsDate, false) => "ASC NULLS FIRST",
        (NovelSortKey::NewArrivalsDate, true) => "DESC NULLS LAST",
        (_, false) => "ASC NULLS LAST",
        (_, true) => "DESC NULLS FIRST",
    }
}

pub fn sort_expression(key: NovelSortKey) -> &'static str {
    match key {
        NovelSortKey::Id => "n.id",
        NovelSortKey::LastUpdate => "n.last_update",
        NovelSortKey::GeneralLastup => "n.general_lastup",
        NovelSortKey::LastCheckDate => "n.last_check_date",
        NovelSortKey::Title => "n.title_fold",
        NovelSortKey::Author => "n.author_fold",
        NovelSortKey::SiteName => "n.sitename_fold",
        NovelSortKey::NovelType => "n.novel_type",
        NovelSortKey::Tags => "n.tags_sort",
        NovelSortKey::GeneralAllNo => "n.general_all_no",
        NovelSortKey::Length => "n.length",
        NovelSortKey::Status => "n.status_sort",
        NovelSortKey::TocUrl => "n.toc_url_fold",
        NovelSortKey::NewArrivalsDate => "n.new_arrivals_date",
        NovelSortKey::AverageLength => "CASE WHEN n.length > 0 AND n.general_all_no > 0 THEN n.length / n.general_all_no ELSE 0 END",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::SearchTerm;

    fn filter_with_every_clause() -> NovelFilter {
        NovelFilter {
            ids: Some(vec![NovelId(1), NovelId(2)]),
            keyword: Some("Keyword".to_string()),
            site: Some(" Site ".to_string()),
            domain: Some("Domain".to_string()),
            tag: Some("tag".to_string()),
            ncode: Some("NCODE".to_string()),
            is_narou: Some(true),
            suspend: Some(false),
            novel_type: Some(1),
            end: Some(true),
            terms: vec![
                SearchTerm::new(SearchField::Any, false, vec![" a ".to_string()]),
                SearchTerm::new(SearchField::Title, true, vec!["b".to_string()]),
            ],
            frozen_ids: None,
            ..NovelFilter::default()
        }
    }

    #[test]
    fn every_statement_binds_one_value_per_placeholder() {
        let filter = filter_with_every_clause();
        let where_sql = build_where(&filter, |bind| bind);
        assert_eq!(where_sql.sql.matches('?').count(), where_sql.binds.len());

        let count = count_sql(&filter, |bind| bind);
        assert!(count.sql.starts_with("SELECT COUNT(*) AS count FROM novels n WHERE 1=1"));
        assert_eq!(count.sql.matches('?').count(), count.binds.len());

        let scanned = scan_ids_sql(&filter, Some(NovelId(9)), 50, |bind| bind);
        assert!(scanned.sql.starts_with("SELECT n.id FROM novels n WHERE 1=1"));
        assert!(scanned.sql.ends_with("ORDER BY n.id ASC LIMIT ?"));
        assert_eq!(scanned.sql.matches('?').count(), scanned.binds.len());
        let tail = &scanned.binds[scanned.binds.len() - 2..];
        assert_eq!(tail, &[NovelBind::Int(9), NovelBind::Int(50)][..]);
    }

    #[test]
    fn query_sql_appends_limit_and_offset_after_the_filter_binds() {
        let query = NovelQuery {
            filter: filter_with_every_clause(),
            sort: NovelSort {
                key: NovelSortKey::Title,
                reverse: true,
            },
            offset: 5,
            limit: 20,
        };
        let page = query_sql(&query, |bind| bind);
        assert!(page.sql.starts_with(NOVEL_SELECT_SQL));
        assert_eq!(page.sql.matches('?').count(), page.binds.len());
        // 降順の tie-break は `n.id DESC`、NULL は同値キーの後ろへ。
        assert!(page.sql.ends_with("ORDER BY n.title_fold DESC NULLS FIRST, n.id DESC LIMIT ? OFFSET ?"));
        let tail = &page.binds[page.binds.len() - 2..];
        assert_eq!(tail, &[NovelBind::Int(20), NovelBind::Int(5)][..]);
    }

    #[test]
    fn scan_ids_without_a_cursor_skips_the_id_clause() {
        let filter = NovelFilter::default();
        let scanned = scan_ids_sql(&filter, None, 10, |bind| bind);
        assert!(!scanned.sql.contains("n.id > ?"));
        assert_eq!(scanned.binds, vec![NovelBind::Int(10)]);
    }

    #[test]
    fn hidden_exclusions_render_negated_clauses_and_bind_values() {
        let filter = NovelFilter {
            exclude_ids: vec![1, 2],
            exclude_tags: vec!["秘密".to_string()],
            exclude_authors: vec!["author".to_string()],
            exclude_sites: vec!["site".to_string()],
            ..NovelFilter::default()
        };
        let count = count_sql(&filter, |bind| bind);
        assert!(count.sql.contains("n.id NOT IN (SELECT CAST(value AS INTEGER) FROM json_each(?))"));
        assert!(count.sql.contains("t.tag IN (SELECT value FROM json_each(?))"));
        assert!(count.sql.contains("n.author_fold NOT IN (SELECT value FROM json_each(?))"));
        assert!(count.sql.contains("n.sitename_fold NOT IN (SELECT value FROM json_each(?))"));
        assert_eq!(count.sql.matches('?').count(), count.binds.len());
        assert!(count.binds.iter().any(|bind| matches!(bind, NovelBind::Text(json) if json == "[1,2]")));
    }

    /// The adapters differ only in the bind conversion: the statement and the
    /// bind order must not depend on it.
    #[test]
    fn the_bind_conversion_cannot_change_the_statement() {
        let query = NovelQuery {
            filter: filter_with_every_clause(),
            sort: NovelSort::default(),
            offset: 0,
            limit: 10,
        };
        let worker_like = query_sql(&query, |bind| bind);
        let driver_like = query_sql(&query, |bind| match bind {
            NovelBind::Text(text) => format!("text:{text}"),
            NovelBind::Int(int) => format!("int:{int}"),
            NovelBind::Null => "null".to_string(),
        });
        assert_eq!(worker_like.sql, driver_like.sql);
        assert_eq!(worker_like.binds.len(), driver_like.binds.len());
        // 先頭は ids の JSON 配列 — どちらの変換でも同じ 1 バインド。
        let expected_json = serde_json::to_string(&[1, 2]).unwrap();
        assert_eq!(worker_like.binds[0], NovelBind::Text(expected_json.clone()));
        assert_eq!(driver_like.binds[0], format!("text:{expected_json}"));
    }

    #[test]
    fn ncode_lookup_covers_legacy_records_without_an_ncode() {
        let sql = select_by_ncode_sql();
        assert_eq!(sql.matches('?').count(), 3);
        assert!(sql.contains("n.ncode_fold = ?"));
        assert!(sql.contains("substr(rtrim(n.toc_url_fold, '/'), -(length(?) + 1)) = '/' || ?"));
        assert!(sql.ends_with("ORDER BY n.id LIMIT 1"));
        assert!(select_by_toc_url_sql().contains("n.toc_url_fold = ? ORDER BY n.id LIMIT 1"));
        assert!(select_by_title_sql().contains("n.title_fold = ? ORDER BY n.id LIMIT 1"));
        assert!(select_by_id_sql().ends_with("WHERE n.id = ? LIMIT 1"));
    }

}
