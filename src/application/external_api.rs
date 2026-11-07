//! Read-only external API over the novel library.
//! 小説ライブラリに対する読み取り専用の外部 API。
//!
//! 外部連携 (表の生成・自動化・MCP) 用の読み取り専用 API。Web UI の
//! `/api/list` と同じ一覧ロジック (検索・型付き絞り込み・並び替え) を共有し、
//! 出力形式は JSON / JSON Lines / XML から選べる。
//!
//! 移植可能層に置くため、HTTP フレームワークにもファイルシステムにも
//! 依存しない。認証 (トークン照合) は呼び出し側 (native の
//! `src/web/external.rs` と Worker の `worker_entry/src/webui/external.rs`)
//! が担う。ここが返すのは「どのデータを出すか」だけ。
//!
//! バーチャル小説の扱いは [`VirtualNovelMode`] で選ぶ:
//!
//! - [`VirtualNovelMode::Group`][] (既定): バーチャル小説を 1 行にまとめ、
//!   メンバーは出さない (Web UI と同じ)。
//! - [`VirtualNovelMode::Expand`][]: メンバーを個別の行として出す。
//! - [`VirtualNovelMode::Exclude`][]: バーチャル小説もメンバーも出さない
//!   (どのバーチャル小説にも入っていない元の小説だけ)。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::application::api_tokens::ApiTokenService;
use crate::application::attribute_tags;
use crate::application::error::ApplicationError;
use crate::application::library::LibraryService;
use crate::application::novel_content::NovelContentService;
use crate::application::settings::SettingsService;
use crate::db::NovelRecord;
use crate::downloader::{SectionFile, SubtitleInfo, TocObject};
use crate::hidden::HiddenState;
use crate::platform::repository::{FreezeReadMode, record_is_frozen};
use crate::platform::{
    NovelFilter, NovelId, NovelSort, NovelSortKey, PlatformFuture, PlatformService, SearchField,
    SearchTerm,
};
use crate::virtual_novel::VirtualNovelMembers;

/// Bearer トークンを必須にするかの設定名。
pub const REQUIRE_TOKEN_SETTING: &str = "external-api.require-token";

/// 一覧の既定ページ長。
pub const DEFAULT_LIST_LIMIT: usize = 100;

/// 一覧で要求できる最大件数。`limit=all` 相当は別の指定で無制限にできる。
pub const MAX_LIST_LIMIT: usize = 10_000;

/// Public HTTP error classification, independent of either runtime's response type.
pub fn http_error(error: &ApplicationError) -> (u16, &'static str) {
    match error {
        ApplicationError::InvalidRequest(_) => (400, "bad_request"),
        ApplicationError::NotFound(_) => (404, "not_found"),
        ApplicationError::Conflict(_) => (409, "conflict"),
        ApplicationError::Platform(_) => (500, "internal_error"),
    }
}

/// 出力形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// 1 つの JSON ドキュメント。
    #[default]
    Json,
    /// 1 行 1 レコードの JSON Lines。
    Jsonl,
    /// XML。
    Xml,
}

impl OutputFormat {
    /// 形式名 (`json` / `jsonl` / `xml`) を解釈する。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "json" => Some(Self::Json),
            "jsonl" | "ndjson" | "jsonlines" => Some(Self::Jsonl),
            "xml" => Some(Self::Xml),
            _ => None,
        }
    }

    /// 応答の `Content-Type`。
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Json => "application/json; charset=utf-8",
            Self::Jsonl => "application/x-ndjson; charset=utf-8",
            Self::Xml => "application/xml; charset=utf-8",
        }
    }
}

/// バーチャル小説を一覧でどう見せるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VirtualNovelMode {
    /// バーチャル小説を 1 行にまとめ、メンバーは出さない (既定)。
    #[default]
    Group,
    /// メンバーを個別の行として出す (バーチャル小説の行は出さない)。
    Expand,
    /// バーチャル小説もメンバーも出さない。
    Exclude,
}

impl VirtualNovelMode {
    /// モード名 (`group` / `expand` / `exclude`) を解釈する。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "group" | "grouped" | "default" => Some(Self::Group),
            "expand" | "expanded" | "members" => Some(Self::Expand),
            "exclude" | "excluded" | "none" => Some(Self::Exclude),
            _ => None,
        }
    }

    /// モード名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Expand => "expand",
            Self::Exclude => "exclude",
        }
    }
}

/// 一覧の絞り込み条件。
///
/// `search` は Web UI と同じ自由検索 (`tag:` / `author:` / `site:` /
/// `title:` / `status:`、`-` での否定、`a|b` の OR)。残りは型付きの条件。
#[derive(Debug, Clone, Default)]
pub struct ExternalListRequest {
    /// 自由検索。
    pub search: Option<String>,
    /// 題名の部分一致。
    pub title: Option<String>,
    /// 作者名の部分一致。
    pub author: Option<String>,
    /// サイト名 (完全一致)。
    pub site: Option<String>,
    /// タグ (完全一致)。
    pub tag: Option<String>,
    /// ncode (完全一致)。
    pub ncode: Option<String>,
    /// 属性タグ (`小説` / `漫画`)。
    pub attribute: Option<String>,
    /// 種別 (0 / 1 = 連載、2 = 短編)。
    pub novel_type: Option<u8>,
    /// 完結かどうか。
    pub end: Option<bool>,
    /// 中断かどうか。
    pub suspend: Option<bool>,
    /// 凍結かどうか。
    pub frozen: Option<bool>,
    /// 非表示設定に従うかどうか (既定は従う = 非表示作品を出さない)。
    pub include_hidden: bool,
    /// バーチャル小説の扱い。
    pub virtual_mode: VirtualNovelMode,
    /// 並び順のキー。
    pub sort: Option<NovelSortKey>,
    /// 降順かどうか。
    pub descending: bool,
    /// 開始位置。
    pub offset: usize,
    /// 件数上限。`None` は無制限。
    pub limit: Option<usize>,
    /// 出力形式。
    pub format: OutputFormat,
}

impl ExternalListRequest {
    /// クエリパラメータ (または MCP の引数) から条件を作る。
    ///
    /// 値が配列や数値でも同じ解析を通るよう、値はいったん文字列に寄せる。
    pub fn from_pairs(pairs: &[(String, String)]) -> Result<Self, ApplicationError> {
        // 既定は [`DEFAULT_LIST_LIMIT`] 件。`limit=all` / `limit=0` で無制限。
        let mut request = Self {
            limit: Some(DEFAULT_LIST_LIMIT),
            ..Self::default()
        };
        for (key, value) in pairs {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "format" => {
                    request.format = parse_format(value)?;
                }
                "q" | "search" | "query" => request.search = non_empty(value),
                "title" => request.title = non_empty(value),
                "author" => request.author = non_empty(value),
                "site" | "sitename" => request.site = non_empty(value),
                "tag" => request.tag = non_empty(value),
                "ncode" => request.ncode = non_empty(value),
                "attribute" | "attribute_tag" => request.attribute = non_empty(value),
                "novel_type" | "type" => {
                    request.novel_type = Some(parse_novel_type(value)?);
                }
                "end" => request.end = Some(parse_bool(value, "end")?),
                "suspend" => request.suspend = Some(parse_bool(value, "suspend")?),
                "frozen" => request.frozen = Some(parse_bool(value, "frozen")?),
                "include_hidden" | "show_hidden" => {
                    request.include_hidden = parse_bool(value, "include_hidden")?;
                }
                "virtual" | "virtual_novels" => {
                    request.virtual_mode = VirtualNovelMode::parse(value).ok_or_else(|| {
                        invalid(format!(
                            "virtual は group / expand / exclude のいずれかです: {value}"
                        ))
                    })?;
                }
                "sort" | "sort_by" => {
                    request.sort = Some(NovelSortKey::from_db_key(value).ok_or_else(|| {
                        invalid(format!("sort は正しいキーではありません: {value}"))
                    })?);
                }
                "order" | "direction" => {
                    request.descending = match value.to_ascii_lowercase().as_str() {
                        "asc" | "ascending" | "up" => false,
                        "desc" | "descending" | "down" => true,
                        _ => return Err(invalid(format!("order は asc / desc のいずれかです: {value}"))),
                    };
                }
                "offset" | "start" => {
                    request.offset = parse_offset(value)?;
                }
                "limit" | "length" => {
                    request.limit = parse_limit(value)?;
                }
                _ => {}
            }
        }
        Ok(request)
    }

    /// 一覧の絞り込み句_filters を組み立てる。
    fn build_filter(
        &self,
        frozen_ids: &std::collections::HashSet<i64>,
        members: &VirtualNovelMembers,
        attribute_tags_enabled: bool,
    ) -> NovelFilter {
        let mut filter = NovelFilter {
            frozen_ids: Some(frozen_ids.clone()),
            ..NovelFilter::default()
        };

        if let Some(title) = &self.title {
            filter.terms.push(SearchTerm::new(
                SearchField::Title,
                false,
                vec![title.to_lowercase()],
            ));
        }
        if let Some(author) = &self.author {
            filter.terms.push(SearchTerm::new(
                SearchField::Author,
                false,
                vec![author.to_lowercase()],
            ));
        }
        if let Some(site) = &self.site {
            filter.site = Some(site.clone());
        }
        if let Some(tag) = &self.tag {
            filter.tag = Some(tag.clone());
        }
        if let Some(ncode) = &self.ncode {
            filter.ncode = Some(ncode.clone());
        }
        if let Some(attribute) = &self.attribute {
            // 属性タグは専用列にあるため、タグ検索の語句として指定する。
            filter.terms.push(SearchTerm::new(
                SearchField::Tag,
                false,
                vec![attribute.to_lowercase()],
            ));
        }
        if let Some(novel_type) = self.novel_type {
            filter.novel_type = Some(novel_type);
        }
        if let Some(end) = self.end {
            filter.end = Some(end);
        }
        if let Some(suspend) = self.suspend {
            filter.suspend = Some(suspend);
        }
        if let Some(frozen) = self.frozen {
            filter.terms.push(SearchTerm::new(
                SearchField::Status,
                !frozen,
                vec!["凍結".to_string()],
            ));
        }
        filter.attribute_tags_enabled = attribute_tags_enabled || self.attribute.is_some();

        // バーチャル小説の扱い。メンバーを除外する ID は hidden の除外 ID
        // と合わせるため、この後で足す。
        match self.virtual_mode {
            VirtualNovelMode::Group => {
                filter.exclude_ids.extend(members.all_member_ids());
            }
            VirtualNovelMode::Expand => {
                filter.exclude_ids.extend(members.groups.keys().copied());
            }
            VirtualNovelMode::Exclude => {
                filter.exclude_ids.extend(members.groups.keys().copied());
                filter.exclude_ids.extend(members.all_member_ids());
            }
        }
        filter
    }
}

/// バーチャル小説の所属を読み出すポート。
///
/// native は在庫 (`virtual_novel_members`)、Worker は D1 の `app_state` から
/// 読む。どちらも同じ [`VirtualNovelMembers`] を返す。
pub trait VirtualNovelDirectory: PlatformService {
    /// バーチャル小説 ID -> メンバー ID の対応。
    fn members(&self) -> PlatformFuture<'_, crate::error::Result<VirtualNovelMembers>>;
}

/// バーチャル小説を 1 つも持たないディレクトリ (テストとモック用)。
#[derive(Debug, Default)]
pub struct EmptyVirtualNovelDirectory;

impl VirtualNovelDirectory for EmptyVirtualNovelDirectory {
    fn members(&self) -> PlatformFuture<'_, crate::error::Result<VirtualNovelMembers>> {
        Box::pin(async move { Ok(VirtualNovelMembers::default()) })
    }
}

/// 外部 API の設定情報 (Web UI と CLI から確認する)。
#[derive(Debug, Clone, Serialize)]
pub struct ExternalApiInfo {
    /// Bearer トークンを必須にするか。
    pub require_token: bool,
    /// 外部から見た API の URL。
    pub base_url: Option<String>,
    /// 保存されているトークン。
    pub tokens: Vec<crate::application::api_tokens::ApiTokenInfo>,
    /// Cloudflare Access の Client Id (Worker のみ)。
    pub access_client_id: Option<String>,
    /// Cloudflare Access の Client Secret (Worker のみ)。
    pub access_client_secret: Option<String>,
}

/// 外部 API のサービス本体。
pub struct ExternalApiService {
    library: Arc<LibraryService>,
    content: Arc<NovelContentService>,
    settings: Arc<SettingsService>,
    virtual_novels: Arc<dyn VirtualNovelDirectory>,
    api_tokens: Arc<ApiTokenService>,
}

impl ExternalApiService {
    pub fn new(
        library: Arc<LibraryService>,
        content: Arc<NovelContentService>,
        settings: Arc<SettingsService>,
        virtual_novels: Arc<dyn VirtualNovelDirectory>,
        api_tokens: Arc<ApiTokenService>,
    ) -> Self {
        Self {
            library,
            content,
            settings,
            virtual_novels,
            api_tokens,
        }
    }

    /// Bearer トークンを必須にするか。
    pub async fn require_token(&self) -> bool {
        self.settings
            .get(REQUIRE_TOKEN_SETTING)
            .await
            .ok()
            .flatten()
            .and_then(|value| crate::setting_core::setting_bool_value(&value))
            .unwrap_or(true)
    }

    /// 外部 API の設定情報。
    ///
    /// `base_url` と Cloudflare Access の資格情報は環境側で埋める (native
    /// なら待ち受けポート、Worker ならリクエストの URL と secret)。
    pub async fn info(
        &self,
        base_url: Option<String>,
        access_client_id: Option<String>,
        access_client_secret: Option<String>,
    ) -> Result<ExternalApiInfo, ApplicationError> {
        let require_token = self
            .settings
            .get(REQUIRE_TOKEN_SETTING)
            .await
            .ok()
            .flatten()
            .and_then(|value| crate::setting_core::setting_bool_value(&value))
            .unwrap_or(true);
        let tokens = self
            .api_tokens
            .infos()
            .await
            .map_err(ApplicationError::platform)?;
        Ok(ExternalApiInfo {
            require_token,
            base_url,
            tokens,
            access_client_id,
            access_client_secret,
        })
    }

    /// 一覧を返す。
    pub async fn list(
        &self,
        request: &ExternalListRequest,
        hidden: Option<HiddenState>,
    ) -> Result<ExternalList, ApplicationError> {
        let members = self
            .virtual_novels
            .members()
            .await
            .map_err(ApplicationError::platform)?;
        let frozen_ids = self.library.frozen_ids().await?;
        let attribute_tags_enabled = attribute_tags::enabled(&self.settings).await;

        let mut filter = request.build_filter(&frozen_ids, &members, attribute_tags_enabled);
        // hidden の除外 ID は上書きで入るので、バーチャル小説の除外より先に
        // 適用してから足す (順序を逆にすると除外 ID が消える)。
        if !request.include_hidden
            && let Some(hidden) = &hidden
        {
            let mut hidden_filter = NovelFilter::default();
            hidden.apply_to_filter(&mut hidden_filter);
            filter.exclude_ids.extend(hidden_filter.exclude_ids);
            filter.exclude_tags.extend(hidden_filter.exclude_tags);
            filter.exclude_authors.extend(hidden_filter.exclude_authors);
            filter.exclude_sites.extend(hidden_filter.exclude_sites);
        }
        // 自由検索は型付き条件と同じ AND 条件で足す。
        if let Some(search) = request.search.as_deref() {
            let search_filter = self.library.build_filter_with_ids(Some(search), &frozen_ids);
            filter.terms.extend(search_filter.terms);
        }

        let sort = NovelSort {
            key: request.sort.unwrap_or(NovelSortKey::Id),
            reverse: request.descending,
        };
        // 無制限 (`limit=all` / `limit=0`) は `usize::MAX` を渡す。
        // ページングの切り出しはリポジトリ側の責務 (native は
        // `skip(offset).take(limit)`、D1 は SQL の LIMIT/OFFSET)。
        let (records_total, records_filtered, records) = self
            .library
            .list_records(filter, sort, request.offset, request.limit)
            .await?;

        let related_titles = self.related_titles(&members).await;
        let mut novels: Vec<ExternalNovel> = records
            .iter()
            .map(|record| {
                let owner = members.owner_of(record.id);
                let parent_title = owner.and_then(|owner_id| related_titles.get(&owner_id).cloned());
                self.novel_row(
                    record,
                    &frozen_ids,
                    owner,
                    parent_title,
                    attribute_tags_enabled,
                )
            })
            .collect();
        // バーチャル小説の行には、まとめられているメンバーを載せる。
        for novel in &mut novels {
            let Some(member_ids) = members.get(novel.id) else {
                continue;
            };
            let member_ids = member_ids.to_vec();
            novel.virtual_novel = Some(self.virtual_novel_info(
                novel.id,
                &records,
                &member_ids,
                &related_titles,
            ));
        }

        Ok(ExternalList {
            records_total,
            records_filtered,
            offset: request.offset,
            limit: request.limit,
            novels,
        })
    }

    /// 1 件の作品情報。
    pub async fn novel(&self, id: i64) -> Result<ExternalNovel, ApplicationError> {
        let members = self
            .virtual_novels
            .members()
            .await
            .map_err(ApplicationError::platform)?;
        let frozen_ids = self.library.frozen_ids().await?;
        let attribute_tags_enabled = attribute_tags::enabled(&self.settings).await;
        let record = self
            .library
            .get(NovelId(id))
            .await?
            .ok_or_else(|| ApplicationError::NotFound(format!("作品が見つかりません: {id}")))?;
        // 親バーチャル小説の題名だけを読む (全メンバーの記録は読まない)。
        let owner = members.owner_of(id);
        let parent_title = self.parent_title(owner).await;
        Ok(self.novel_row(
            &record,
            &frozen_ids,
            owner,
            parent_title,
            attribute_tags_enabled,
        ))
    }

    /// 目次。バーチャル小説はメンバーの目次を連結する。
    pub async fn toc(&self, id: i64) -> Result<ExternalToc, ApplicationError> {
        let members = self
            .virtual_novels
            .members()
            .await
            .map_err(ApplicationError::platform)?;
        let record = self
            .library
            .get(NovelId(id))
            .await?
            .ok_or_else(|| ApplicationError::NotFound(format!("作品が見つかりません: {id}")))?;

        let member_ids = members.get(id).map(<[i64]>::to_vec).unwrap_or_default();
        // このバーチャル小説のメンバーの題名だけを読む。
        let member_titles = self.titles_of(&member_ids).await;
        let virtual_novel = if member_ids.is_empty() {
            None
        } else {
            Some(ExternalVirtualNovel {
                id,
                title: record.title.clone(),
                members: member_ids
                    .iter()
                    .map(|member_id| ExternalVirtualMember {
                        id: *member_id,
                        title: member_titles.get(member_id).cloned().unwrap_or_default(),
                    })
                    .collect(),
            })
        };

        let mut sections = Vec::new();
        let mut stories = Vec::new();
        // バーチャル小説の目次はメンバーの目次を連結する。識別子は
        // `メンバーID:話番号` になる (通常作品の話番号だけと区別するため)。
        let member = !member_ids.is_empty();
        let owners: Vec<i64> = if member_ids.is_empty() {
            vec![id]
        } else {
            member_ids.clone()
        };
        for owner in owners {
            let toc = self.read_toc(owner).await?;
            if let Some(story) = toc.story.clone() {
                stories.push(story);
            }
            sections.extend(sections_of(&toc, owner, member));
        }

        Ok(ExternalToc {
            id,
            title: record.title,
            author: record.author,
            toc_url: record.toc_url,
            story: (!stories.is_empty()).then(|| stories.join("\n\n")),
            virtual_novel,
            sections,
        })
    }

    /// 1 話の本文。
    ///
    /// `key` は目次が返す識別子。通常の作品では話番号 (`"3"`)、
    /// バーチャル小説では `メンバーID:話番号` (`"12:3"`)。
    pub async fn section(&self, id: i64, key: &str) -> Result<ExternalSection, ApplicationError> {
        let members = self
            .virtual_novels
            .members()
            .await
            .map_err(ApplicationError::platform)?;
        self.library
            .get(NovelId(id))
            .await?
            .ok_or_else(|| ApplicationError::NotFound(format!("作品が見つかりません: {id}")))?;
        let member_ids = members.get(id).map(<[i64]>::to_vec).unwrap_or_default();
        let member = !member_ids.is_empty();
        let owners: Vec<i64> = if member_ids.is_empty() {
            vec![id]
        } else {
            member_ids.clone()
        };

        // 話の実体を探す。`メンバーID:話番号` ならそのメンバーの目次だけを
        // 読む (他のメンバーの目次は読み直さない)。
        let (owner, index) = resolve_section_owner(&owners, key)
            .ok_or_else(|| section_not_found(id, key))?;
        let toc = self.read_toc(owner).await?;
        let subtitle = toc
            .subtitles
            .iter()
            .find(|subtitle| subtitle.index == key)
            .or_else(|| toc.subtitles.iter().find(|subtitle| subtitle.index == index))
            .ok_or_else(|| section_not_found(id, key))?;

        self.read_section(id, owner, member, subtitle).await
    }

    /// 作品 1 件の全話の本文 (目次の順)。
    ///
    /// 目次は所有者ごとに 1 回だけ読む (話ごとに目次を読み直さない)。
    /// 欠落・破損した話があれば失敗し、不完全な本を成功として返さない。
    pub async fn sections(&self, id: i64) -> Result<ExternalSections, ApplicationError> {
        let members = self
            .virtual_novels
            .members()
            .await
            .map_err(ApplicationError::platform)?;
        self.library
            .get(NovelId(id))
            .await?
            .ok_or_else(|| ApplicationError::NotFound(format!("作品が見つかりません: {id}")))?;
        let member_ids = members.get(id).map(<[i64]>::to_vec).unwrap_or_default();
        let member = !member_ids.is_empty();
        let owners: Vec<i64> = if member_ids.is_empty() {
            vec![id]
        } else {
            member_ids.clone()
        };

        let mut sections = Vec::new();
        for owner in owners {
            let toc = self.read_toc(owner).await?;
            for subtitle in &toc.subtitles {
                sections.push(self.read_section(id, owner, member, subtitle).await?);
            }
        }
        Ok(ExternalSections {
            id,
            sections,
        })
    }

    /// 1 話の本文を読む。
    async fn read_section(
        &self,
        id: i64,
        owner: i64,
        member: bool,
        subtitle: &SubtitleInfo,
    ) -> Result<ExternalSection, ApplicationError> {
        let bytes = self
            .content
            .section(NovelId(owner), &subtitle.index, &subtitle.file_subtitle)
            .await?
            .ok_or_else(|| {
                ApplicationError::NotFound(format!("本文が見つかりません: {owner} / {}", subtitle.index))
            })?;
        let section: SectionFile = serde_yaml::from_slice(&bytes)
            .map_err(|error| ApplicationError::Platform(format!("本文を読めません: {error}")))?;

        Ok(ExternalSection {
            id,
            key: section_key(owner, &subtitle.index, member),
            index: subtitle.index.clone(),
            member_id: owner,
            chapter: subtitle.chapter.clone(),
            subchapter: subtitle.subchapter.clone(),
            subtitle: subtitle.subtitle.clone(),
            href: subtitle.href.clone(),
            data_type: section.element.data_type.clone(),
            body: section.element.body.clone(),
            introduction: section.element.introduction.clone(),
            postscript: section.element.postscript.clone(),
        })
    }

    /// タグの一覧 (名前と使用数)。
    pub async fn tags(&self) -> Result<Vec<ExternalTag>, ApplicationError> {
        let records = self.library.records().await?;
        let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for record in records {
            for tag in record.tags {
                if crate::virtual_novel::is_internal_tag(&tag) {
                    continue;
                }
                *counts.entry(tag).or_insert(0) += 1;
            }
        }
        Ok(counts
            .into_iter()
            .map(|(name, count)| ExternalTag { name, count })
            .collect())
    }

    async fn read_toc(&self, id: i64) -> Result<TocObject, ApplicationError> {
        let bytes = self
            .content
            .toc(NovelId(id))
            .await?
            .ok_or_else(|| ApplicationError::NotFound(format!("目次がありません: {id}")))?;
        serde_yaml::from_slice(&bytes)
            .map_err(|error| ApplicationError::Platform(format!("目次を読めません: {error}")))
    }

    /// バーチャル小説と、そのメンバーの題名 (作品 ID -> 題名)。
    async fn related_titles(
        &self,
        members: &VirtualNovelMembers,
    ) -> std::collections::HashMap<i64, String> {
        let mut ids: Vec<i64> = members.all_member_ids().into_iter().collect();
        ids.extend(members.groups.keys().copied());
        if ids.is_empty() {
            return std::collections::HashMap::new();
        }
        match self.library.records_by_ids(&ids).await {
            Ok(records) => records
                .into_iter()
                .map(|record| (record.id, record.title))
                .collect(),
            Err(_) => std::collections::HashMap::new(),
        }
    }

    /// 親バーチャル小説の題名 (メンバーでなければ `None`)。
    ///
    /// 1 件の作品情報では親 1 件分だけ読む (全メンバーの記録は読まない)。
    async fn parent_title(
        &self,
        owner: Option<i64>,
    ) -> Option<String> {
        let owner = owner?;
        match self.library.get(NovelId(owner)).await {
            Ok(Some(record)) => Some(record.title),
            _ => None,
        }
    }

    /// 指定した作品の題名 (作品 ID -> 題名)。
    async fn titles_of(&self, ids: &[i64]) -> std::collections::HashMap<i64, String> {
        if ids.is_empty() {
            return std::collections::HashMap::new();
        }
        match self.library.records_by_ids(ids).await {
            Ok(records) => records
                .into_iter()
                .map(|record| (record.id, record.title))
                .collect(),
            Err(_) => std::collections::HashMap::new(),
        }
    }

    fn virtual_novel_info(
        &self,
        id: i64,
        records: &[NovelRecord],
        member_ids: &[i64],
        related_titles: &std::collections::HashMap<i64, String>,
    ) -> ExternalVirtualNovel {
        let title = records
            .iter()
            .find(|record| record.id == id)
            .map(|record| record.title.clone())
            .unwrap_or_default();
        ExternalVirtualNovel {
            id,
            title,
            members: member_ids
                .iter()
                .map(|member_id| ExternalVirtualMember {
                    id: *member_id,
                    title: related_titles.get(member_id).cloned().unwrap_or_default(),
                })
                .collect(),
        }
    }

    fn novel_row(
        &self,
        record: &NovelRecord,
        frozen_ids: &std::collections::HashSet<i64>,
        owner: Option<i64>,
        parent_title: Option<String>,
        attribute_tags_enabled: bool,
    ) -> ExternalNovel {
        let frozen = record_is_frozen(record, Some(frozen_ids), FreezeReadMode::InventoryOrTag);
        let mut tags: Vec<String> = record
            .tags
            .iter()
            .filter(|tag| !crate::virtual_novel::is_internal_tag(tag))
            .cloned()
            .collect();
        let attribute_tag = record.attribute_tag().map(str::to_owned);
        if attribute_tags_enabled
            && let Some(attribute) = &attribute_tag
        {
            tags.push(attribute.clone());
        }
        ExternalNovel {
            id: record.id,
            title: record.title.clone(),
            author: record.author.clone(),
            sitename: record.sitename.clone(),
            domain: record.domain.clone(),
            ncode: record.ncode.clone(),
            toc_url: record.toc_url.clone(),
            display_url: record.display_url().to_string(),
            novel_type: record.novel_type,
            end: record.end,
            suspend: record.suspend,
            is_narou: record.is_narou,
            length: record.length,
            general_all_no: record.general_all_no,
            tags,
            attribute_tag,
            last_update: record.last_update,
            general_lastup: record.general_lastup,
            new_arrivals_date: record.new_arrivals_date,
            last_check_date: record.last_check_date,
            frozen,
            status: status_text(record, frozen),
            virtual_novel: None,
            virtual_parent_id: owner,
            virtual_parent_title: parent_title,
        }
    }
}

/// 1 行分の作品情報。
#[derive(Debug, Clone, Serialize)]
pub struct ExternalNovel {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub sitename: String,
    pub domain: Option<String>,
    pub ncode: Option<String>,
    pub toc_url: String,
    pub display_url: String,
    pub novel_type: u8,
    pub end: bool,
    pub suspend: bool,
    pub is_narou: bool,
    pub length: Option<i64>,
    pub general_all_no: Option<i64>,
    pub tags: Vec<String>,
    pub attribute_tag: Option<String>,
    pub last_update: DateTime<Utc>,
    pub general_lastup: Option<DateTime<Utc>>,
    pub new_arrivals_date: Option<DateTime<Utc>>,
    pub last_check_date: Option<DateTime<Utc>>,
    pub frozen: bool,
    pub status: String,
    /// バーチャル小説の行なら、その構成。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub virtual_novel: Option<ExternalVirtualNovel>,
    /// バーチャル小説にまとめられている元の作品なら、その親 ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub virtual_parent_id: Option<i64>,
    /// 親バーチャル小説の題名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub virtual_parent_title: Option<String>,
}

/// バーチャル小説の情報。
#[derive(Debug, Clone, Serialize)]
pub struct ExternalVirtualNovel {
    pub id: i64,
    pub title: String,
    pub members: Vec<ExternalVirtualMember>,
}

/// バーチャル小説のメンバー。
#[derive(Debug, Clone, Serialize)]
pub struct ExternalVirtualMember {
    pub id: i64,
    pub title: String,
}

/// 一覧の応答。
#[derive(Debug, Clone)]
pub struct ExternalList {
    pub records_total: u64,
    pub records_filtered: u64,
    pub offset: usize,
    pub limit: Option<usize>,
    pub novels: Vec<ExternalNovel>,
}

impl ExternalList {
    /// JSON 用の文書。
    pub fn to_json(&self) -> Value {
        json!({
            "records_total": self.records_total,
            "records_filtered": self.records_filtered,
            "offset": self.offset,
            "limit": self.limit,
            "data": self.novels,
        })
    }
}

/// 目次。
#[derive(Debug, Clone, Serialize)]
pub struct ExternalToc {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub toc_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub story: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub virtual_novel: Option<ExternalVirtualNovel>,
    pub sections: Vec<ExternalTocSection>,
}

/// 目次の 1 話。
#[derive(Debug, Clone, Serialize)]
pub struct ExternalTocSection {
    /// 本文取得に使う識別子。通常の作品では話番号、バーチャル小説では
    /// `メンバーID:話番号`。
    pub key: String,
    /// 話番号。
    pub index: String,
    /// 属する作品 ID (バーチャル小説のメンバーならそのメンバー)。
    pub member_id: i64,
    pub chapter: String,
    pub subchapter: String,
    pub subtitle: String,
    pub file_subtitle: String,
    pub href: String,
    pub subdate: String,
    pub subupdate: Option<String>,
    pub download_time: Option<String>,
}

/// 1 話の本文。
#[derive(Debug, Clone, Serialize)]
pub struct ExternalSection {
    pub id: i64,
    pub key: String,
    pub index: String,
    pub member_id: i64,
    pub chapter: String,
    pub subchapter: String,
    pub subtitle: String,
    pub href: String,
    pub data_type: String,
    pub body: String,
    pub introduction: String,
    pub postscript: String,
}

/// 作品 1 件の全話の本文。
#[derive(Debug, Clone, Serialize)]
pub struct ExternalSections {
    /// 作品 ID (バーチャル小説ならそのバーチャル小説)。
    pub id: i64,
    /// 全話の本文 (目次の順)。
    pub sections: Vec<ExternalSection>,
}

/// タグと使用数。
#[derive(Debug, Clone, Serialize)]
pub struct ExternalTag {
    pub name: String,
    pub count: usize,
}

/// 描画の指定。
pub struct RenderOptions<'a> {
    pub format: OutputFormat,
    /// XML の根要素名。
    pub root: &'a str,
    /// 配列要素の子要素名 (JSON キー -> XML 要素名)。
    pub array_names: &'a [(&'a str, &'a str)],
    /// XML で「応答文書が配列」のときの要素名。
    ///
    /// 応答文書が配列のとき、XML はルート要素を 1 つだけ出し、その中に
    /// この名前の要素を並べる (XML 文書として 1 つのルート要素に収める)。
    pub xml_item: &'a str,
    /// JSON Lines で 1 行ずつ出力する項目。
    pub jsonl_items: Option<&'a [Value]>,
}

/// 応答本文を JSON / JSON Lines / XML で描画する。
pub fn render(value: &Value, options: &RenderOptions<'_>) -> String {
    match options.format {
        OutputFormat::Json => serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string()),
        OutputFormat::Jsonl => {
            let mut out = String::new();
            match options.jsonl_items {
                Some(items) => {
                    for item in items {
                        out.push_str(&serde_json::to_string(item).unwrap_or_default());
                        out.push('\n');
                    }
                }
                None => {
                    out.push_str(&serde_json::to_string(value).unwrap_or_default());
                    out.push('\n');
                }
            }
            out
        }
        OutputFormat::Xml => {
            let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
            match value {
                // ルート要素は 1 つだけ。配列のときは `xml_item` の要素を
                // 並べて、XML 文書として正しくする (ルート要素を並べるのは
                // XML として不正)。
                Value::Array(items) => {
                    out.push_str(&xml_indent(0));
                    out.push('<');
                    out.push_str(options.root);
                    out.push_str(">\n");
                    for item in items {
                        json_to_xml(item, options.xml_item, options.array_names, 1, &mut out);
                    }
                    out.push_str(&xml_indent(0));
                    out.push_str("</");
                    out.push_str(options.root);
                    out.push_str(">\n");
                }
                other => json_to_xml(other, options.root, options.array_names, 0, &mut out),
            }
            out
        }
    }
}

/// JSON 値を XML 要素へ変換する。
fn json_to_xml(
    value: &Value,
    element: &str,
    array_names: &[(&str, &str)],
    depth: usize,
    out: &mut String,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                json_to_xml(item, element, array_names, depth, out);
            }
        }
        Value::Object(map) => {
            out.push_str(&xml_indent(depth));
            out.push('<');
            out.push_str(element);
            out.push_str(">\n");
            for (key, child) in map {
                match child {
                    Value::Array(items) => {
                        let name = array_names
                            .iter()
                            .find(|(candidate, _)| *candidate == key)
                            .map(|(_, name)| *name)
                            .unwrap_or(key.as_str());
                        for item in items {
                            json_to_xml(item, name, array_names, depth + 1, out);
                        }
                    }
                    Value::Object(_) => json_to_xml(child, key, array_names, depth + 1, out),
                    _ => {
                        out.push_str(&xml_indent(depth + 1));
                        out.push('<');
                        out.push_str(key);
                        out.push('>');
                        out.push_str(&xml_text(child));
                        out.push_str("</");
                        out.push_str(key);
                        out.push_str(">\n");
                    }
                }
            }
            out.push_str(&xml_indent(depth));
            out.push_str("</");
            out.push_str(element);
            out.push_str(">\n");
        }
        scalar => {
            out.push_str(&xml_indent(depth));
            out.push('<');
            out.push_str(element);
            out.push('>');
            out.push_str(&xml_text(scalar));
            out.push_str("</");
            out.push_str(element);
            out.push_str(">\n");
        }
    }
}

fn xml_indent(depth: usize) -> String {
    "  ".repeat(depth)
}

fn xml_text(value: &Value) -> String {
    match value {
        Value::String(text) => xml_escape(text),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 目次からセクションの要素を 1 行ずつ作る。
fn sections_of(toc: &TocObject, owner: i64, member: bool) -> Vec<ExternalTocSection> {
    toc.subtitles
        .iter()
        .map(|subtitle| ExternalTocSection {
            key: section_key(owner, &subtitle.index, member),
            index: subtitle.index.clone(),
            member_id: owner,
            chapter: subtitle.chapter.clone(),
            subchapter: subtitle.subchapter.clone(),
            subtitle: subtitle.subtitle.clone(),
            file_subtitle: subtitle.file_subtitle.clone(),
            href: subtitle.href.clone(),
            subdate: subtitle.subdate.clone(),
            subupdate: subtitle.subupdate.clone(),
            download_time: subtitle.download_time.clone(),
        })
        .collect()
}

/// 本文取得に使う識別子。
///
/// 通常作品の話番号だけ、バーチャル小説のメンバーは `メンバーID:話番号`。
fn section_key(owner: i64, index: &str, member: bool) -> String {
    if member {
        format!("{owner}:{index}")
    } else {
        index.to_string()
    }
}

/// 話の識別子から (作品 ID, 話番号) を解決する。
///
/// `メンバーID:話番号` ならそのメンバーが属する作品、話番号だけなら
/// 所有者が 1 件のときだけその作品を指す (バーチャル小説が複数メンバー
/// のときは `メンバーID:話番号` で指定する)。
fn resolve_section_owner(owners: &[i64], key: &str) -> Option<(i64, String)> {
    if let Some((owner_text, index)) = key.split_once(':')
        && let Ok(owner) = owner_text.parse::<i64>()
        && owners.contains(&owner)
    {
        return Some((owner, index.to_string()));
    }
    if owners.len() == 1 {
        return Some((owners[0], key.to_string()));
    }
    None
}

/// 話が見つからないときのエラー。
fn section_not_found(id: i64, key: &str) -> ApplicationError {
    ApplicationError::NotFound(format!(
        "話が見つかりません: {id} / {key} (バーチャル小説は メンバーID:話番号 で指定します)"
    ))
}

/// 状態文字列 (Web UI の `record_status_text` と同じ)。
fn status_text(record: &NovelRecord, frozen: bool) -> String {
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
    status.join(", ")
}

fn invalid(message: String) -> ApplicationError {
    ApplicationError::InvalidRequest(message)
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_bool(value: &str, name: &str) -> Result<bool, ApplicationError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(invalid(format!("{name} は true / false のいずれかです: {value}"))),
    }
}

fn parse_number(value: &str, name: &str) -> Result<i64, ApplicationError> {
    value
        .parse::<i64>()
        .map_err(|_| invalid(format!("{name} は数値です: {value}")))
}

/// 種別 (0 / 1 = 連載、2 = 短編)。
///
/// 範囲外は 400 にする。`u8` へ unchecked 変換すると 256 が 0 になり、
/// 「種別 0」の作品だけを返してしまう。
fn parse_novel_type(value: &str) -> Result<u8, ApplicationError> {
    let number = parse_number(value, "novel_type")?;
    u8::try_from(number)
        .map_err(|_| invalid(format!("novel_type は 0 から 255 の数値です: {value}")))
}

/// 開始位置。
///
/// 負の値は 400 にする (符号なしへ変換すると巨大な値に wrap して、
/// 常に空のページになる)。
fn parse_offset(value: &str) -> Result<usize, ApplicationError> {
    let number = parse_number(value, "offset")?;
    usize::try_from(number)
        .map_err(|_| invalid(format!("offset は 0 以上の数値です: {value}")))
}

/// 出力形式を解釈する (不正なら 400 のエラー)。
///
/// 一覧以外でも同じ規則を使う (黙って JSON へ落とさない)。
pub fn parse_format(value: &str) -> Result<OutputFormat, ApplicationError> {
    OutputFormat::parse(value)
        .ok_or_else(|| invalid(format!("format は json / jsonl / xml のいずれかです: {value}")))
}

fn parse_limit(value: &str) -> Result<Option<usize>, ApplicationError> {
    if value.eq_ignore_ascii_case("all") || value.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let count = parse_number(value, "limit")?;
    if count < 0 {
        return Err(invalid(format!("limit は 0 以上の数値です: {value}")));
    }
    if count == 0 {
        return Ok(None);
    }
    if count as usize > MAX_LIST_LIMIT {
        return Err(invalid(format!(
            "limit は {MAX_LIST_LIMIT} 以下です (全件は limit=all)"
        )));
    }
    Ok(Some(count as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn a_negative_offset_is_rejected() {
        let error = ExternalListRequest::from_pairs(&pairs(&[("offset", "-1")])).unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidRequest(_)));
    }

    #[test]
    fn an_out_of_range_novel_type_is_rejected() {
        let error = ExternalListRequest::from_pairs(&pairs(&[("novel_type", "256")])).unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidRequest(_)));
        let request = ExternalListRequest::from_pairs(&pairs(&[("novel_type", "2")])).unwrap();
        assert_eq!(request.novel_type, Some(2));
    }

    #[test]
    fn an_unknown_format_is_rejected() {
        let error = ExternalListRequest::from_pairs(&pairs(&[("format", "yaml")])).unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidRequest(_)));
        assert_eq!(parse_format("jsonl").unwrap(), OutputFormat::Jsonl);
        assert!(parse_format("yaml").is_err());
    }

    #[test]
    fn the_limit_all_means_no_limit() {
        let request = ExternalListRequest::from_pairs(&pairs(&[("limit", "all")])).unwrap();
        assert_eq!(request.limit, None);
        let request = ExternalListRequest::from_pairs(&pairs(&[("limit", "0")])).unwrap();
        assert_eq!(request.limit, None);
        let request = ExternalListRequest::from_pairs(&pairs(&[("limit", "5")])).unwrap();
        assert_eq!(request.limit, Some(5));
        assert!(ExternalListRequest::from_pairs(&pairs(&[("limit", "10001")])).is_err());
    }

    #[test]
    fn an_xml_array_document_has_exactly_one_root_element() {
        let document = serde_json::json!([
            { "name": "a", "count": 1 },
            { "name": "b", "count": 2 }
        ]);
        let body = render(
            &document,
            &RenderOptions {
                format: OutputFormat::Xml,
                root: "tags",
                array_names: &[],
                xml_item: "tag",
                jsonl_items: None,
            },
        );
        assert_eq!(body.matches("<tags>").count(), 1);
        assert_eq!(body.matches("<tag>").count(), 2);
        assert!(body.contains("<name>a</name>"), "{body}");
    }

    #[test]
    fn a_section_key_is_the_index_for_a_normal_novel() {
        assert_eq!(section_key(1, "3", false), "3");
        assert_eq!(section_key(12, "3", true), "12:3");
    }

    #[test]
    fn a_member_section_key_resolves_to_its_member() {
        let owners = vec![12, 13];
        assert_eq!(
            resolve_section_owner(&owners, "12:3"),
            Some((12, "3".to_string()))
        );
        // 複数メンバーのバーチャル小説では話番号だけでは特定できない。
        assert_eq!(resolve_section_owner(&owners, "3"), None);
        assert_eq!(
            resolve_section_owner(&[12], "3"),
            Some((12, "3".to_string()))
        );
    }
}
