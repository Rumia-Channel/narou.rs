//! Novel library list service.
//!
//! Owns the business logic behind the web UI's novel table: search token
//! parsing (quoted terms, negation, fielded and OR values), typed filter
//! construction, paginated query/count, frozen status, site timezone lookup,
//! and the six-hour new-arrival marker. Rendering (DataTables `draw`,
//! JSON, HTTP) stays in the web layer.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::application::error::ApplicationError;
use crate::application::events::{FreezeStore, SiteTimezone, SiteTimezoneProvider};
use crate::db::NovelRecord;
use crate::platform::{
    NovelFilter, NovelQuery, NovelRepository, NovelSort, NovelSortKey, SearchField, SearchTerm,
};
use crate::platform::clock::Clock;

/// New-arrival marker window: a novel is "new" for six hours after its
/// new-arrivals date (Ruby `ANNOTATION_COLOR_TIME_LIMIT` semantics).
pub const NEW_ARRIVALS_TIME_LIMIT_SECS: i64 = 6 * 60 * 60;

/// Default timezone used when a record's site has no configured timezone.
pub const DEFAULT_SITE_TIMEZONE: &str = "Asia/Tokyo";

/// Sort column of the web table, as a typed enum.
///
/// The web layer maps its DataTables column index to this enum; the service
/// never sees raw column numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibrarySortColumn {
    Id,
    LastUpdate,
    GeneralLastup,
    LastCheckDate,
    Title,
    Author,
    SiteName,
    NovelType,
    GeneralAllNo,
    Length,
}

impl LibrarySortColumn {
    /// Map to the repository's typed sort key.
    pub fn sort_key(self) -> NovelSortKey {
        match self {
            Self::Id => NovelSortKey::Id,
            Self::LastUpdate => NovelSortKey::LastUpdate,
            Self::GeneralLastup => NovelSortKey::GeneralLastup,
            Self::LastCheckDate => NovelSortKey::LastCheckDate,
            Self::Title => NovelSortKey::Title,
            Self::Author => NovelSortKey::Author,
            Self::SiteName => NovelSortKey::SiteName,
            Self::NovelType => NovelSortKey::NovelType,
            Self::GeneralAllNo => NovelSortKey::GeneralAllNo,
            Self::Length => NovelSortKey::Length,
        }
    }
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibrarySortOrder {
    Ascending,
    Descending,
}

/// Request for a page of the novel library.
#[derive(Debug, Clone)]
pub struct LibraryListRequest {
    /// Free-text search: `filter` and `search[value]` combined as AND terms.
    /// Quoted phrases, `-`/`^`/`!` negation, `field:value` and `a|b` OR
    /// values are supported.
    pub search: Option<String>,
    /// First row to return (0-based).
    pub start: usize,
    /// Maximum rows to return. `None` means "all matching rows".
    pub length: Option<usize>,
    /// Sort column.
    pub sort_column: LibrarySortColumn,
    /// Sort direction.
    pub sort_order: LibrarySortOrder,
}

/// One row of the library table: the business fields the web list needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NovelSummary {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub sitename: String,
    pub novel_type: u8,
    pub end: bool,
    pub last_update: DateTime<Utc>,
    pub general_lastup: Option<DateTime<Utc>>,
    pub last_check_date: Option<DateTime<Utc>>,
    pub new_arrivals_date: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
    /// True within the six-hour window after `new_arrivals_date`.
    pub new_arrivals: bool,
    /// True when the record is in the freeze inventory or tagged `frozen`.
    pub frozen: bool,
    pub suspend: bool,
    pub length: Option<i64>,
    pub toc_url: String,
    pub general_all_no: Option<i64>,
}

/// A page of the library: the filtered rows plus the total counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryPage {
    /// Total records in the library (unfiltered).
    pub records_total: u64,
    /// Records matching the search filter.
    pub records_filtered: u64,
    /// The requested page of rows.
    pub data: Vec<NovelSummary>,
}

/// Concrete library service.
///
/// All dependencies are injected platform ports, so the service is
/// testable with the platform mocks and usable from both the native web
/// layer and a future Worker backend.
pub struct LibraryService {
    novels: Arc<dyn NovelRepository>,
    clock: Arc<dyn Clock>,
    freeze_store: Arc<dyn FreezeStore>,
    timezones: Arc<dyn SiteTimezoneProvider>,
}

impl LibraryService {
    /// Create the service with injected dependencies.
    pub fn new(
        novels: Arc<dyn NovelRepository>,
        clock: Arc<dyn Clock>,
        freeze_store: Arc<dyn FreezeStore>,
        timezones: Arc<dyn SiteTimezoneProvider>,
    ) -> Self {
        Self {
            novels,
            clock,
            freeze_store,
            timezones,
        }
    }

    pub async fn get(
        &self,
        id: crate::platform::NovelId,
    ) -> Result<Option<NovelRecord>, ApplicationError> {
        self.novels.get(id).await.map_err(ApplicationError::platform)
    }
    pub async fn find_by_toc_url(
        &self,
        url: &str,
    ) -> Result<Option<NovelRecord>, ApplicationError> {
        self.novels
            .find_by_toc_url(url)
            .await
            .map_err(ApplicationError::platform)
    }

    pub async fn find_by_ncode(
        &self,
        ncode: &str,
    ) -> Result<Option<NovelRecord>, ApplicationError> {
        self.novels
            .find_by_ncode(ncode)
            .await
            .map_err(ApplicationError::platform)
    }

    pub async fn find_by_title(
        &self,
        title: &str,
    ) -> Result<Option<NovelRecord>, ApplicationError> {
        self.novels
            .find_by_title(title)
            .await
            .map_err(ApplicationError::platform)
    }


    pub async fn count(&self) -> Result<usize, ApplicationError> {
        self.novels
            .count(&NovelFilter::all())
            .await
            .map(|count| count as usize)
            .map_err(ApplicationError::platform)
    }
    pub async fn frozen_ids(&self) -> Result<HashSet<i64>, ApplicationError> {
        self.freeze_store
            .frozen_ids()
            .await
            .map_err(ApplicationError::platform)
    }
    pub async fn records(&self) -> Result<Vec<NovelRecord>, ApplicationError> {
        self.novels
            .query(&NovelQuery::page(
                NovelFilter::all(),
                NovelSort::by(crate::platform::NovelSortKey::Id),
                0,
                usize::MAX,
            ))
            .await
            .map_err(ApplicationError::platform)
    }


    /// Fetch one page of the library.
    ///
    /// Runs the count and the page query concurrently, then decorates each
    /// row with frozen status and the new-arrival marker.
    pub async fn list(
        &self,
        request: &LibraryListRequest,
    ) -> Result<LibraryPage, ApplicationError> {
        let frozen_ids = self.freeze_store.frozen_ids().await.map_err(ApplicationError::platform)?;
        let filter = self.build_filter_with_ids(request.search.as_deref(), &frozen_ids);
        let sort = NovelSort {
            key: request.sort_column.sort_key(),
            reverse: request.sort_order == LibrarySortOrder::Descending,
        };

        let (records_total, records_filtered, records) = futures::future::try_join3(
            self.novels.count(&NovelFilter::all()),
            self.novels.count(&filter),
            self.novels.query(&NovelQuery::page(
                filter.clone(),
                sort,
                request.start,
                request.length.unwrap_or(usize::MAX),
            )),
        )
        .await
        .map_err(ApplicationError::platform)?;

        let now = self.clock.now_utc();
        let default_timezone = site_timezone(Some(DEFAULT_SITE_TIMEZONE));

        let mut data = Vec::with_capacity(records.len());
        for record in records {
            let timezone = self
                .record_site_timezone(&record, default_timezone)
                .await
                .unwrap_or(default_timezone);
            let is_new = is_new_arrivals_marker(
                record.new_arrivals_date,
                record.last_update,
                now,
                timezone,
            );
            let is_frozen = record_is_frozen(&record, &frozen_ids);
            data.push(NovelSummary {
                id: record.id,
                title: record.title,
                author: record.author,
                sitename: record.sitename,
                novel_type: record.novel_type,
                end: record.end,
                last_update: record.last_update,
                general_lastup: record.general_lastup,
                last_check_date: record.last_check_date,
                new_arrivals_date: record.new_arrivals_date,
                tags: record.tags,
                new_arrivals: is_new,
                frozen: is_frozen,
                suspend: record.suspend,
                length: record.length,
                toc_url: record.toc_url,
                general_all_no: record.general_all_no,
            });
        }

        Ok(LibraryPage {
            records_total,
            records_filtered,
            data,
        })
    }

    /// Build the typed filter for a search string.
    ///
    /// The search string is split into quoted-aware terms; each term becomes
    /// a [`SearchTerm`] (fielded, OR values, optional negation) and all terms
    /// are AND-combined, matching the web UI's `filter`/`search[value]`
    /// semantics. The frozen id set is attached so `Status`/`Any` terms can
    /// match the `凍結` status text.
    pub async fn build_filter(
        &self,
        search: Option<&str>,
    ) -> Result<NovelFilter, ApplicationError> {
        let frozen_ids = self.freeze_store.frozen_ids().await.map_err(ApplicationError::platform)?;
        Ok(self.build_filter_with_ids(search, &frozen_ids))
    }

    /// Build the typed filter from a search string and a caller-provided
    /// frozen id set (avoids a second freeze-store round trip when the
    /// caller already loaded the ids).
    pub fn build_filter_with_ids(
        &self,
        search: Option<&str>,
        frozen_ids: &HashSet<i64>,
    ) -> NovelFilter {
        let terms = collect_search_tokens(search)
            .into_iter()
            .map(|token| {
                let field = match token.field.as_deref() {
                    Some("tag") => SearchField::Tag,
                    Some("author") => SearchField::Author,
                    Some("site") | Some("sitename") => SearchField::Site,
                    Some("title") => SearchField::Title,
                    Some("status") => SearchField::Status,
                    _ => SearchField::Any,
                };
                SearchTerm::new(field, token.negated, token.values)
            })
            .collect::<Vec<_>>();
        NovelFilter {
            terms,
            frozen_ids: Some(frozen_ids.clone()),
            ..Default::default()
        }
    }

    /// Resolve the timezone for a record's site domain, falling back to the
    /// default timezone when the domain is unknown or has no entry.
    async fn record_site_timezone(
        &self,
        record: &NovelRecord,
        default_timezone: SiteTimezone,
    ) -> Option<SiteTimezone> {
        let domain = record_domain(record)?;
        match self.timezones.timezone_for_domain(domain).await {
            Ok(Some(timezone)) => Some(timezone),
            _ => Some(default_timezone),
        }
    }
}

// ---------------------------------------------------------------------------
// Search token parsing (moved from the web layer).
// ---------------------------------------------------------------------------

/// One parsed search token: a field, OR-combined values, optional negation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchToken {
    negated: bool,
    field: Option<String>,
    values: Vec<String>,
}

/// Split a search string into quoted-aware terms.
fn split_search_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;

    for ch in query.chars() {
        if ch == '"' {
            quoted = !quoted;
            current.push(ch);
            continue;
        }
        if ch.is_whitespace() && !quoted {
            if !current.trim().is_empty() {
                terms.push(current.trim().to_string());
            }
            current.clear();
            continue;
        }
        current.push(ch);
    }

    if !current.trim().is_empty() {
        terms.push(current.trim().to_string());
    }

    terms
}

/// Strip surrounding quotes from a value.
fn strip_search_quotes(value: &str) -> &str {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Split a value on `|` (OR), honoring quotes, lowercasing each part.
fn split_search_values(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quoted = false;

    for ch in value.chars() {
        if ch == '"' {
            quoted = !quoted;
            current.push(ch);
            continue;
        }
        if ch == '|' && !quoted {
            let trimmed = strip_search_quotes(current.trim()).trim().to_lowercase();
            if !trimmed.is_empty() {
                values.push(trimmed);
            }
            current.clear();
            continue;
        }
        current.push(ch);
    }

    let trimmed = strip_search_quotes(current.trim()).trim().to_lowercase();
    if !trimmed.is_empty() {
        values.push(trimmed);
    }

    values
}

/// Parse one raw term into a token: `-`/`^`/`!` negation, `field:value`.
fn parse_search_token(raw_term: &str) -> SearchToken {
    let term = raw_term.trim();
    let negated = matches!(term.chars().next(), Some('-' | '^' | '!'));
    let body = if negated { &term[1..] } else { term };
    let (field, value) = if let Some(colon) = body.find(':') {
        let field = body[..colon].trim();
        if field.is_empty() {
            (None, body)
        } else {
            (Some(field.to_lowercase()), body[colon + 1..].trim())
        }
    } else {
        (None, body)
    };

    SearchToken {
        negated,
        field,
        values: split_search_values(value),
    }
}

/// Parse a search string into tokens, dropping empty ones.
fn collect_search_tokens(search: Option<&str>) -> Vec<SearchToken> {
    search
        .into_iter()
        .flat_map(split_search_terms)
        .map(|term| parse_search_token(&term))
        .filter(|token| !token.values.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Frozen status, timezone lookup, new-arrival marker.
// ---------------------------------------------------------------------------

/// True when a record is frozen: in the freeze inventory or tagged `frozen`.
fn record_is_frozen(record: &NovelRecord, frozen_ids: &HashSet<i64>) -> bool {
    frozen_ids.contains(&record.id) || record.tags.iter().any(|tag| tag == "frozen")
}

/// The site domain of a record: the explicit `domain` field, else the
/// `toc_url` host.
fn record_domain(record: &NovelRecord) -> Option<&str> {
    record
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|domain| !domain.is_empty())
        .or_else(|| {
            let domain = domain_of(&record.toc_url).trim();
            (!domain.is_empty()).then_some(domain)
        })
}

/// Extract the host from a URL (https:// or http:// prefix, up to the first
/// `/`). Mirrors `downloader::http_policy::domain_of` without depending on
/// the downloader module.
fn domain_of(url: &str) -> &str {
    let s = url.strip_prefix("https://").unwrap_or(url);
    let s = s.strip_prefix("http://").unwrap_or(s);
    s.split('/').next().unwrap_or(s)
}

/// Parse a timezone name into a [`SiteTimezone`], with the same aliases the
/// downloader accepts (`JST`, `UTC`, `GMT`, `Z`, `+09:00`, ...).
fn site_timezone(timezone: Option<&str>) -> SiteTimezone {
    let configured = timezone
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SITE_TIMEZONE);
    parse_site_timezone(configured).unwrap_or_else(default_site_timezone)
}

fn default_site_timezone() -> SiteTimezone {
    SiteTimezone::Named(chrono_tz::Asia::Tokyo)
}

fn parse_site_timezone(value: &str) -> Option<SiteTimezone> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let upper = value.to_ascii_uppercase();
    let timezone_name = match upper.as_str() {
        "JST" | "ASIA/TOKYO/JST" => "Asia/Tokyo",
        "UTC" | "GMT" | "Z" => "UTC",
        _ => value,
    };
    if let Ok(tz) = timezone_name.parse::<chrono_tz::Tz>() {
        return Some(SiteTimezone::Named(tz));
    }

    let (sign, rest) = match value.as_bytes().first().copied() {
        Some(b'+') => (1, &value[1..]),
        Some(b'-') => (-1, &value[1..]),
        _ => return None,
    };
    let compact = rest.replace(':', "");
    let (hours, minutes) = match compact.len() {
        2 => (compact.parse::<i32>().ok()?, 0),
        4 => (
            compact[..2].parse::<i32>().ok()?,
            compact[2..].parse::<i32>().ok()?,
        ),
        _ => return None,
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    chrono::FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60)).map(SiteTimezone::Fixed)
}

/// True when the record is a new arrival: `new_arrivals_date` is not older
/// than `last_update` and the six-hour window (in the site's local time)
/// has not expired.
fn is_new_arrivals_marker(
    new_arrivals_date: Option<DateTime<Utc>>,
    last_update: DateTime<Utc>,
    now: DateTime<Utc>,
    timezone: SiteTimezone,
) -> bool {
    new_arrivals_date.is_some_and(|nad| {
        let limit = Duration::seconds(NEW_ARRIVALS_TIME_LIMIT_SECS);
        let nad = timezone.local_naive_datetime(nad);
        let last_update = timezone.local_naive_datetime(last_update);
        let now = timezone.local_naive_datetime(now);
        nad >= last_update && (nad + limit) >= now
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::events::{EmptySiteTimezoneProvider, SystemFreezeStore};
    use crate::platform::mocks::MemoryNovelRepository;
    use crate::platform::clock::Clock;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// Test-only controllable clock.
    #[derive(Debug, Default)]
    struct FakeClock(AtomicI64);

    impl Clock for FakeClock {
        fn now_utc(&self) -> DateTime<Utc> {
            DateTime::from_timestamp(self.0.load(Ordering::SeqCst), 0).unwrap()
        }
        fn advance(&self, duration: std::time::Duration) {
            self.0
                .fetch_add(duration.as_secs() as i64, Ordering::SeqCst);
        }
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

    fn service(
        repo: Arc<dyn NovelRepository>,
        clock: Arc<dyn Clock>,
    ) -> LibraryService {
        LibraryService::new(
            repo,
            clock,
            Arc::new(SystemFreezeStore),
            Arc::new(EmptySiteTimezoneProvider),
        )
    }

    fn request(search: Option<&str>, start: usize, length: Option<usize>) -> LibraryListRequest {
        LibraryListRequest {
            search: search.map(str::to_string),
            start,
            length,
            sort_column: LibrarySortColumn::Id,
            sort_order: LibrarySortOrder::Ascending,
        }
    }

    #[test]
    fn search_token_parsing_supports_quotes_negation_fields_and_or() {
        let tokens = collect_search_tokens(Some("title:\"テスト 小説\" -tag:coffee|mystery 完結"));
        assert_eq!(tokens.len(), 3);

        let title = &tokens[0];
        assert!(!title.negated);
        assert_eq!(title.field.as_deref(), Some("title"));
        assert_eq!(title.values, vec!["テスト 小説".to_string()]);

        let tag = &tokens[1];
        assert!(tag.negated);
        assert_eq!(tag.field.as_deref(), Some("tag"));
        assert_eq!(tag.values, vec!["coffee".to_string(), "mystery".to_string()]);

        let plain = &tokens[2];
        assert!(!plain.negated);
        assert_eq!(plain.field, None);
        assert_eq!(plain.values, vec!["完結".to_string()]);
    }

    #[test]
    fn search_token_parsing_drops_empty_values() {
        let tokens = collect_search_tokens(Some("tag:  -title:"));
        assert!(tokens.is_empty());
    }

    #[test]
    fn build_filter_maps_fields_to_typed_search_terms() {
        let repo = Arc::new(MemoryNovelRepository::new());
        let service = service(repo, Arc::new(FakeClock::default()));
        let filter = futures::executor::block_on(service.build_filter(Some(
            "title:foo -site:bar|baz status:凍結",
        )))
        .unwrap();

        assert_eq!(filter.terms.len(), 3);
        assert_eq!(filter.terms[0].field, SearchField::Title);
        assert_eq!(filter.terms[0].values, vec!["foo".to_string()]);
        assert!(!filter.terms[0].negated);
        assert_eq!(filter.terms[1].field, SearchField::Site);
        assert_eq!(filter.terms[1].values, vec!["bar".to_string(), "baz".to_string()]);
        assert!(filter.terms[1].negated);
        assert_eq!(filter.terms[2].field, SearchField::Status);
        assert_eq!(filter.terms[2].values, vec!["凍結".to_string()]);
        assert!(filter.frozen_ids.is_some());
    }

    #[test]
    fn list_paginates_and_counts() {
        let mut records = Vec::new();
        for id in 1..=5 {
            let mut record = sample_record(id);
            record.title = format!("novel {id}");
            records.push(record);
        }
        let repo = Arc::new(MemoryNovelRepository::from_records(records));
        let service = service(repo, Arc::new(FakeClock::default()));

        let page = futures::executor::block_on(service.list(&request(None, 1, Some(2)))).unwrap();
        assert_eq!(page.records_total, 5);
        assert_eq!(page.records_filtered, 5);
        assert_eq!(page.data.len(), 2);
        assert_eq!(page.data[0].id, 2);
        assert_eq!(page.data[1].id, 3);
    }

    #[test]
    fn list_filters_by_search_terms() {
        let mut records = Vec::new();
        for id in 1..=3 {
            let mut record = sample_record(id);
            record.title = format!("novel {id}");
            records.push(record);
        }
        let repo = Arc::new(MemoryNovelRepository::from_records(records));
        let service = service(repo, Arc::new(FakeClock::default()));

        let page = futures::executor::block_on(service.list(&request(Some("novel 2"), 0, None)))
            .unwrap();
        assert_eq!(page.records_total, 3);
        assert_eq!(page.records_filtered, 1);
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.data[0].id, 2);
    }

    #[test]
    fn list_marks_frozen_records() {
        let mut record = sample_record(1);
        record.tags = vec!["frozen".into()];
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
        let service = service(repo, Arc::new(FakeClock::default()));

        let page = futures::executor::block_on(service.list(&request(None, 0, None))).unwrap();
        assert_eq!(page.data.len(), 1);
        assert!(page.data[0].frozen);
    }

    #[test]
    fn new_arrivals_marker_uses_six_hour_window() {
        let last_update = chrono::Utc::now();
        let timezone = site_timezone(Some("Asia/Tokyo"));

        assert!(is_new_arrivals_marker(
            Some(last_update),
            last_update,
            last_update + Duration::hours(5),
            timezone
        ));
        assert!(is_new_arrivals_marker(
            Some(last_update),
            last_update,
            last_update + Duration::hours(6),
            timezone
        ));
        assert!(!is_new_arrivals_marker(
            Some(last_update),
            last_update,
            last_update + Duration::hours(7),
            timezone
        ));
    }

    #[test]
    fn new_arrivals_marker_requires_date_not_older_than_last_update() {
        let last_update = chrono::Utc::now();
        let earlier = last_update - Duration::minutes(1);
        let now = last_update + Duration::minutes(30);
        assert!(!is_new_arrivals_marker(
            Some(earlier),
            last_update,
            now,
            site_timezone(Some("Asia/Tokyo"))
        ));
    }

    #[test]
    fn new_arrivals_marker_uses_site_timezone_for_local_order() {
        let last_update = chrono::DateTime::parse_from_rfc3339("2024-11-03T05:50:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let new_arrivals = chrono::DateTime::parse_from_rfc3339("2024-11-03T06:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let now = new_arrivals + Duration::minutes(30);

        assert!(is_new_arrivals_marker(
            Some(new_arrivals),
            last_update,
            now,
            site_timezone(Some("UTC"))
        ));
        assert!(!is_new_arrivals_marker(
            Some(new_arrivals),
            last_update,
            now,
            site_timezone(Some("America/New_York"))
        ));
    }

    #[test]
    fn list_marks_new_arrivals_within_window() {
        let now = chrono::Utc::now();
        let mut record = sample_record(1);
        record.last_update = now - Duration::hours(1);
        record.new_arrivals_date = Some(now - Duration::hours(1));
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
        let clock = Arc::new(FakeClock(AtomicI64::new(now.timestamp())));
        let service = service(repo, clock);

        let page = futures::executor::block_on(service.list(&request(None, 0, None))).unwrap();
        assert_eq!(page.data.len(), 1);
        assert!(page.data[0].new_arrivals);
    }

    #[test]
    fn list_clears_new_arrivals_after_six_hours() {
        let now = chrono::Utc::now();
        let mut record = sample_record(1);
        record.last_update = now - Duration::hours(7);
        record.new_arrivals_date = Some(now - Duration::hours(7));
        let repo = Arc::new(MemoryNovelRepository::from_records(vec![record]));
        let clock = Arc::new(FakeClock(AtomicI64::new(now.timestamp())));
        let service = service(repo, clock);

        let page = futures::executor::block_on(service.list(&request(None, 0, None))).unwrap();
        assert_eq!(page.data.len(), 1);
        assert!(!page.data[0].new_arrivals);
    }

    #[test]
    fn record_domain_falls_back_to_toc_url_host() {
        let mut record = sample_record(1);
        record.domain = None;
        record.toc_url = "https://ncode.syosetu.com/n1234ab/".into();
        assert_eq!(record_domain(&record), Some("ncode.syosetu.com"));

        record.domain = Some("kakuyomu.jp".into());
        assert_eq!(record_domain(&record), Some("kakuyomu.jp"));
    }

    #[test]
    fn site_timezone_parses_aliases_and_fixed_offsets() {
        assert_eq!(
            site_timezone(Some("JST")),
            SiteTimezone::Named(chrono_tz::Asia::Tokyo)
        );
        assert_eq!(
            site_timezone(Some("UTC")),
            SiteTimezone::Named(chrono_tz::UTC)
        );
        assert_eq!(
            site_timezone(Some("+09:00")),
            SiteTimezone::Fixed(chrono::FixedOffset::east_opt(9 * 3600).unwrap())
        );
        assert_eq!(
            site_timezone(Some("-05:00")),
            SiteTimezone::Fixed(chrono::FixedOffset::east_opt(-5 * 3600).unwrap())
        );
        // Unknown names fall back to the default (Asia/Tokyo).
        assert_eq!(
            site_timezone(Some("Not/AZone")),
            SiteTimezone::Named(chrono_tz::Asia::Tokyo)
        );
    }
}
