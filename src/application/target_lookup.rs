//! Target resolution shared by the CLI, the native Web server and the Worker.
//!
//! One ordered lookup plan ([`TargetLookupPlan`]) drives both the synchronous
//! native adapter ([`resolve_target_record_sync`], for CLI commands that lock
//! the YAML state directly) and the asynchronous repository adapter
//! ([`resolve_target_record`], for the native Web layer and the Worker). The
//! observable differences between `narou update`, `narou convert`,
//! `narou download`'s existing-novel check, `narou mail` and the read paths
//! are explicit mode parameters (alias handling, URL fallback, title
//! fallback) instead of per-caller copies.
//!
//! `tag:` / `^tag:` / bare-name expansion is the same kind of shared
//! decision: [`expand_tag_targets_from`] holds the rules, and the sync /
//! async drivers only fetch the tag member ids and the full id list.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::aliases::{alias_to_target_for_update, resolve_alias_target};
use super::events::SiteDefinitionProvider;
use super::library::LibraryService;
use crate::db::NovelRecord;
use crate::downloader::site_setting::SiteSetting;
use crate::downloader::{Downloader, TargetType};
use crate::error::Result;
use crate::platform::repository::NovelLookup;
use crate::platform::{NovelFilter, NovelId, NovelRepository};

// ---------------------------------------------------------------------------
// Lookup plan
// ---------------------------------------------------------------------------

/// How the alias table is applied before classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasHandling {
    /// No alias lookup (already resolved by the caller, or an alias-free path).
    AsGiven,
    /// Exact-match alias, one hop, input unchanged when unmapped
    /// (`aliases::resolve_alias_target`).
    Exact,
    /// Exact-match alias; an unmapped non-numeric input is lowercased
    /// (`aliases::alias_to_target_for_update`, `narou update`).
    UpdateLowercase,
}

/// Whether a signed numeric string is an ID or a title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericLookup {
    /// Only targets classified as IDs (`download` checks and Web read paths).
    Classified,
    /// Try a parsed ID, then classify a missing ID (`update` and `mail`).
    ExistingThenClassify,
    /// A parsed ID is final even when absent (`convert` and diff).
    Parsed,
}

/// How a URL target becomes the canonical TOC URL used for the record lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlStrategy {
    /// The first definition matching the URL must yield
    /// `toc_url_with_url_captures`; no `toc_url()` fallback (`narou update`).
    StrictCaptures,
    /// The first matching definition yields captures, else `toc_url()`
    /// (`narou convert`, `narou download` pre-check, the downloader).
    CapturesOrTocUrl,
    /// Every matching definition is tried in order and the first one whose
    /// TOC URL names a record wins (`narou mail`).
    CapturesOrTocUrlFirstRecord,
    /// `SiteDefinitionProvider::resolve_toc_url`, no raw-URL fallback
    /// (native Web read paths).
    ProviderOnly,
    /// Provider resolution, else the raw target as TOC URL (Worker read/diff).
    ProviderOrRaw,
}

/// Which lookup `TargetType::Other` performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtherLookup {
    /// Title only (`narou download` pre-check, `narou mail`, read paths).
    TitleOnly,
    /// Title, then ncode (`narou update`, `narou convert`, downloader, diff).
    TitleThenNcode,
}

/// The ordered lookup decisions for one resolution pass: alias normalization,
/// URL fallback, and the `Other` (title) lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetLookupPlan {
    pub alias: AliasHandling,
    pub numeric: NumericLookup,
    pub url: UrlStrategy,
    pub other: OtherLookup,
}

impl TargetLookupPlan {
    /// `narou update` and Worker `/api/update`: lowercase unmapped aliases,
    /// strict URL captures, title -> ncode.
    pub const UPDATE: Self = Self {
        alias: AliasHandling::UpdateLowercase,
        numeric: NumericLookup::ExistingThenClassify,
        url: UrlStrategy::StrictCaptures,
        other: OtherLookup::TitleThenNcode,
    };

    /// `narou convert` (with clean / folder / illust / inspect / browser) and
    /// Worker `/api/convert`: exact alias, captures-or-`toc_url()`,
    /// title -> ncode.
    pub const CONVERT: Self = Self {
        alias: AliasHandling::Exact,
        numeric: NumericLookup::Parsed,
        url: UrlStrategy::CapturesOrTocUrl,
        other: OtherLookup::TitleThenNcode,
    };

    /// `narou download`'s existing-novel check (`get_data_by_target`): exact
    /// alias, captures-or-`toc_url()`, title only (Pixiv-style ncodes resolve
    /// only inside the downloader itself).
    pub const DOWNLOAD_CHECK: Self = Self {
        alias: AliasHandling::Exact,
        numeric: NumericLookup::Classified,
        url: UrlStrategy::CapturesOrTocUrl,
        other: OtherLookup::TitleOnly,
    };

    /// `narou mail`'s record lookup: no alias here (the caller applies
    /// `resolve_alias_target` before the `hotentry` check), the first matching
    /// definition whose TOC URL names a record, title only.
    pub const MAIL: Self = Self {
        alias: AliasHandling::AsGiven,
        numeric: NumericLookup::ExistingThenClassify,
        url: UrlStrategy::CapturesOrTocUrlFirstRecord,
        other: OtherLookup::TitleOnly,
    };

    /// Native Web read paths: provider resolution only, title only, no alias.
    pub const READ: Self = Self {
        alias: AliasHandling::AsGiven,
        numeric: NumericLookup::Classified,
        url: UrlStrategy::ProviderOnly,
        other: OtherLookup::TitleOnly,
    };

    /// Worker read paths: provider resolution with the raw-URL fallback the
    /// Worker relies on, title only, no alias.
    pub const READ_WITH_RAW_FALLBACK: Self = Self {
        alias: AliasHandling::AsGiven,
        numeric: NumericLookup::Classified,
        url: UrlStrategy::ProviderOrRaw,
        other: OtherLookup::TitleOnly,
    };

    /// Worker diff (`narou diff` subprocess order): exact alias, provider
    /// resolution with raw fallback, title -> ncode.
    pub const DIFF: Self = Self {
        alias: AliasHandling::Exact,
        numeric: NumericLookup::Parsed,
        url: UrlStrategy::ProviderOrRaw,
        other: OtherLookup::TitleThenNcode,
    };

    /// Worker diff after its alias, trim, and empty-target check.
    pub const ALIAS_APPLIED: Self = Self {
        alias: AliasHandling::AsGiven,
        numeric: NumericLookup::Parsed,
        url: UrlStrategy::ProviderOrRaw,
        other: OtherLookup::TitleThenNcode,
    };

    /// Download execution resolves nonstandard ncodes after titles. Signed
    /// strings remain titles, not record IDs.
    pub const DOWNLOAD: Self = Self {
        alias: AliasHandling::AsGiven,
        numeric: NumericLookup::Classified,
        url: UrlStrategy::CapturesOrTocUrl,
        other: OtherLookup::TitleThenNcode,
    };
}

enum InitialLookup {
    Numeric(i64),
    Classified(TargetType),
}

fn initial_lookup(mode: NumericLookup, target: &str) -> InitialLookup {
    if mode != NumericLookup::Classified && let Ok(id) = target.parse::<i64>() {
        return InitialLookup::Numeric(id);
    }
    let target_type = Downloader::get_target_type(target);
    if mode == NumericLookup::Classified && matches!(target_type, TargetType::Id)
        && let Ok(id) = target.parse::<i64>()
    {
        return InitialLookup::Numeric(id);
    }
    InitialLookup::Classified(target_type)
}

/// The URL definitions a pass resolves through.
pub enum UrlSource<'a> {
    /// Effective site definitions, in definition order (native
    /// `effective_site_settings()` / Worker loaded `SiteSetting`s).
    Settings(&'a [SiteSetting]),
    /// A site-definition provider (native Web / Worker read paths).
    Provider(&'a dyn SiteDefinitionProvider),
}

/// Apply the plan's alias step. Borrows the input unless an alias actually
/// changes the target.
fn effective_target<'a>(
    alias: AliasHandling,
    aliases: &'a HashMap<String, String>,
    target: &'a str,
) -> Cow<'a, str> {
    match alias {
        AliasHandling::AsGiven => Cow::Borrowed(target),
        AliasHandling::Exact => resolve_alias_target(aliases, target),
        AliasHandling::UpdateLowercase => alias_to_target_for_update(aliases, target),
    }
}

/// The TOC URL of the first definition matching `target`: captures, else
/// `toc_url()`. `None` when no definition matches. This is the
/// `CapturesOrTocUrl` rule shared by the CLI's existing-novel checks.
pub fn first_matching_toc_url(settings: &[SiteSetting], target: &str) -> Option<String> {
    let setting = settings.iter().find(|setting| setting.matches_url(target))?;
    Some(
        setting
            .toc_url_with_url_captures(target)
            .unwrap_or_else(|| setting.toc_url()),
    )
}

/// The TOC URL of the first definition matching `target`; `strict` refuses
/// the `toc_url()` fallback (`narou update`).
fn settings_toc_url(settings: &[SiteSetting], target: &str, strict: bool) -> Option<String> {
    if strict {
        let setting = settings.iter().find(|setting| setting.matches_url(target))?;
        setting.toc_url_with_url_captures(target)
    } else {
        first_matching_toc_url(settings, target)
    }
}

/// A [`SiteDefinitionProvider`] over one loaded snapshot of runtime site
/// definitions.
///
/// Worker production wiring loads bundle + user definitions once
/// (`bundled_sites::load_site_settings{,_uncached}`); wrapping that snapshot
/// gives the read paths native's snapshot provider behavior
/// (`resolve_toc_url` with captures-or-`toc_url()`, and the URL validation
/// patterns) instead of the empty provider that always fell back to the raw
/// URL. The snapshot is immutable: callers rebuild the provider when the
/// definitions are reloaded.
pub struct SiteSettingSnapshotProvider {
    settings: Arc<Vec<SiteSetting>>,
}

impl SiteSettingSnapshotProvider {
    /// Wrap one loaded snapshot (bundle + user definitions, runtime gate
    /// applied). Rebuild the provider when the definitions are reloaded.
    pub fn new(settings: Vec<SiteSetting>) -> Self {
        Self {
            settings: Arc::new(settings),
        }
    }
}

impl SiteDefinitionProvider for SiteSettingSnapshotProvider {
    fn resolve_toc_url(&self, url: &str) -> Option<String> {
        self.settings
            .iter()
            .find(|setting| setting.matches_url(url))
            .map(|setting| {
                setting
                    .toc_url_with_url_captures(url)
                    .unwrap_or_else(|| setting.toc_url())
            })
    }

    fn url_patterns_for_validation(&self) -> Vec<String> {
        self.settings
            .iter()
            .flat_map(|setting| setting.url_patterns_for_validation())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Synchronous adapter (native CLI)
// ---------------------------------------------------------------------------

/// Resolve `target` through `plan` against a synchronous lookup port.
///
/// The native CLI (convert / update / download pre-check / mail) runs without
/// an executor and locks the YAML state directly; the caller maps `Ok(None)`
/// to its own "not found" presentation exactly as before.
pub fn resolve_target_record_sync(
    plan: TargetLookupPlan,
    aliases: &HashMap<String, String>,
    urls: UrlSource<'_>,
    target: &str,
    lookup: &dyn NovelLookup,
) -> Result<Option<NovelRecord>> {
    let effective = effective_target(plan.alias, aliases, target);
    let effective = effective.as_ref();
    let target_type = match initial_lookup(plan.numeric, effective) {
        InitialLookup::Numeric(id) => {
            let record = lookup.get(NovelId(id))?;
            if record.is_some() || plan.numeric != NumericLookup::ExistingThenClassify {
                return Ok(record);
            }
            Downloader::get_target_type(effective)
        }
        InitialLookup::Classified(target_type) => target_type,
    };
    match target_type {
        TargetType::Url => resolve_url_sync(plan.url, urls, effective, lookup),
        TargetType::Ncode => lookup.find_by_ncode(effective),
        TargetType::Id => Ok(None),
        TargetType::Other => {
            if let Some(record) = lookup.find_by_title(effective)? {
                return Ok(Some(record));
            }
            match plan.other {
                OtherLookup::TitleOnly => Ok(None),
                OtherLookup::TitleThenNcode => lookup.find_by_ncode(effective),
            }
        }
    }
}

fn resolve_url_sync(
    strategy: UrlStrategy,
    urls: UrlSource<'_>,
    target: &str,
    lookup: &dyn NovelLookup,
) -> Result<Option<NovelRecord>> {
    match urls {
        UrlSource::Settings(settings) => match strategy {
            UrlStrategy::StrictCaptures => match settings_toc_url(settings, target, true) {
                Some(toc_url) => lookup.find_by_toc_url(&toc_url),
                None => Ok(None),
            },
            UrlStrategy::CapturesOrTocUrl => match settings_toc_url(settings, target, false) {
                Some(toc_url) => lookup.find_by_toc_url(&toc_url),
                None => Ok(None),
            },
            UrlStrategy::CapturesOrTocUrlFirstRecord => {
                for setting in settings
                    .iter()
                    .filter(|setting| setting.matches_url(target))
                {
                    let toc_url = setting
                        .toc_url_with_url_captures(target)
                        .unwrap_or_else(|| setting.toc_url());
                    if let Some(record) = lookup.find_by_toc_url(&toc_url)? {
                        return Ok(Some(record));
                    }
                }
                Ok(None)
            }
            UrlStrategy::ProviderOnly | UrlStrategy::ProviderOrRaw => Ok(None),
        },
        UrlSource::Provider(provider) => match strategy {
            UrlStrategy::ProviderOnly => match provider.resolve_toc_url(target) {
                Some(toc_url) => lookup.find_by_toc_url(&toc_url),
                None => Ok(None),
            },
            UrlStrategy::ProviderOrRaw => match provider.resolve_toc_url(target) {
                Some(toc_url) => lookup.find_by_toc_url(&toc_url),
                None => lookup.find_by_toc_url(target),
            },
            UrlStrategy::StrictCaptures
            | UrlStrategy::CapturesOrTocUrl
            | UrlStrategy::CapturesOrTocUrlFirstRecord => Ok(None),
        },
    }
}

// ---------------------------------------------------------------------------
// Asynchronous adapter (native Web / Worker)
// ---------------------------------------------------------------------------

/// Resolve `target` through `plan` against a [`NovelRepository`].
///
/// The native Web layer and the Worker share this adapter; callers that used
/// to swallow repository errors with `.ok().flatten()` keep doing that at the
/// call site.
pub async fn resolve_target_record(
    plan: TargetLookupPlan,
    aliases: &HashMap<String, String>,
    urls: UrlSource<'_>,
    target: &str,
    novels: &dyn NovelRepository,
) -> Result<Option<NovelRecord>> {
    let effective = effective_target(plan.alias, aliases, target);
    let effective = effective.as_ref();
    let target_type = match initial_lookup(plan.numeric, effective) {
        InitialLookup::Numeric(id) => {
            let record = novels.get(NovelId(id)).await?;
            if record.is_some() || plan.numeric != NumericLookup::ExistingThenClassify {
                return Ok(record);
            }
            Downloader::get_target_type(effective)
        }
        InitialLookup::Classified(target_type) => target_type,
    };
    match target_type {
        TargetType::Url => resolve_url(novels, plan.url, urls, effective).await,
        TargetType::Ncode => novels.find_by_ncode(effective).await,
        TargetType::Id => Ok(None),
        TargetType::Other => {
            if let Some(record) = novels.find_by_title(effective).await? {
                return Ok(Some(record));
            }
            match plan.other {
                OtherLookup::TitleOnly => Ok(None),
                OtherLookup::TitleThenNcode => novels.find_by_ncode(effective).await,
            }
        }
    }
}

async fn resolve_url(
    novels: &dyn NovelRepository,
    strategy: UrlStrategy,
    urls: UrlSource<'_>,
    target: &str,
) -> Result<Option<NovelRecord>> {
    match urls {
        UrlSource::Settings(settings) => match strategy {
            UrlStrategy::StrictCaptures => match settings_toc_url(settings, target, true) {
                Some(toc_url) => novels.find_by_toc_url(&toc_url).await,
                None => Ok(None),
            },
            UrlStrategy::CapturesOrTocUrl => match settings_toc_url(settings, target, false) {
                Some(toc_url) => novels.find_by_toc_url(&toc_url).await,
                None => Ok(None),
            },
            UrlStrategy::CapturesOrTocUrlFirstRecord => {
                for setting in settings
                    .iter()
                    .filter(|setting| setting.matches_url(target))
                {
                    let toc_url = setting
                        .toc_url_with_url_captures(target)
                        .unwrap_or_else(|| setting.toc_url());
                    if let Some(record) = novels.find_by_toc_url(&toc_url).await? {
                        return Ok(Some(record));
                    }
                }
                Ok(None)
            }
            UrlStrategy::ProviderOnly | UrlStrategy::ProviderOrRaw => Ok(None),
        },
        UrlSource::Provider(provider) => match strategy {
            UrlStrategy::ProviderOnly => match provider.resolve_toc_url(target) {
                Some(toc_url) => novels.find_by_toc_url(&toc_url).await,
                None => Ok(None),
            },
            UrlStrategy::ProviderOrRaw => match provider.resolve_toc_url(target) {
                Some(toc_url) => novels.find_by_toc_url(&toc_url).await,
                None => novels.find_by_toc_url(target).await,
            },
            UrlStrategy::StrictCaptures
            | UrlStrategy::CapturesOrTocUrl
            | UrlStrategy::CapturesOrTocUrlFirstRecord => Ok(None),
        },
    }
}

// ---------------------------------------------------------------------------
// tag: / ^tag: / bare-name expansion
// ---------------------------------------------------------------------------

/// The expansion class of one raw target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagTargetClass<'a> {
    /// Numeric id; the caller's existence lookup decides whether it is kept
    /// as an id or falls through to the bare-name rule (see
    /// [`TagExpansionLookups`]).
    Numeric(i64),
    /// `^tag:<name>`: every id except the tag's members.
    ExcludeTag(&'a str),
    /// `tag:<name>`: the tag's members.
    Tag(&'a str),
    /// Bare name, expanded when it names a tag.
    Bare(&'a str),
}

/// The consumer-visible tag-expansion variants.
///
/// Both follow the same classification / lookup / dedupe order; the two
/// recorded differences between `narou update` and `narou download` are
/// explicit here instead of drifting:
///
/// - an empty `^tag:<name>` keeps the bare name (`UPDATE`; it falls out in
///   the later id resolution) or expands to every id (`DOWNLOAD`, "all minus
///   nothing"),
/// - an existing numeric target is pushed in canonical parsed form
///   (`UPDATE`) or keeps its original spelling (`DOWNLOAD`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagExpansionVariant {
    /// `narou update` and Worker `/api/update`.
    Update,
    /// `narou download`'s pre-expansion.
    Download,
}

/// Classify one raw target for expansion (`tag:` / `^tag:` / numeric / bare).
pub fn classify_tag_target(target: &str) -> TagTargetClass<'_> {
    if let Ok(id) = target.parse::<i64>() {
        return TagTargetClass::Numeric(id);
    }
    if let Some(name) = target.strip_prefix("^tag:") {
        return TagTargetClass::ExcludeTag(name);
    }
    if let Some(name) = target.strip_prefix("tag:") {
        return TagTargetClass::Tag(name);
    }
    TagTargetClass::Bare(target)
}

/// The tag-name lookups an expansion needs, derived from the raw targets and
/// the numeric targets already known to exist.
///
/// A numeric target that is not in `existing_numeric` falls through to the
/// bare-name rule, so its raw string is looked up as a tag name — the same
/// queries `commands::update::expand_tag_targets` and the Worker's copy ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagExpansionLookups {
    /// Unique names needing an exact-tag id lookup, in first-seen order.
    pub names: Vec<String>,
}

/// Derive the tag-id lookups for `targets`.
pub fn tag_expansion_lookups(
    targets: &[String],
    existing_numeric: &HashSet<i64>,
) -> TagExpansionLookups {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for target in targets {
        let name = match classify_tag_target(target) {
            TagTargetClass::Numeric(id) if existing_numeric.contains(&id) => continue,
            TagTargetClass::Numeric(_) => target.as_str(),
            TagTargetClass::ExcludeTag(name) | TagTargetClass::Tag(name) => name,
            TagTargetClass::Bare(name) => name,
        };
        if seen.insert(name.to_string()) {
            names.push(name.to_string());
        }
    }
    TagExpansionLookups { names }
}

/// Whether a `^tag:` target actually needs the full id list: `UPDATE` only
/// when the tag has members (an empty tag keeps its name), `DOWNLOAD` whenever
/// any `^tag:` target is present (an empty tag expands to every id).
pub fn needs_all_ids(
    targets: &[String],
    tag_ids: &HashMap<String, Vec<i64>>,
    variant: TagExpansionVariant,
) -> bool {
    targets.iter().any(|target| match classify_tag_target(target) {
        TagTargetClass::ExcludeTag(name) => match variant {
            TagExpansionVariant::Update => tag_ids.get(name).is_some_and(|ids| !ids.is_empty()),
            TagExpansionVariant::Download => true,
        },
        _ => false,
    })
}

/// The shared expansion rules: each target is replaced by ids (numbers as
/// strings) when a lookup answered, or by the name itself when the tag has no
/// members; duplicates are removed keeping the first occurrence.
pub fn expand_tag_targets_from(
    targets: &[String],
    existing_numeric: &HashSet<i64>,
    tag_ids: &HashMap<String, Vec<i64>>,
    all_ids: &[i64],
    variant: TagExpansionVariant,
) -> Vec<String> {
    let mut builder = TagExpansionBuilder::new(variant);
    for target in targets {
        match classify_tag_target(target) {
            TagTargetClass::Numeric(id) if existing_numeric.contains(&id) => {
                builder.push_numeric(id, target);
            }
            TagTargetClass::Numeric(_) => {
                builder.push_name_expansion(
                    target,
                    tag_ids.get(target.as_str()).map(|ids| ids.as_slice()),
                );
            }
            TagTargetClass::ExcludeTag(name) => {
                builder.push_excluded_name(
                    name,
                    tag_ids.get(name).map(|ids| ids.as_slice()),
                    all_ids,
                );
            }
            TagTargetClass::Tag(name) => {
                builder.push_name_expansion(name, tag_ids.get(name).map(|ids| ids.as_slice()));
            }
            TagTargetClass::Bare(name) => {
                builder.push_name_expansion(name, tag_ids.get(name).map(|ids| ids.as_slice()));
            }
        }
    }
    builder.finish()
}

/// Driver for the synchronous native CLI: tag members and the full id list are
/// read through [`NovelLookup::scan_ids`] (ID projections, one query each,
/// tag names deduplicated).
pub fn expand_tag_targets_sync(
    targets: &[String],
    lookup: &dyn NovelLookup,
    variant: TagExpansionVariant,
    attribute_tags_enabled: bool,
) -> Vec<String> {
    let mut existing_numeric = HashSet::new();
    for target in targets {
        if let TagTargetClass::Numeric(id) = classify_tag_target(target)
            && lookup.get(NovelId(id)).ok().flatten().is_some()
        {
            existing_numeric.insert(id);
        }
    }
    let lookups = tag_expansion_lookups(targets, &existing_numeric);
    let mut tag_ids = HashMap::new();
    for name in &lookups.names {
        let ids = scan_tag_ids_sync(lookup, name, attribute_tags_enabled).unwrap_or_default();
        tag_ids.insert(name.clone(), ids);
    }
    let all_ids = if needs_all_ids(targets, &tag_ids, variant) {
        scan_all_ids_sync(lookup).unwrap_or_default()
    } else {
        Vec::new()
    };
    expand_tag_targets_from(targets, &existing_numeric, &tag_ids, &all_ids, variant)
}

fn scan_tag_ids_sync(lookup: &dyn NovelLookup, tag_name: &str, attribute_tags_enabled: bool) -> Result<Vec<i64>> {
    let filter = NovelFilter {
        tag: Some(tag_name.to_string()),
        attribute_tags_enabled,
        ..NovelFilter::default()
    };
    Ok(lookup
        .scan_ids(&filter, None, usize::MAX)?
        .into_iter()
        .map(|id| id.0)
        .collect())
}

fn scan_all_ids_sync(lookup: &dyn NovelLookup) -> Result<Vec<i64>> {
    Ok(lookup
        .scan_ids(&NovelFilter::all(), None, usize::MAX)?
        .into_iter()
        .map(|id| id.0)
        .collect())
}

/// Driver for the asynchronous repository (`LibraryService` ID projections).
pub async fn expand_tag_targets(
    library: &LibraryService,
    targets: &[String],
    existing_numeric: &HashSet<i64>,
    variant: TagExpansionVariant,
    attribute_tags_enabled: bool,
) -> Vec<String> {
    let lookups = tag_expansion_lookups(targets, existing_numeric);
    let mut tag_ids = HashMap::new();
    for name in &lookups.names {
        tag_ids.insert(name.clone(), scan_tag_ids(library, name, attribute_tags_enabled).await);
    }
    let all_ids = if needs_all_ids(targets, &tag_ids, variant) {
        scan_all_ids(library).await
    } else {
        Vec::new()
    };
    expand_tag_targets_from(targets, existing_numeric, &tag_ids, &all_ids, variant)
}

async fn scan_tag_ids(library: &LibraryService, tag_name: &str, attribute_tags_enabled: bool) -> Vec<i64> {
    let filter = NovelFilter {
        tag: Some(tag_name.to_string()),
        attribute_tags_enabled,
        ..NovelFilter::default()
    };
    scan_id_projection(library, &filter).await
}

async fn scan_all_ids(library: &LibraryService) -> Vec<i64> {
    scan_id_projection(library, &NovelFilter::all()).await
}

async fn scan_id_projection(library: &LibraryService, filter: &NovelFilter) -> Vec<i64> {
    library
        .all_ids(filter)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect()
}

/// String-keyed accumulator for the expansion rules: pushes keep the first
/// occurrence (native `expand_tag_targets`' trailing `HashSet` dedupe and the
/// Worker copy's identical rule).
#[derive(Debug)]
struct TagExpansionBuilder {
    variant: TagExpansionVariant,
    seen: HashSet<String>,
    expanded: Vec<String>,
}

impl TagExpansionBuilder {
    fn new(variant: TagExpansionVariant) -> Self {
        Self {
            variant,
            seen: HashSet::new(),
            expanded: Vec::new(),
        }
    }

    fn push_value(&mut self, value: &str) {
        if self.seen.insert(value.to_string()) {
            self.expanded.push(value.to_string());
        }
    }

    fn push_id(&mut self, id: i64) {
        self.push_value(&id.to_string());
    }

    /// An existing numeric target: canonical parsed form (`UPDATE`) or the
    /// target's original spelling (`DOWNLOAD`).
    fn push_numeric(&mut self, id: i64, raw: &str) {
        match self.variant {
            TagExpansionVariant::Update => self.push_id(id),
            TagExpansionVariant::Download => self.push_value(raw),
        }
    }

    /// `tag:<name>` / bare name: the tag's member ids, else the name itself.
    fn push_name_expansion(&mut self, name: &str, ids: Option<&[i64]>) {
        match ids {
            Some(ids) if !ids.is_empty() => {
                for id in ids {
                    self.push_id(*id);
                }
            }
            _ => match self.variant {
                TagExpansionVariant::Update => self.push_value(name),
                TagExpansionVariant::Download => {
                    self.seen.insert(name.to_string());
                    self.expanded.push(name.to_string());
                }
            },
        }
    }

    /// `^tag:<name>`: every id except the tag's members. An empty tag keeps
    /// the name (`UPDATE`) or expands to every id (`DOWNLOAD`).
    fn push_excluded_name(&mut self, name: &str, excluded: Option<&[i64]>, all_ids: &[i64]) {
        let excluded = excluded.unwrap_or_default();
        if excluded.is_empty() {
            match self.variant {
                TagExpansionVariant::Update => {
                    self.push_value(name);
                    return;
                }
                TagExpansionVariant::Download => {
                    for id in all_ids {
                        self.push_id(*id);
                    }
                    return;
                }
            }
        }
        let excluded: HashSet<i64> = excluded.iter().copied().collect();
        for id in all_ids {
            if !excluded.contains(id) {
                self.push_id(*id);
            }
        }
    }

    fn finish(self) -> Vec<String> {
        self.expanded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn record(id: i64, title: &str, toc_url: &str, ncode: Option<&str>, tags: &[&str]) -> NovelRecord {
        NovelRecord {
            id,
            author: "author".to_string(),
            title: title.to_string(),
            file_title: title.to_string(),
            toc_url: toc_url.to_string(),
            sitename: "小説家になろう".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 4, 20, 0, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            ncode: ncode.map(str::to_string),
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    struct FakeLookup {
        records: Vec<NovelRecord>,
    }

    impl FakeLookup {
        fn new(records: Vec<NovelRecord>) -> Self {
            Self { records }
        }
    }

    impl NovelLookup for FakeLookup {
        fn get(&self, id: NovelId) -> Result<Option<NovelRecord>> {
            Ok(self.records.iter().find(|r| r.id == id.0).cloned())
        }

        fn find_by_toc_url(&self, url: &str) -> Result<Option<NovelRecord>> {
            Ok(self.records.iter().find(|r| r.toc_url == url).cloned())
        }

        fn find_by_ncode(&self, ncode: &str) -> Result<Option<NovelRecord>> {
            Ok(self
                .records
                .iter()
                .find(|r| r.ncode.as_deref().is_some_and(|nc| nc.eq_ignore_ascii_case(ncode)))
                .cloned())
        }

        fn find_by_title(&self, title: &str) -> Result<Option<NovelRecord>> {
            Ok(self.records.iter().find(|r| r.title == title).cloned())
        }

        fn scan_ids(
            &self,
            filter: &NovelFilter,
            after_id: Option<NovelId>,
            limit: usize,
        ) -> Result<Vec<NovelId>> {
            let after = after_id.map(|id| id.0).unwrap_or(i64::MIN);
            let tag = filter.tag.as_deref();
            let mut ids: Vec<NovelId> = self
                .records
                .iter()
                .filter(|r| r.id > after)
                .filter(|r| tag.is_none_or(|tag| r.tags.iter().any(|t| t == tag)))
                .map(|r| NovelId(r.id))
                .collect();
            ids.sort();
            ids.truncate(limit);
            Ok(ids)
        }
    }

    struct FakeProvider {
        toc_url: Option<String>,
    }

    impl SiteDefinitionProvider for FakeProvider {
        fn resolve_toc_url(&self, _url: &str) -> Option<String> {
            self.toc_url.clone()
        }

        fn url_patterns_for_validation(&self) -> Vec<String> {
            Vec::new()
        }
    }

    fn no_aliases() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn title_only_and_title_then_ncode_differ_for_other_targets() {
        // `n29204764` はなろう式 ncode 判定 (`n\d+[a-z]+`) に当たらない
        // Pixiv 風 ncode なので `TargetType::Other` になる。
        let lookup = FakeLookup::new(vec![record(
            1,
            "sample title",
            "https://test.example/book/1",
            Some("n29204764"),
            &[],
        )]);
        let settings: Vec<SiteSetting> = Vec::new();

        assert_eq!(
            resolve_target_record_sync(
                TargetLookupPlan::CONVERT,
                &no_aliases(),
                UrlSource::Settings(&settings),
                "n29204764",
                &lookup,
            )
            .unwrap()
            .map(|r| r.id),
            Some(1)
        );
        assert_eq!(
            resolve_target_record_sync(
                TargetLookupPlan::DOWNLOAD_CHECK,
                &no_aliases(),
                UrlSource::Settings(&settings),
                "n29204764",
                &lookup,
            )
            .unwrap()
            .map(|r| r.id),
            None
        );
    }

    #[test]
    fn read_plan_uses_the_provider_and_raw_fallback_only_when_configured() {
        let lookup = FakeLookup::new(vec![record(
            1,
            "sample title",
            "https://test.example/raw/1",
            None,
            &[],
        )]);
        let target = "https://test.example/raw/1";

        // provider が解決できないとき: READ は None、raw fallback 付きは生 URL を引く。
        let missing = FakeProvider { toc_url: None };
        assert_eq!(
            resolve_target_record_sync(
                TargetLookupPlan::READ,
                &no_aliases(),
                UrlSource::Provider(&missing),
                target,
                &lookup,
            )
            .unwrap()
            .map(|r| r.id),
            None
        );
        assert_eq!(
            resolve_target_record_sync(
                TargetLookupPlan::READ_WITH_RAW_FALLBACK,
                &no_aliases(),
                UrlSource::Provider(&missing),
                target,
                &lookup,
            )
            .unwrap()
            .map(|r| r.id),
            Some(1)
        );

        // provider の解決結果が生 URL より優先される。
        let resolved = FakeProvider {
            toc_url: Some("https://test.example/book/1".to_string()),
        };
        assert_eq!(
            resolve_target_record_sync(
                TargetLookupPlan::READ_WITH_RAW_FALLBACK,
                &no_aliases(),
                UrlSource::Provider(&resolved),
                target,
                &lookup,
            )
            .unwrap()
            .map(|r| r.id),
            None
        );
    }

    #[test]
    fn tag_expansion_keeps_names_for_empty_tags_and_dedupes_ids() {
        let targets: Vec<String> = ["tag:fav", "tag:fav", "tag:missing", "^tag:bad", "42", "0"]
            .iter()
            .map(|target| target.to_string())
            .collect();
        let existing_numeric: HashSet<i64> = HashSet::from([42]);
        let tag_ids: HashMap<String, Vec<i64>> = HashMap::from([
            ("fav".to_string(), vec![1, 2]),
            ("bad".to_string(), Vec::new()),
        ]);
        let all_ids = vec![1, 2, 3, 42];

        assert!(
            !needs_all_ids(&targets, &tag_ids, TagExpansionVariant::Update),
            "^tag:bad は空なので UPDATE では全件は要らない"
        );
        assert_eq!(
            expand_tag_targets_from(
                &targets,
                &existing_numeric,
                &tag_ids,
                &all_ids,
                TagExpansionVariant::Update,
            ),
            vec!["1", "2", "missing", "bad", "42", "0"]
        );

        let with_members = vec!["^tag:fav".to_string()];
        assert!(needs_all_ids(&with_members, &tag_ids, TagExpansionVariant::Update));
        assert_eq!(
            expand_tag_targets_from(
                &with_members,
                &existing_numeric,
                &tag_ids,
                &all_ids,
                TagExpansionVariant::Update,
            ),
            vec!["3", "42"]
        );
    }

    #[test]
    fn tag_expansion_sync_driver_reads_members_and_numeric_existence() {
        let lookup = FakeLookup::new(vec![
            record(1, "one", "https://test.example/book/1", None, &["fav"]),
            record(2, "two", "https://test.example/book/2", None, &["fav"]),
            record(3, "three", "https://test.example/book/3", None, &[]),
        ]);
        let targets: Vec<String> = ["tag:fav", "^tag:fav", "3", "9"]
            .iter()
            .map(|target| target.to_string())
            .collect();

        assert_eq!(
            expand_tag_targets_sync(&targets, &lookup, TagExpansionVariant::Update, false),
            vec!["1", "2", "3", "9"]
        );
    }

    #[test]
    fn download_variant_expands_empty_exclude_and_keeps_numeric_spelling() {
        let targets: Vec<String> = ["^tag:bad", "007"]
            .iter()
            .map(|target| target.to_string())
            .collect();
        let existing_numeric: HashSet<i64> = HashSet::from([7]);
        let tag_ids: HashMap<String, Vec<i64>> =
            HashMap::from([("bad".to_string(), Vec::new())]);
        let all_ids = vec![7, 8];

        assert!(needs_all_ids(&targets, &tag_ids, TagExpansionVariant::Download));
        assert!(!needs_all_ids(&targets, &tag_ids, TagExpansionVariant::Update));
        assert_eq!(
            expand_tag_targets_from(
                &targets,
                &existing_numeric,
                &tag_ids,
                &all_ids,
                TagExpansionVariant::Download,
            ),
            vec!["7", "8", "007"]
        );
        assert_eq!(
            expand_tag_targets_from(
                &targets,
                &existing_numeric,
                &tag_ids,
                &all_ids,
                TagExpansionVariant::Update,
            ),
            vec!["bad", "7"]
        );
    }
}
