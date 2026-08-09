//! Novel record repository abstraction.
//!
//! The domain layer reads and writes novel records through this interface
//! instead of holding the whole database in memory (`db::DATABASE`,
//! `all_records()`). The native backend keeps the current YAML database file
//! (see `native::novel_repository`); a Worker backend uses D1.
//!
//! Design constraints (Phase 3):
//! - async interface via [`PlatformFuture`] so D1 can be implemented naturally
//! - filter/sort expressed as data ([`NovelFilter`], [`NovelSortKey`]), never
//!   as arbitrary closures — every query maps to SQL on D1
//! - no "load everything" query: paginated [`NovelQuery`] for display, ID
//!   keyset [`NovelRepository::scan_ids`] for bulk processing
//! - ID allocation is an atomic reservation ([`NovelRepository::allocate_id`]),
//!   never `max(id) + 1` caller logic
//! - batch mutations ([`NovelMutation`] / [`NovelRepository::apply_batch`]) so
//!   native saves the YAML once per batch

use std::cmp::Ordering;
use std::collections::HashSet;

use super::{PlatformFuture, PlatformService};
use crate::db::NovelRecord;

/// Novel identifier (the numeric record id used across the codebase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NovelId(pub i64);

impl From<i64> for NovelId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<NovelId> for i64 {
    fn from(id: NovelId) -> Self {
        id.0
    }
}

/// Which field a [`SearchTerm`] matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchField {
    /// Unqualified: title / author / sitename / status text / tags.
    Any,
    Title,
    Author,
    Site,
    Status,
    Tag,
}

/// One search term: a field plus OR-combined lowercase values, optionally
/// negated. Multiple terms in a filter are AND-combined (the Web UI's
/// `filter`/`search[value]` semantics). Stored as data so a D1 backend can
/// translate it to SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTerm {
    pub field: SearchField,
    pub negated: bool,
    pub values: Vec<String>,
}

impl SearchTerm {
    pub fn new(field: SearchField, negated: bool, values: Vec<String>) -> Self {
        Self {
            field,
            negated,
            values,
        }
    }
}

/// Typed filter over novel records. Every field maps to a WHERE clause on D1.
#[derive(Debug, Clone, Default)]
pub struct NovelFilter {
    pub ids: Option<Vec<NovelId>>,
    /// Free-text substring over title OR author (lowercase).
    pub keyword: Option<String>,
    pub site: Option<String>,
    pub domain: Option<String>,
    /// Any of the record's tags equals this tag.
    pub tag: Option<String>,
    pub ncode: Option<String>,
    pub is_narou: Option<bool>,
    pub suspend: Option<bool>,
    pub novel_type: Option<u8>,
    pub end: Option<bool>,
    /// AND-combined free-text/fielded search terms (Web UI list).
    pub terms: Vec<SearchTerm>,
    /// Native hint: ids frozen via the freeze inventory, used only for
    /// `Status`/`Any` term matching. D1 backends ignore this and join their
    /// freeze table instead.
    pub frozen_ids: Option<HashSet<i64>>,
}

impl NovelFilter {
    pub fn all() -> Self {
        Self::default()
    }
}

/// Typed sort key. CLI/Web string keys are converted at the boundary via
/// [`NovelSortKey::from_db_key`]; the key never flows into the repository as
/// an arbitrary string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NovelSortKey {
    Id,
    LastUpdate,
    GeneralLastup,
    LastCheckDate,
    Title,
    Author,
    SiteName,
    NovelType,
    Tags,
    GeneralAllNo,
    Length,
    Status,
    TocUrl,
    NewArrivalsDate,
}

impl NovelSortKey {
    pub const ALL: &'static [NovelSortKey] = &[
        NovelSortKey::Id,
        NovelSortKey::LastUpdate,
        NovelSortKey::GeneralLastup,
        NovelSortKey::LastCheckDate,
        NovelSortKey::Title,
        NovelSortKey::Author,
        NovelSortKey::SiteName,
        NovelSortKey::NovelType,
        NovelSortKey::Tags,
        NovelSortKey::GeneralAllNo,
        NovelSortKey::Length,
        NovelSortKey::Status,
        NovelSortKey::TocUrl,
        NovelSortKey::NewArrivalsDate,
    ];

    /// Map to the `db::SORT_KEYS` string so the canonical comparator
    /// (`db::compare_records_by_key`, BUG-9 None-stable order) is shared
    /// between the repository and the existing CLI/Web sort paths.
    pub fn as_db_key(self) -> &'static str {
        match self {
            NovelSortKey::Id => "id",
            NovelSortKey::LastUpdate => "last_update",
            NovelSortKey::GeneralLastup => "general_lastup",
            NovelSortKey::LastCheckDate => "last_check_date",
            NovelSortKey::Title => "title",
            NovelSortKey::Author => "author",
            NovelSortKey::SiteName => "sitename",
            NovelSortKey::NovelType => "novel_type",
            NovelSortKey::Tags => "tags",
            NovelSortKey::GeneralAllNo => "general_all_no",
            NovelSortKey::Length => "length",
            NovelSortKey::Status => "status",
            NovelSortKey::TocUrl => "toc_url",
            NovelSortKey::NewArrivalsDate => "new_arrivals_date",
        }
    }

    pub fn from_db_key(key: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.as_db_key() == key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NovelSort {
    pub key: NovelSortKey,
    pub reverse: bool,
}

impl Default for NovelSort {
    fn default() -> Self {
        Self {
            key: NovelSortKey::Id,
            reverse: false,
        }
    }
}

impl NovelSort {
    pub fn by(key: NovelSortKey) -> Self {
        Self {
            key,
            reverse: false,
        }
    }
}

/// Paginated, sorted query. `limit` must be non-zero; use
/// [`NovelRepository::scan_ids`] for unbounded bulk processing.
#[derive(Debug, Clone)]
pub struct NovelQuery {
    pub filter: NovelFilter,
    pub sort: NovelSort,
    pub offset: usize,
    pub limit: usize,
}

impl NovelQuery {
    pub fn page(filter: NovelFilter, sort: NovelSort, offset: usize, limit: usize) -> Self {
        Self {
            filter,
            sort,
            offset,
            limit,
        }
    }
}

/// A single record mutation; several are applied atomically by
/// [`NovelRepository::apply_batch`] (native: one YAML save; D1: one
/// transaction/batch).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum NovelMutation {
    Upsert(NovelRecord),
    Remove(NovelId),
}

/// CRUD + query access to novel records.
///
/// Implementations decide persistence and indexing. Full-table scans are an
/// implementation detail; the D1 backend must paginate/filter at the SQL
/// level and never `SELECT` the whole table.
pub trait NovelRepository: PlatformService {
    fn get<'a>(
        &'a self,
        id: NovelId,
    ) -> PlatformFuture<'a, crate::error::Result<Option<NovelRecord>>>;

    fn find_by_toc_url<'a>(
        &'a self,
        url: &'a str,
    ) -> PlatformFuture<'a, crate::error::Result<Option<NovelRecord>>>;

    fn find_by_title<'a>(
        &'a self,
        title: &'a str,
    ) -> PlatformFuture<'a, crate::error::Result<Option<NovelRecord>>>;

    /// Resolve a record by ncode, including the legacy `toc_url` suffix
    /// fallback (records without an `ncode` field). D1: `WHERE ncode = ? OR
    /// lower(toc_url) LIKE '%/' || ?`.
    fn find_by_ncode<'a>(
        &'a self,
        ncode: &'a str,
    ) -> PlatformFuture<'a, crate::error::Result<Option<NovelRecord>>>;

    fn count<'a>(
        &'a self,
        filter: &'a NovelFilter,
    ) -> PlatformFuture<'a, crate::error::Result<u64>>;

    /// Paginated sorted query for display (Web UI list, CLI list).
    fn query<'a>(
        &'a self,
        query: &'a NovelQuery,
    ) -> PlatformFuture<'a, crate::error::Result<Vec<NovelRecord>>>;

    /// Bulk-processing ID scan in ascending ID order:
    /// `WHERE id > after_id [AND filter] ORDER BY id LIMIT limit`.
    /// D1 uses keyset pagination (no OFFSET) for background bulk jobs.
    fn scan_ids<'a>(
        &'a self,
        filter: &'a NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> PlatformFuture<'a, crate::error::Result<Vec<NovelId>>>;

    /// Atomically reserve a unique novel id. Native keeps the existing
    /// `max + 1` behaviour under the database lock; D1 uses a sequence
    /// table / atomic SQL. Gaps in the sequence are fine.
    fn allocate_id(&self) -> PlatformFuture<'_, crate::error::Result<NovelId>>;

    /// Apply several mutations atomically and persist once.
    fn apply_batch<'a>(
        &'a self,
        mutations: Vec<NovelMutation>,
    ) -> PlatformFuture<'a, crate::error::Result<()>>;
}

// ---------------------------------------------------------------------------
// Pure matching / sorting helpers shared by native and memory backends.
// ---------------------------------------------------------------------------

/// True when a record is frozen: either in the freeze inventory (native hint)
/// or tagged `frozen`. Mirrors `compat::record_is_frozen`.
pub(crate) fn record_is_frozen_for_search(
    record: &NovelRecord,
    frozen_ids: Option<&HashSet<i64>>,
) -> bool {
    frozen_ids.is_some_and(|ids| ids.contains(&record.id))
        || record.tags.iter().any(|tag| tag == "frozen")
}

/// Status text used by `Status`/`Any` search terms. Matches the Web UI's
/// `record_status_text` semantics (lowercase, comma-separated).
pub(crate) fn record_status_text(record: &NovelRecord, frozen: bool) -> String {
    let mut status = Vec::new();
    if frozen {
        status.push("凍結");
    }
    if record.tags.iter().any(|tag| tag == "end") || record.end {
        status.push("完結");
    }
    if record.tags.iter().any(|tag| tag == "404") {
        status.push("削除");
    }
    if record.suspend {
        status.push("中断");
    }
    status.join(", ").to_lowercase()
}

/// Match one term against a record (values are OR-combined; negation flips
/// the result). Mirrors the Web UI's `record_matches_token`.
pub(crate) fn record_matches_term(
    record: &NovelRecord,
    term: &SearchTerm,
    frozen: bool,
) -> bool {
    let title = record.title.to_lowercase();
    let author = record.author.to_lowercase();
    let sitename = record.sitename.to_lowercase();
    let status = record_status_text(record, frozen);
    let tags: Vec<String> = record.tags.iter().map(|tag| tag.to_lowercase()).collect();

    let matched = match term.field {
        SearchField::Tag => term
            .values
            .iter()
            .any(|value| tags.iter().any(|tag| tag.contains(value))),
        SearchField::Author => term.values.iter().any(|value| author.contains(value)),
        SearchField::Site => term.values.iter().any(|value| sitename.contains(value)),
        SearchField::Title => term.values.iter().any(|value| title.contains(value)),
        SearchField::Status => term.values.iter().any(|value| status.contains(value)),
        SearchField::Any => term.values.iter().any(|value| {
            title.contains(value)
                || author.contains(value)
                || sitename.contains(value)
                || status.contains(value)
                || tags.iter().any(|tag| tag.contains(value))
        }),
    };

    if term.negated {
        !matched
    } else {
        matched
    }
}

/// All terms are AND-combined.
pub(crate) fn record_matches_terms(
    record: &NovelRecord,
    terms: &[SearchTerm],
    frozen: bool,
) -> bool {
    terms
        .iter()
        .all(|term| record_matches_term(record, term, frozen))
}

/// Typed-filter match. Every branch maps to a D1 WHERE clause.
pub(crate) fn record_matches_filter(record: &NovelRecord, filter: &NovelFilter) -> bool {
    let keyword = filter.keyword.as_ref().map(|keyword| keyword.to_lowercase());
    if let Some(ids) = &filter.ids
        && !ids.iter().any(|id| id.0 == record.id)
    {
        return false;
    }
    if let Some(keyword) = keyword
        && !record.title.to_lowercase().contains(&keyword)
        && !record.author.to_lowercase().contains(&keyword)
    {
        return false;
    }
    if let Some(site) = &filter.site
        && !record.sitename.eq_ignore_ascii_case(site)
    {
        return false;
    }
    if let Some(domain) = &filter.domain
        && record.domain.as_deref() != Some(domain.as_str())
    {
        return false;
    }
    if let Some(tag) = &filter.tag
        && !record.tags.iter().any(|record_tag| record_tag == tag)
    {
        return false;
    }
    if let Some(ncode) = &filter.ncode
        && record.ncode.as_deref() != Some(ncode.as_str())
    {
        return false;
    }
    if let Some(is_narou) = filter.is_narou
        && record.is_narou != is_narou
    {
        return false;
    }
    if let Some(suspend) = filter.suspend
        && record.suspend != suspend
    {
        return false;
    }
    if let Some(novel_type) = filter.novel_type
        && record.novel_type != novel_type
    {
        return false;
    }
    if let Some(end) = filter.end
        && record.end != end
    {
        return false;
    }
    if !filter.terms.is_empty()
        && !record_matches_terms(
            record,
            &filter.terms,
            record_is_frozen_for_search(record, filter.frozen_ids.as_ref()),
        )
    {
        return false;
    }
    true
}

/// Compare two records by a typed key, delegating to the canonical
/// `db::compare_records_by_key` so repository sorting matches the existing
/// CLI/Web sort semantics (BUG-9 None-stable order).
pub(crate) fn compare_records_by_sort_key(
    a: &NovelRecord,
    b: &NovelRecord,
    key: NovelSortKey,
) -> Ordering {
    crate::db::compare_records_by_key(a, b, key.as_db_key())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn novel_id_conversions() {
        let id = NovelId::from(42_i64);
        assert_eq!(id.0, 42);
        let back: i64 = id.into();
        assert_eq!(back, 42);
    }

    #[test]
    fn sort_key_roundtrips_through_db_keys() {
        for key in NovelSortKey::ALL {
            let db_key = key.as_db_key();
            assert_eq!(NovelSortKey::from_db_key(db_key), Some(*key));
        }
        assert_eq!(NovelSortKey::from_db_key("nonsense"), None);
    }

    fn sample_record(id: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: "author".into(),
            title: "title".into(),
            file_title: "title".into(),
            toc_url: format!("https://example.com/novel/{id}"),
            sitename: "example".into(),
            novel_type: 1,
            end: false,
            last_update: chrono::Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn typed_filter_matches_typed_fields() {
        let mut record = sample_record(1);
        record.sitename = "カクヨム".into();
        record.is_narou = false;
        record.novel_type = 2;
        record.tags = vec!["sf".into(), "frozen".into()];

        assert!(record_matches_filter(
            &record,
            &NovelFilter {
                site: Some("カクヨム".into()),
                novel_type: Some(2),
                tag: Some("sf".into()),
                ..Default::default()
            }
        ));
        assert!(!record_matches_filter(
            &record,
            &NovelFilter {
                tag: Some("coffee".into()),
                ..Default::default()
            }
        ));
        assert!(record_matches_filter(
            &record,
            &NovelFilter {
                ids: Some(vec![NovelId::from(1)]),
                ..Default::default()
            }
        ));
        assert!(!record_matches_filter(
            &record,
            &NovelFilter {
                ids: Some(vec![NovelId::from(2)]),
                ..Default::default()
            }
        ));
    }

    #[test]
    fn keyword_matches_title_or_author_case_insensitively() {
        let record = sample_record(1);
        assert!(record_matches_filter(
            &record,
            &NovelFilter {
                keyword: Some("TITLE".into()),
                ..Default::default()
            }
        ));
        assert!(record_matches_filter(
            &record,
            &NovelFilter {
                keyword: Some("AUTHOR".into()),
                ..Default::default()
            }
        ));
        assert!(!record_matches_filter(
            &record,
            &NovelFilter {
                keyword: Some("missing".into()),
                ..Default::default()
            }
        ));
    }

    #[test]
    fn search_terms_support_fielded_or_and_negation() {
        let mut record = sample_record(1);
        record.title = "テスト小説".into();
        record.author = "作者".into();
        record.tags = vec!["sf".into(), "coffee".into()];

        let terms = vec![SearchTerm::new(
            SearchField::Tag,
            false,
            vec!["sf".into(), "mystery".into()],
        )];
        assert!(record_matches_terms(&record, &terms, false));

        let negated = vec![SearchTerm::new(
            SearchField::Tag,
            true,
            vec!["mystery".into()],
        )];
        assert!(record_matches_terms(&record, &negated, false));
        assert!(!record_matches_terms(
            &record,
            &[SearchTerm::new(SearchField::Tag, true, vec!["sf".into()])],
            false
        ));
    }

    #[test]
    fn status_terms_include_frozen_when_frozen() {
        let mut record = sample_record(1);
        record.end = true;
        let frozen_ids = HashSet::from([1]);

        assert!(record_matches_terms(
            &record,
            &[SearchTerm::new(SearchField::Status, false, vec!["完結".into()])],
            false
        ));
        assert!(record_matches_terms(
            &record,
            &[SearchTerm::new(SearchField::Status, false, vec!["凍結".into()])],
            frozen_ids.contains(&record.id)
        ));
        assert!(!record_matches_terms(
            &record,
            &[SearchTerm::new(SearchField::Status, false, vec!["凍結".into()])],
            false
        ));
        assert_eq!(record_status_text(&record, true), "凍結, 完結");
    }

    #[test]
    fn ncode_and_domain_filters_are_exact() {
        let mut record = sample_record(1);
        record.ncode = Some("n1234ab".into());
        record.domain = Some("ncode.syosetu.com".into());

        assert!(record_matches_filter(
            &record,
            &NovelFilter {
                ncode: Some("n1234ab".into()),
                ..Default::default()
            }
        ));
        assert!(record_matches_filter(
            &record,
            &NovelFilter {
                domain: Some("ncode.syosetu.com".into()),
                ..Default::default()
            }
        ));
        assert!(!record_matches_filter(
            &record,
            &NovelFilter {
                ncode: Some("n9999zz".into()),
                ..Default::default()
            }
        ));
    }
}
