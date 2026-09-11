//! Query construction, ported verbatim from `worker_entry/src/d1_repository.rs`
//! (`select_sql` / `build_where` / `term_expression` / sort helpers /
//! `UPSERT_SQL`). The status expressions and the UPSERT live in sibling SQL
//! fragment files so the long literals stay byte-identical to the D1 source.

use crate::platform::{NovelFilter, NovelSortKey, SearchField};

pub(crate) const UPSERT_SQL: &str = include_str!("sql/upsert.sql");
pub(crate) const STATUS_SEARCH_EXPRESSION: &str = include_str!("sql/status_search_expression.sql");
pub(crate) const STATUS_SORT_EXPRESSION: &str = include_str!("sql/status_sort_expression.sql");

pub(crate) fn select_sql() -> &'static str {
    "SELECT n.id, n.author, n.author_fold, n.title, n.file_title, n.toc_url, n.sitename, n.novel_type, n.end, n.last_update, n.new_arrivals_date, n.use_subdirectory, n.general_firstup, n.novelupdated_at, n.general_lastup, n.last_mail_date, n.tags_json, n.ncode, n.domain, n.general_all_no, n.length, n.suspend, n.is_narou, n.last_check_date, n.convert_failure, n.extra_fields_yaml FROM novels n"
}

pub(crate) struct WhereBuilder {
    pub sql: String,
    /// Bind values as `rusqlite::types::Value`; positions match the `?`
    /// placeholders appended to [`WhereBuilder::sql`] in order.
    pub params: Vec<rusqlite::types::Value>,
}

fn push_param(builder: &mut WhereBuilder, value: impl Into<String>) {
    builder.params.push(rusqlite::types::Value::Text(value.into()));
}

fn push_json_param<T: serde::Serialize>(builder: &mut WhereBuilder, value: &T) {
    let json = serde_json::to_string(value).expect("serializing filter values cannot fail");
    builder.params.push(rusqlite::types::Value::Text(json));
}

pub(crate) fn build_where(filter: &NovelFilter) -> WhereBuilder {
    let mut builder = WhereBuilder {
        sql: "WHERE 1=1".to_string(),
        params: Vec::new(),
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
            builder.params.push(rusqlite::types::Value::Text(json));
        }
    }
    if let Some(keyword) = &filter.keyword {
        let keyword = crate::native::sqlite::record_map::fold(keyword);
        builder.sql.push_str(
            " AND EXISTS (SELECT 1 FROM json_each(?) WHERE instr(n.title_fold, value) > 0 OR instr(n.author_fold, value) > 0)",
        );
        push_json_param(&mut builder, &[keyword]);
    }
    if let Some(site) = &filter.site {
        builder.sql.push_str(" AND n.sitename_fold = ?");
        push_param(&mut builder, crate::native::sqlite::record_map::fold(site));
    }
    if let Some(domain) = &filter.domain {
        builder.sql.push_str(" AND n.domain_fold = ?");
        push_param(&mut builder, crate::native::sqlite::record_map::fold(domain));
    }
    if let Some(tag) = &filter.tag {
        builder.sql.push_str(" AND EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = n.id AND t.tag = ?)");
        push_param(&mut builder, tag.clone());
    }
    if let Some(ncode) = &filter.ncode {
        builder.sql.push_str(" AND n.ncode_fold = ?");
        push_param(&mut builder, crate::native::sqlite::record_map::fold(ncode));
    }
    if let Some(is_narou) = filter.is_narou {
        builder.sql.push_str(" AND n.is_narou = ?");
        builder
            .params
            .push(rusqlite::types::Value::Integer(is_narou as i64));
    }
    if let Some(suspend) = filter.suspend {
        builder.sql.push_str(" AND n.suspend = ?");
        builder
            .params
            .push(rusqlite::types::Value::Integer(suspend as i64));
    }
    if let Some(novel_type) = filter.novel_type {
        builder.sql.push_str(" AND n.novel_type = ?");
        builder
            .params
            .push(rusqlite::types::Value::Integer(i64::from(novel_type)));
    }
    if let Some(end) = filter.end {
        builder.sql.push_str(" AND n.end = ?");
        builder
            .params
            .push(rusqlite::types::Value::Integer(end as i64));
    }
    for term in &filter.terms {
        if term.values.is_empty() {
            builder.sql.push_str(if term.negated { " AND 1=1" } else { " AND 0=1" });
            continue;
        }
        let values = term
            .values
            .iter()
            .map(|value| crate::native::sqlite::record_map::fold(value))
            .collect::<Vec<_>>();
        builder.sql.push_str(if term.negated {
            " AND NOT EXISTS (SELECT 1 FROM json_each(?) WHERE "
        } else {
            " AND EXISTS (SELECT 1 FROM json_each(?) WHERE "
        });
        builder.sql.push_str(&term_expression(term.field));
        builder.sql.push(')');
        push_json_param(&mut builder, &values);
    }
    builder
}

fn term_expression(field: SearchField) -> String {
    match field {
        SearchField::Title => "instr(n.title_fold, value) > 0".to_string(),
        SearchField::Author => "instr(n.author_fold, value) > 0".to_string(),
        SearchField::Site => "instr(n.sitename_fold, value) > 0".to_string(),
        SearchField::Tag => "instr(n.tags_fold, value) > 0".to_string(),
        SearchField::Status => format!("instr({STATUS_SEARCH_EXPRESSION}, value) > 0"),
        SearchField::Any => format!(
            "instr(n.title_fold, value) > 0 OR instr(n.author_fold, value) > 0 OR instr(n.sitename_fold, value) > 0 OR instr(n.tags_fold, value) > 0 OR instr({STATUS_SEARCH_EXPRESSION}, value) > 0"
        ),
    }
}

/// SQL NULL ordering for a sort key, mirroring the native comparators:
/// `new_arrivals_date` uses `Option::cmp` (None < Some), every other optional
/// key uses `compare_optional` (Some < None).
pub(crate) fn sort_direction(key: NovelSortKey, reverse: bool) -> &'static str {
    match (key, reverse) {
        (NovelSortKey::NewArrivalsDate, false) => "ASC NULLS FIRST",
        (NovelSortKey::NewArrivalsDate, true) => "DESC NULLS LAST",
        (_, false) => "ASC NULLS LAST",
        (_, true) => "DESC NULLS FIRST",
    }
}

pub(crate) fn sort_expression(key: NovelSortKey) -> &'static str {
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
    }
}
