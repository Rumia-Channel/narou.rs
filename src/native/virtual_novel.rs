//! Native のバーチャル小説メンバー構成ストアと結合 EPUB 変換。
//!
//! 在庫 `virtual_novel_members` を唯一の保存先とする。SQLite 管理時は
//! `app_state(scope='inv', key='virtual_novel_members')`、YAML 管理時は
//! `.narou/virtual_novel_members.yaml`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::StreamExt;

use crate::converter::NovelConverter;
use crate::converter::device::Device;
use crate::converter::settings::NovelSettings;
use crate::db;
use crate::db::inventory::{Inventory, InventoryScope};
use crate::db::NovelRecord;
use crate::downloader::settings::{DownloaderSettings, NativeDownloaderSettings};
use crate::downloader::site_setting::SiteSetting;
use crate::downloader::types::TocFile;
use crate::downloader::util::{load_length_limit, sanitize_filename_with_limit};
use crate::error::{NarouError, Result};
use crate::native::novel_repository::NativeNovelRepository;
use crate::native::object_store::{NativeObjectStore, NativeStore, NativeStores};
use crate::platform::{NovelId, NovelObjectKeys, ObjectStore};
use crate::virtual_novel::{self, AttributeComposition, VirtualNovelMembers};

/// 仮想小説の属性分類 (小説 / 漫画)。
///
/// 分類は通常タグとは別の `attribute_tag` フィールドに保存し、表示機能の
/// ON / OFF とは独立に保持する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributeState {
    /// メンバーが小説と漫画に混在しているか。
    pub mixed: bool,
    /// 適用される属性タグ (小説 / 漫画)。
    pub attribute_tag: &'static str,
}

/// 仮想小説の登録内容。メンバーは登録順。
#[derive(Debug, Clone)]
pub struct NewVirtualNovel {
    pub title: String,
    pub author: String,
    pub sitename: String,
    pub domain: Option<String>,
    pub tags: Vec<String>,
    pub members: Vec<i64>,
    /// 利用者が指定した属性タグ (小説 / 漫画)。混在しているときだけ意味を持つ。
    pub attribute: Option<String>,
}

/// メンバーの属性タグ構成 (小説 / 漫画) を求める。
///
/// メンバーは普通の小説行なので、それぞれ `attribute_tag` を持つ。属性が
/// 未設定のメンバーは保存済み情報 (サイト設定など) から判定する。
pub fn composition_of(members: &[i64]) -> Result<AttributeComposition> {
    let sites = crate::downloader::site_setting::effective_site_settings();
    composition_with_sites(members, &sites)
}

/// サイト設定を使い回してメンバーの属性タグ構成を求める。
fn composition_with_sites(
    members: &[i64],
    sites: &[SiteSetting],
) -> Result<AttributeComposition> {
    db::with_database(|database| composition_in(members, sites, |id| database.get(id)))
}

fn composition_in<'a>(
    members: &[i64],
    sites: &[SiteSetting],
    lookup: impl Fn(i64) -> Option<&'a NovelRecord>,
) -> Result<AttributeComposition> {
    let mut composition = AttributeComposition::default();
    for member in members {
        let record = lookup(*member)
            .ok_or_else(|| NarouError::Database(format!("作品が見つかりません: {member}")))?;
        if record.ncode.as_deref().is_some_and(virtual_novel::is_virtual_ncode) {
            return Err(NarouError::InvalidTarget(format!("仮想小説をメンバーにはできません: {member}")));
        }
        composition.add(Some(member_attribute_tag(record, sites)));
    }
    Ok(composition)
}

/// メンバー 1 件の属性タグを保存済み情報から求める。
fn member_attribute_tag(record: &NovelRecord, sites: &[SiteSetting]) -> &'static str {
    crate::application::attribute_tags::initial_tag(record, sites)
}

/// 読み込み済みのメンバー構成から、属性分類をまとめて求める。
///
/// 単一種別のときはメンバー構成から自動分類し、混在しているときは保存済みの
/// 値 (利用者の選択) を優先する。保存値が無いときは小説を既定とする。
/// サイト設定は 1 度だけ読み込む。分類を確定できない仮想小説は省く。
pub fn attribute_states_in(members: &VirtualNovelMembers) -> BTreeMap<i64, AttributeState> {
    let sites = crate::downloader::site_setting::effective_site_settings();
    let mut states = BTreeMap::new();
    for id in members.groups.keys() {
        let Some(member_ids) = members.get(*id) else {
            continue;
        };
        let Ok(composition) = composition_with_sites(member_ids, &sites) else {
            continue;
        };
        let Ok(Some(record)) = NativeNovelRepository::new().get_sync(NovelId(*id)) else {
            continue;
        };
        let Ok(resolved) = composition.resolve(None, record.attribute_tag()) else {
            continue;
        };
        states.insert(
            *id,
            AttributeState {
                mixed: composition.is_mixed(),
                attribute_tag: resolved,
            },
        );
    }
    states
}

/// バーチャル小説の属性タグを変更する (混在しているときだけ)。
///
/// 保存済みの値だけを書き換えるため、他の項目 (タグなど) を上書きしない。
pub fn set_attribute(id: i64, requested: &str) -> Result<AttributeState> {
    let sites = crate::downloader::site_setting::effective_site_settings();
    mutate(|records, members, _| {
        let ids = members.get(id)
            .ok_or_else(|| NarouError::Database(format!("仮想小説 vs_{id} は登録されていません")))?;
        let composition = composition_in(ids, &sites, |id| records.get(&id))?;
        if !composition.is_mixed() {
            return Err(NarouError::InvalidTarget("混在していないバーチャル小説の属性タグは変更できません".into()));
        }
        let attribute_tag = composition.resolve(Some(requested), None).map_err(NarouError::InvalidTarget)?;
        let record = records.get_mut(&id)
            .ok_or_else(|| NarouError::Database(format!("仮想小説 vs_{id} が見つかりません")))?;
        record.set_attribute_tag(attribute_tag);
        Ok(AttributeState { mixed: true, attribute_tag })
    })
}

/// 既存のバーチャル小説の属性タグをメンバー構成に合わせて整合させる。
///
/// 混在しているものは保存済みの値を維持し、単一種別のものは自動分類の値へ
/// 直す。メンバーが見つからないものは既存値をそのまま残す。
pub fn reconcile_attribute_tags() -> Result<()> {
    if load()?.is_empty() { return Ok(()); }
    let sites = crate::downloader::site_setting::effective_site_settings();
    mutate(|records, members, _| {
        for (id, member_ids) in &members.groups {
            let result = (|| {
                let composition = composition_in(member_ids, &sites, |id| records.get(&id))?;
                let record = records.get_mut(id)
                    .ok_or_else(|| NarouError::Database(format!("仮想小説 vs_{id} が見つかりません")))?;
                let attribute = composition.resolve(None, record.attribute_tag()).map_err(NarouError::InvalidTarget)?;
                record.set_attribute_tag(attribute);
                Ok::<_, NarouError>(())
            })();
            if let Err(error) = result {
                tracing::warn!("バーチャル小説 vs_{id} の属性タグを整合できません: {error}");
            }
        }
        Ok(())
    })
}


/// 仮想小説の行を作り、メンバー構成を保存する。
pub fn create(input: NewVirtualNovel) -> Result<NovelRecord> {
    if input.title.trim().is_empty() {
        return Err(NarouError::InvalidTarget("タイトルを指定してください".into()));
    }
    if input.members.is_empty() {
        return Err(NarouError::InvalidTarget("メンバーを指定してください".into()));
    }
    virtual_novel::validate_members(&input.members).map_err(NarouError::InvalidTarget)?;
    if input.tags.iter().any(|tag| crate::application::attribute_tags::is_attribute_tag(tag.trim())) {
        return Err(NarouError::InvalidTarget("属性タグは編集できません".into()));
    }
    let sites = crate::downloader::site_setting::effective_site_settings();
    let limit = load_length_limit("folder-length-limit", Some(50));
    let file_title = sanitize_filename_with_limit(&input.title, limit);
    let use_subdirectory = NativeDownloaderSettings.download_use_subdirectory();
    // The identity travels with the group: a library-local URL would collide
    // with the same number in another library.
    let toc_url = virtual_novel::new_toc_url()?;
    let reservation = db::with_database_mut(|database| database.allocate_id())?;
    let id = reservation.id().0;
    let saved = mutate(|records, members, archive_root| {
        if records.contains_key(&id) {
            return Err(NarouError::Database(format!("作品 ID {id} は既に使用されています。再試行してください")));
        }
        let summary = summarize_members(&input.members, records, archive_root, &sites)?;
        let requested = input.attribute.as_deref().map(str::trim).filter(|tag| !tag.is_empty());
        let attribute = summary.composition.resolve(requested, None).map_err(NarouError::InvalidTarget)?;
        members.insert(id, input.members).map_err(NarouError::InvalidTarget)?;
        let ncode = virtual_novel::ncode(id);
        let mut tags: Vec<String> = input.tags.iter()
            .map(|tag| tag.trim().to_string()).filter(|tag| !tag.is_empty())
            .chain(summary.tags.into_iter().filter(|tag| !crate::application::attribute_tags::is_attribute_tag(tag)))
            .collect();
        tags.push(virtual_novel::tag(id));
        tags.sort();
        tags.dedup();
        let mut record = NovelRecord {
            id,
            author: input.author,
            title: input.title,
            file_title: if file_title.is_empty() { ncode.clone() } else { file_title },
            toc_url,
            sitename: input.sitename,
            novel_type: 0,
            end: true,
            last_update: Utc::now(),
            new_arrivals_date: None,
            use_subdirectory,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: summary.latest_update,
            last_mail_date: None,
            tags,
            ncode: Some(ncode),
            domain: input.domain,
            general_all_no: (summary.general_all_no > 0).then_some(summary.general_all_no),
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        };
        record.set_attribute_tag(attribute);
        records.insert(id, record.clone());
        Ok(record)
    });
    if saved.is_err() {
        let _ = db::with_database_mut(|database| { database.release_id(reservation); Ok(()) });
    }
    saved
}

/// バーチャル小説に移植可能な保存先 URL を持たせる。
///
/// 古いライブラリは `.../virtual/vs_<ID>` というライブラリ固有の URL を持つ。
/// この URL は別のライブラリで衝突するため、初回エクスポート時に 1 回だけ
/// 移植可能な URL を保存する。同時に走ったエクスポートは 1 つの結果に
/// 収束し、ほかの作品の変更はそのまま残る。
pub fn ensure_portable_identity(id: i64) -> Result<String> {
    let repository = NativeNovelRepository::new();
    for _ in 0..8 {
        let Some(record) = repository.get_sync(NovelId(id))? else {
            return Err(NarouError::NotFound(format!("ID: {id} が見つかりません")));
        };
        if !virtual_novel::is_virtual_record(&record) {
            return Err(NarouError::InvalidTarget(format!("ID: {id} はバーチャル小説ではありません")));
        }
        if virtual_novel::is_portable_toc_url(&record.toc_url) {
            return Ok(record.toc_url);
        }
        let observed = record.toc_url.clone();
        let candidate = virtual_novel::new_toc_url()?;
        // 書き込みは 1 つの境界の中で行う。先に保存した側の URL が共有値
        // になり、再読み込みで同じ値へ収束する。
        let current = db::with_database_mut(|database| {
            database.update_records(|mut records| {
                let Some(entry) = records.get_mut(&id) else {
                    return Err(NarouError::NotFound(format!("ID: {id} が見つかりません")));
                };
                if !virtual_novel::is_virtual_record(entry) {
                    return Err(NarouError::InvalidTarget(format!("ID: {id} はバーチャル小説ではありません")));
                }
                if entry.toc_url == observed {
                    entry.toc_url = candidate.clone();
                }
                // The URL is copied out before the map is moved back.
                let current = entry.toc_url.clone();
                Ok((records, current))
            })
        })?;
        if virtual_novel::is_portable_toc_url(&current) {
            return Ok(current);
        }
    }
    Err(NarouError::Platform(format!(
        "ID: {id} のバーチャル小説の保存先 URL を更新できません"
    )))
}

/// アーカイブがバーチャル小説について持つメタデータ。
///
/// 取り込みでは、この値だけを保存済みの行へ反映する。保存済みの行のほかの
/// 項目 (変換状態など) は消さない。
#[derive(Debug, Clone, Default)]
pub struct ImportedParentMetadata {
    pub author: String,
    pub title: String,
    pub domain: Option<String>,
    /// The parent's ordinary tags; the internal tag never travels.
    pub tags: Vec<String>,
    pub attribute_tag: Option<String>,
    /// The date the archive carries, used when no member has one.
    pub general_lastup: Option<DateTime<Utc>>,
}

/// 取り込みしたバーチャル小説を公開する (レコードと所属は 1 回の保存)。
///
/// ID の重複と保存先 URL の重複は、この保存の中で判断する。メンバーが 1 件
/// でも欠けていれば公開しない。話数・最新掲載日・タグ構成はメンバーから
/// 集計し、日付を持たないメンバーはアーカイブの日付を使う。
pub fn publish_imported_parent(
    record: NovelRecord,
    member_ids: Vec<i64>,
    fallback_lastup: Option<DateTime<Utc>>,
) -> Result<()> {
    let sites = crate::downloader::site_setting::effective_site_settings();
    mutate(|records, members, archive_root| {
        // The plan ran against an earlier snapshot: the live rows decide.
        if records.contains_key(&record.id) {
            return Err(NarouError::Database(format!(
                "作品 ID {} は既に使用されています。再試行してください",
                record.id
            )));
        }
        if let Some(existing) = records.values().find(|other| other.toc_url == record.toc_url) {
            return Err(NarouError::Platform(format!(
                "既に ID:{} として登録されています",
                existing.id
            )));
        }
        let summary = summarize_members(&member_ids, records, archive_root, &sites)?;
        let mut record = record;
        summary.refresh(&mut record).map_err(NarouError::InvalidTarget)?;
        if record.general_lastup.is_none() {
            record.general_lastup = fallback_lastup;
        }
        members.insert(record.id, member_ids.clone()).map_err(NarouError::InvalidTarget)?;
        records.insert(record.id, record);
        Ok(())
    })
}

/// 既存、バーチャル小説の所属と派生メタデータを置き換える。
///
/// ID と保存フォルダはそのまま残す。所属順はアーカイブの順で置き換え、話数・
/// 最新掲載日・タグ構成はメンバーから集計し直す。保存済みの行は書き換えの
/// 境界の中で読み直すので、同時に変わった項目は消さない。
pub fn replace_imported_parent(
    id: i64,
    metadata: &ImportedParentMetadata,
    member_ids: Vec<i64>,
) -> Result<()> {
    let sites = crate::downloader::site_setting::effective_site_settings();
    mutate(|records, members, archive_root| {
        virtual_novel::validate_members(&member_ids).map_err(NarouError::InvalidTarget)?;
        if !members.groups.contains_key(&id) {
            return Err(NarouError::NotFound(format!(
                "バーチャル小説 vs_{id} は登録されていません"
            )));
        }
        // 1 つの小説は 1 つのバーチャル小説にしか属さない。
        for member in &member_ids {
            if let Some(owner) = members.owner_of(*member) && owner != id {
                return Err(NarouError::InvalidTarget(format!(
                    "小説 {member} は既にバーチャル小説 vs_{owner} に属しています"
                )));
            }
        }
        // The live row decides: a concurrent change of another field stays.
        let mut updated = records.get(&id)
            .ok_or_else(|| NarouError::NotFound(format!("バーチャル小説 vs_{id} が見つかりません")))?
            .clone();
        if !virtual_novel::is_virtual_record(&updated) {
            return Err(NarouError::InvalidTarget(format!("ID: {id} はバーチャル小説ではありません")));
        }
        if updated.author.trim().is_empty() && !metadata.author.trim().is_empty() {
            updated.author = metadata.author.clone();
        }
        if updated.title.trim().is_empty() && !metadata.title.trim().is_empty() {
            updated.title = metadata.title.clone();
        }
        if updated.domain.is_none() && metadata.domain.is_some() {
            updated.domain = metadata.domain.clone();
        }
        // The archive's ordinary tags join the stored ones; a tag added
        // meanwhile is not erased.
        for tag in &metadata.tags {
            if !virtual_novel::is_internal_tag(tag) && !updated.tags.contains(tag) {
                updated.tags.push(tag.clone());
            }
        }
        // The classification is metadata the archive carries explicitly.
        if let Some(attribute) = metadata.attribute_tag.as_deref() {
            updated.set_attribute_tag(attribute);
        }
        members.groups.insert(id, member_ids.clone());
        let summary = summarize_members(&member_ids, records, archive_root, &sites)?;
        summary.refresh(&mut updated).map_err(NarouError::InvalidTarget)?;
        if updated.general_lastup.is_none() {
            updated.general_lastup = metadata.general_lastup;
        }
        records.insert(id, updated);
        Ok(())
    })
}

fn summarize_members(
    members: &[i64],
    records: &BTreeMap<i64, NovelRecord>,
    archive_root: &Path,
    sites: &[SiteSetting],
) -> Result<virtual_novel::MemberSummary> {
    let mut summary = virtual_novel::MemberSummary::default();
    for member in members {
        let record = records.get(member)
            .ok_or_else(|| NarouError::Database(format!("作品が見つかりません: {member}")))?;
        if record
            .ncode
            .as_deref()
            .is_some_and(virtual_novel::is_virtual_ncode)
        {
            return Err(NarouError::InvalidTarget(format!(
                "仮想小説をメンバーにはできません: {member}"
            )));
        }
        let count = match record.general_all_no {
            Some(count) if count > 0 => count,
            _ => {
                let dir = db::existing_novel_dir_for_record(archive_root, record);
                read_toc(&dir)
                    .map(|toc| toc.subtitles.len() as i64)
                    .unwrap_or(0)
            }
        };
        summary.add(record, count, Some(member_attribute_tag(record, sites)));
    }
    Ok(summary)
}

/// 既存の仮想小説へメンバーを追加する。メンバー行は一覧から隠れ、EPUB は
/// 登録順の末尾へ結合される。仮想小説のタグ・話数・最新話掲載日も更新する。
pub fn add_members(id: i64, new_members: Vec<i64>) -> Result<()> {
    let sites = crate::downloader::site_setting::effective_site_settings();
    mutate(|records, members, archive_root| {
        members.append(id, new_members).map_err(NarouError::InvalidTarget)?;
        let summary = summarize_members(members.get(id).expect("validated virtual novel"), records, archive_root, &sites)?;
        let record = records.get_mut(&id)
            .ok_or_else(|| NarouError::Database(format!("仮想小説 vs_{id} が見つかりません")))?;
        summary.refresh(record).map_err(NarouError::InvalidTarget)
    })
}

/// 仮想小説の行とメンバー構成を削除する。実小説は残す。
pub fn remove(id: i64) -> Result<()> {
    mutate(|records, members, _| {
        if members.remove(id).is_none() {
            return Err(NarouError::Database(format!("仮想小説 vs_{id} は登録されていません")));
        }
        records.remove(&id);
        Ok(())
    })
}

/// 現在のメンバーを ID 列で全件並べ替える。
pub fn reorder_by_ids(id: i64, ordered: Vec<i64>) -> Result<()> {
    update_members(|all| all.reorder(id, ordered).map_err(NarouError::Platform))
}

/// 現在のメンバーを 1 始まりの位置で全件並べ替える。
pub fn reorder_by_positions(id: i64, positions: &[usize]) -> Result<()> {
    update_members(|all| {
        let members = all.get(id)
            .ok_or_else(|| NarouError::Database(format!("仮想小説 vs_{id} がありません")))?;
        if positions.len() != members.len() {
            return Err(NarouError::InvalidTarget("現在のメンバーを全件指定してください".into()));
        }
        let mut seen = std::collections::HashSet::new();
        let mut ordered = Vec::with_capacity(positions.len());
        for position in positions {
            if *position == 0 || *position > members.len() {
                return Err(NarouError::InvalidTarget(format!(
                    "順序 {position} は 1〜{} の範囲で指定してください", members.len(),
                )));
            }
            if !seen.insert(*position) {
                return Err(NarouError::InvalidTarget(format!("順序 {position} が重複しています")));
            }
            ordered.push(members[position - 1]);
        }
        all.reorder(id, ordered).map_err(NarouError::Platform)
    })
}

pub fn load() -> Result<VirtualNovelMembers> {
    let inventory = Inventory::with_default_root()?;
    inventory.load("virtual_novel_members", InventoryScope::Local)
}

/// 在庫を [`VirtualNovelDirectory`](crate::application::external_api::VirtualNovelDirectory) として使うアダプタ。
///
/// 外部 API と MCP はバーチャル小説の所属 (どの作品をまとめているか) を
/// 読む必要がある。native 側は在庫の `virtual_novel_members` がその実体。
pub struct NativeVirtualNovelDirectory;

impl crate::application::external_api::VirtualNovelDirectory for NativeVirtualNovelDirectory {
    fn members(&self) -> crate::platform::PlatformFuture<'_, Result<VirtualNovelMembers>> {
        Box::pin(async move { load() })
    }
}

fn update_members<F>(change: F) -> Result<()>
where
    F: FnOnce(&mut VirtualNovelMembers) -> Result<()>,
{
    let inventory = Inventory::with_default_root()?;
    inventory.update_yaml::<(), VirtualNovelMembers, _>(
        "virtual_novel_members",
        InventoryScope::Local,
        |mut members| {
            change(&mut members)?;
            Ok((members, ()))
        },
    )
}

fn mutate<T>(
    change: impl FnOnce(&mut BTreeMap<i64, NovelRecord>, &mut VirtualNovelMembers, &Path) -> Result<T>,
) -> Result<T> {
    db::with_database_mut(|database| {
        let archive_root = database.archive_root().to_path_buf();
        database.update_virtual_records(|mut records, mut members| {
            let result = change(&mut records, &mut members, &archive_root)?;
            Ok((records, members, result))
        })
    })
}

/// メンバーを登録順に 1 つの EPUB へ結合する。
///
/// 各メンバーの `toc.yaml` / `本文/*.yaml` を一時ディレクトリへ連結し、
/// 通常の [`NovelConverter`] に仮想小説のレコードとして渡す。メンバーの
/// 先頭セクションにはメンバータイトルを章見出しとして設定する。完成した
/// 出力は仮想小説の保存フォルダへコピーし、そのパスを返す。
pub fn convert_virtual(
    id: i64,
    record: &db::NovelRecord,
    members: &[i64],
    device: Option<Device>,
    no_strip: bool,
    verbose: bool,
) -> Result<PathBuf> {
    if members.is_empty() {
        return Err(NarouError::Platform(
            "バーチャル小説にメンバーがありません".into(),
        ));
    }
    let repository = NativeNovelRepository::new();
    let archive_root =
        db::with_database(|database| Ok(database.archive_root().to_path_buf()))?;
    let temp = tempfile::Builder::new()
        .prefix("narou-rs-virtual-")
        .tempdir()
        .map_err(NarouError::Io)?;
    std::fs::create_dir_all(temp.path().join("本文")).map_err(NarouError::Io)?;

    let objects = Arc::new(NativeStore::for_current_root()?.prefer_existing_files());
    let stores = NativeStores::for_current_root()?;
    let local = NativeObjectStore::from_root(archive_root.clone())?;
    let persistence = crate::downloader::persistence::PersistenceService::new(
        objects.clone(), stores.assets.clone(),
    );
    let toc = crate::native::materialization::block_on(async {
        let mut book = crate::application::virtual_book::load(record, members, &repository, &persistence).await?;
        while let Some(section) = book.sections.next().await {
            let section = section?;
            let filename = format!("{} {}.yaml", section.index, section.file_subtitle);
            std::fs::write(
                temp.path().join("本文").join(filename),
                serde_yaml::to_string(&section).map_err(NarouError::Yaml)?,
            )?;
        }
        for (name, key) in &book.illustrations {
            let destination = temp.path().join(name);
            if !crate::native::materialization::copy_asset(&local, key, &destination).await? {
                crate::native::materialization::copy_asset(stores.assets.as_ref(), key, &destination).await?;
            }
        }
        let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
        for name in ["setting.ini", "replace.txt", "converter.yaml", "cover.jpg", "cover.png", "cover.jpeg"] {
            if let Some(bytes) = objects.read_small(&keys.prefix().join(name)?).await? {
                std::fs::write(temp.path().join(name), bytes)?;
            }
        }
        Ok(book.toc)
    })?;
    std::fs::write(
        temp.path().join("toc.yaml"),
        serde_yaml::to_string(&toc).map_err(NarouError::Yaml)?,
    )
    .map_err(NarouError::Io)?;

    let settings = NovelSettings::load_for_novel(id, &record.title, &record.author, temp.path());
    let mut converter = NovelConverter::new(settings);
    converter.set_progress(Box::new(crate::platform::progress::NoProgress));
    let output = match device {
        Some(device) => converter.convert_novel_by_id_with_device(
            id,
            temp.path(),
            device,
            no_strip,
            verbose,
        )?,
        None => PathBuf::from(converter.convert_novel_by_id(id, temp.path())?),
    };

    let destination_dir = db::novel_dir_for_record(&archive_root, record);
    std::fs::create_dir_all(&destination_dir).map_err(NarouError::Io)?;
    let file_name = output.file_name().ok_or_else(|| {
        NarouError::Conversion("バーチャル小説の出力ファイル名を取得できません".into())
    })?;
    let destination = destination_dir.join(file_name);
    std::fs::copy(&output, &destination).map_err(NarouError::Io)?;
    Ok(destination)
}

fn read_toc(novel_dir: &Path) -> Result<TocFile> {
    let content = std::fs::read_to_string(novel_dir.join("toc.yaml")).map_err(NarouError::Io)?;
    serde_yaml::from_str(&content).map_err(NarouError::Yaml)
}

