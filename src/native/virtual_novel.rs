//! Native のバーチャル小説メンバー構成ストアと結合 EPUB 変換。
//!
//! 在庫 `virtual_novel_members` を唯一の保存先とする。SQLite 管理時は
//! `app_state(scope='inv', key='virtual_novel_members')`、YAML 管理時は
//! `.narou/virtual_novel_members.yaml`。

use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::converter::NovelConverter;
use crate::converter::device::Device;
use crate::converter::settings::NovelSettings;
use crate::db;
use crate::db::inventory::{Inventory, InventoryScope};
use crate::db::NovelRecord;
use crate::downloader::settings::{DownloaderSettings, NativeDownloaderSettings};
use crate::downloader::types::{SectionFile, SubtitleInfo, TocFile};
use crate::downloader::util::{load_length_limit, sanitize_filename_with_limit};
use crate::error::{NarouError, Result};
use crate::native::novel_repository::NativeNovelRepository;
use crate::native::object_store::NativeStore;
use crate::platform::{NovelId, NovelMutation};
use crate::virtual_novel::{self, VirtualNovelMembers};

/// 仮想小説の登録内容。メンバーは登録順。
#[derive(Debug, Clone)]
pub struct NewVirtualNovel {
    pub title: String,
    pub author: String,
    pub sitename: String,
    pub domain: Option<String>,
    pub tags: Vec<String>,
    pub members: Vec<i64>,
}

/// 仮想小説の行を作り、メンバー構成を保存する。
pub fn create(input: NewVirtualNovel) -> Result<NovelRecord> {
    if input.title.trim().is_empty() {
        return Err(NarouError::InvalidTarget(
            "タイトルを指定してください".into(),
        ));
    }
    if input.members.is_empty() {
        return Err(NarouError::InvalidTarget(
            "メンバーを指定してください".into(),
        ));
    }
    let repository = NativeNovelRepository::new();
    let mut member_tags: Vec<String> = Vec::new();
    let mut latest_update: Option<chrono::DateTime<Utc>> = None;
    let mut general_all_no = 0i64;
    for member in &input.members {
        let record = repository
            .get_sync(NovelId(*member))?
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
        member_tags.extend(
            record
                .tags
                .iter()
                .filter(|tag| !virtual_novel::is_internal_tag(tag))
                .cloned(),
        );
        if let Some(updated) = record.general_lastup
            && latest_update.is_none_or(|current| updated > current)
        {
            latest_update = Some(updated);
        }
        let count = match record.general_all_no {
            Some(count) if count > 0 => count,
            _ => {
                let archive_root =
                    db::with_database(|database| Ok(database.archive_root().to_path_buf()))?;
                let dir = db::existing_novel_dir_for_record(&archive_root, &record);
                read_toc(&dir)
                    .map(|toc| toc.subtitles.len() as i64)
                    .unwrap_or(0)
            }
        };
        general_all_no += count;
    }
    if crate::native::virtual_novel::load()?
        .groups
        .values()
        .any(|members| members.iter().any(|member| input.members.contains(member)))
    {
        return Err(NarouError::InvalidTarget(
            "既に別の仮想小説に属している作品があります".into(),
        ));
    }

    let reservation = db::with_database_mut(|database| database.allocate_id())?;
    let id = reservation.id().0;
    let ncode = virtual_novel::ncode(id);
    let virtual_tag = virtual_novel::tag(id);
    let mut tags: Vec<String> = input
        .tags
        .iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .chain(member_tags)
        .collect();
    tags.push(virtual_tag);
    tags.sort();
    tags.dedup();

    let limit = load_length_limit("folder-length-limit", Some(50));
    let file_title = sanitize_filename_with_limit(&input.title, limit);
    let file_title = if file_title.is_empty() {
        ncode.clone()
    } else {
        file_title
    };
    let now = Utc::now();
    let record = NovelRecord {
        id,
        author: input.author.clone(),
        title: input.title.clone(),
        file_title,
        toc_url: format!("https://narou.rs/virtual/{ncode}"),
        sitename: input.sitename.clone(),
        novel_type: 0,
        end: true,
        last_update: now,
        new_arrivals_date: None,
        use_subdirectory: NativeDownloaderSettings.download_use_subdirectory(),
        general_firstup: None,
        novelupdated_at: None,
        general_lastup: latest_update,
        last_mail_date: None,
        tags,
        ncode: Some(ncode.clone()),
        domain: input.domain.clone(),
        general_all_no: (general_all_no > 0).then_some(general_all_no),
        length: None,
        suspend: false,
        is_narou: false,
        last_check_date: None,
        convert_failure: false,
        requires_login: false,
        login_session: None,
        extra_fields: Default::default(),
    };

    repository.apply_batch_sync(vec![NovelMutation::Upsert(record.clone())])?;
    let saved = update(|members| {
        members
            .insert(id, input.members.clone())
            .map_err(NarouError::Platform)
    });
    if let Err(error) = saved {
        let _ = repository.apply_batch_sync(vec![NovelMutation::Remove(NovelId(id))]);
        return Err(error);
    }
    Ok(record)
}

/// 仮想小説の行とメンバー構成を削除する。実小説は残す。
pub fn remove(id: i64) -> Result<()> {
    update(|members| {
        if members.remove(id).is_none() {
            return Err(NarouError::Database(format!(
                "仮想小説 vs_{id} は登録されていません"
            )));
        }
        Ok(())
    })?;
    NativeNovelRepository::new()
        .apply_batch_sync(vec![NovelMutation::Remove(NovelId(id))])?;
    Ok(())
}

/// 現在のメンバーを ID 列で全件並べ替える。
pub fn reorder_by_ids(id: i64, ordered: Vec<i64>) -> Result<()> {
    update(|all| {
        all.reorder(id, ordered).map_err(NarouError::Platform)
    })?;
    Ok(())
}

/// 現在のメンバーを 1 始まりの位置で全件並べ替える。
pub fn reorder_by_positions(id: i64, positions: &[usize]) -> Result<()> {
    let current = load()?;
    let members = current
        .get(id)
        .ok_or_else(|| NarouError::Database(format!("仮想小説 vs_{id} がありません")))?;
    if positions.len() != members.len() {
        return Err(NarouError::InvalidTarget(
            "現在のメンバーを全件指定してください".into(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut ordered = Vec::with_capacity(positions.len());
    for position in positions {
        if *position == 0 || *position > members.len() {
            return Err(NarouError::InvalidTarget(format!(
                "順序 {position} は 1〜{} の範囲で指定してください",
                members.len()
            )));
        }
        if !seen.insert(*position) {
            return Err(NarouError::InvalidTarget(format!(
                "順序 {position} が重複しています"
            )));
        }
        ordered.push(members[position - 1]);
    }
    update(|all| {
        all.reorder(id, ordered).map_err(NarouError::Platform)
    })?;
    Ok(())
}

pub fn load() -> Result<VirtualNovelMembers> {
    let inventory = Inventory::with_default_root()?;
    Ok(inventory
        .load("virtual_novel_members", InventoryScope::Local)
        .unwrap_or_default())
}

pub fn save(members: &VirtualNovelMembers) -> Result<()> {
    let inventory = Inventory::with_default_root()?;
    inventory.save("virtual_novel_members", InventoryScope::Local, members)
}

pub fn update<F>(change: F) -> Result<VirtualNovelMembers>
where
    F: FnOnce(&mut VirtualNovelMembers) -> Result<()>,
{
    let inventory = Inventory::with_default_root()?;
    inventory.update_yaml::<VirtualNovelMembers, VirtualNovelMembers, _>(
        "virtual_novel_members",
        InventoryScope::Local,
        |mut members| {
            change(&mut members)?;
            Ok((members.clone(), members))
        },
    )
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

    // 各メンバーの実ファイルを変換の間だけ実体化する。
    let store = NativeStore::for_current_root()?;
    let mut _materialized = Vec::new();
    let mut subtitles: Vec<SubtitleInfo> = Vec::new();
    let mut sequence = 0usize;

    for member_id in members {
        let member = repository
            .get_sync(NovelId(*member_id))?
            .ok_or_else(|| {
                NarouError::Platform(format!(
                    "バーチャル小説のメンバー ID {member_id} が見つかりません"
                ))
            })?;
        let member_dir = db::existing_novel_dir_for_record(&archive_root, &member);
        _materialized.push(store.materialize_novel_files(&member_dir)?);
        let toc = read_toc(&member_dir)?;
        copy_illustrations(&member_dir.join("挿絵"), &temp.path().join("挿絵"))?;

        for (position, subtitle) in toc.subtitles.iter().enumerate() {
            let section_path = resolve_section_path(&member_dir, subtitle).ok_or_else(|| {
                NarouError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "本文が見つかりません: ID:{member_id} {}",
                        subtitle.file_subtitle
                    ),
                ))
            })?;
            let mut section: SectionFile =
                serde_yaml::from_str(&std::fs::read_to_string(&section_path).map_err(NarouError::Io)?)
                    .map_err(NarouError::Yaml)?;
            sequence += 1;
            let index = sequence.to_string();
            section.index = index.clone();
            section.href.clone_from(&subtitle.href);
            section.chapter = if position == 0 {
                member.title.clone()
            } else {
                String::new()
            };
            section.subchapter.clear();
            let filename = format!("{} {}.yaml", index, section.file_subtitle);
            std::fs::write(
                temp.path().join("本文").join(filename),
                serde_yaml::to_string(&section).map_err(NarouError::Yaml)?,
            )
            .map_err(NarouError::Io)?;

            let mut merged = subtitle.clone();
            merged.index = index;
            merged.chapter = section.chapter.clone();
            merged.subchapter.clear();
            subtitles.push(merged);
        }
    }

    let toc = TocFile {
        title: record.title.clone(),
        author: record.author.clone(),
        toc_url: record.toc_url.clone(),
        story: None,
        subtitles,
        novel_type: Some(record.novel_type),
    };
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

fn resolve_section_path(novel_dir: &Path, subtitle: &SubtitleInfo) -> Option<PathBuf> {
    crate::native::legacy_persistence::resolve_section_file_path(
        &novel_dir.join(crate::downloader::SECTION_SAVE_DIR),
        subtitle,
    )
}

fn copy_illustrations(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(destination).map_err(NarouError::Io)?;
    for entry in std::fs::read_dir(source).map_err(NarouError::Io)? {
        let entry = entry.map_err(NarouError::Io)?;
        let path = entry.path();
        if path.is_file() {
            let name = entry.file_name();
            std::fs::copy(&path, destination.join(name)).map_err(NarouError::Io)?;
        } else if path.is_dir() {
            let name = entry.file_name();
            copy_illustrations(&path, &destination.join(name))?;
        }
    }
    Ok(())
}

