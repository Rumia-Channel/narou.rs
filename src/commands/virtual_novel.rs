//! `narou virtual` — 複数の実小説を 1 つのバーチャル小説として登録する。
//!
//! 仮想小説自体も `novels` の通常行で、`ncode = vs_<ID>`。本文は各メンバーの
//! まま保持し、変換時にだけ登録順へ結合する。タグ `vs_<ID>` が必ず付くので、
//! 既存の hidden tag で非表示にできる。

use chrono::Utc;
use narou_rs::db;
use narou_rs::downloader::settings::{DownloaderSettings, NativeDownloaderSettings};
use narou_rs::downloader::util::{load_length_limit, sanitize_filename_with_limit};
use narou_rs::error::{NarouError, Result};
use narou_rs::native::novel_repository::NativeNovelRepository;
use narou_rs::platform::{NovelId, NovelMutation};
use narou_rs::virtual_novel::{self, VirtualNovelMembers};

#[derive(clap::Subcommand, Debug)]
pub enum VirtualAction {
    /// Register a virtual novel from existing novels (in the given order).
    Add {
        /// Title of the combined work.
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        author: String,
        #[arg(long, default_value = "")]
        site: String,
        #[arg(long)]
        domain: Option<String>,
        /// Tags for the virtual novel (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Member novels (ID / URL / title / N-code / alias), in order.
        members: Vec<String>,
    },
    /// List registered virtual novels.
    List,
    /// Reorder members using their current 1-based positions.
    Order {
        /// Virtual novel (`vs_1` or its ID).
        target: String,
        /// New order, e.g. `2,1,3` (all members must be listed).
        order: String,
    },
    /// Remove a virtual novel. Member novels are kept.
    Remove {
        /// Virtual novel (`vs_1` or its ID).
        target: String,
    },
}

pub fn cmd_virtual(action: VirtualAction) -> Result<()> {
    db::init_database()?;
    match action {
        VirtualAction::Add {
            title,
            author,
            site,
            domain,
            tags,
            members,
        } => add(&title, &author, &site, domain, &tags, &members),
        VirtualAction::List => list(),
        VirtualAction::Order { target, order } => reorder(&target, &order),
        VirtualAction::Remove { target } => remove(&target),
    }
}

fn add(
    title: &str,
    author: &str,
    site: &str,
    domain: Option<String>,
    tags: &[String],
    members: &[String],
) -> Result<()> {
    if title.trim().is_empty() {
        return Err(NarouError::InvalidTarget("タイトルを指定してください".into()));
    }
    let repository = NativeNovelRepository::new();
    let mut member_ids = Vec::with_capacity(members.len());
    for target in members {
        let id = super::resolve_target_to_id(target)
            .ok_or_else(|| NarouError::Database(format!("作品が見つかりません: {target}")))?;
        if let Some(record) = repository.get_sync(NovelId(id))?
            && record.ncode.as_deref().is_some_and(virtual_novel::is_virtual_ncode)
        {
            return Err(NarouError::InvalidTarget(format!(
                "仮想小説をメンバーにはできません: {target}"
            )));
        }
        if !member_ids.contains(&id) {
            member_ids.push(id);
        }
    }
    if member_ids.is_empty() {
        return Err(NarouError::InvalidTarget("メンバーを指定してください".into()));
    }

    let reservation = db::with_database_mut(|database| database.allocate_id())?;
    let id = reservation.id().0;
    let ncode = virtual_novel::ncode(id);
    let tag = virtual_novel::tag(id);

    let mut record_tags: Vec<String> = tags
        .iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect();
    record_tags.push(tag);
    record_tags.sort();
    record_tags.dedup();

    let general_all_no = member_ids
        .iter()
        .filter_map(|member| repository.get_sync(NovelId(*member)).ok().flatten())
        .filter_map(|record| record.general_all_no)
        .sum::<i64>();
    let limit = load_length_limit("folder-length-limit", Some(50));
    let file_title = sanitize_filename_with_limit(title, limit);
    let file_title = if file_title.is_empty() {
        ncode.clone()
    } else {
        file_title
    };
    let now = Utc::now();
    let record = narou_rs::db::NovelRecord {
        id,
        author: author.to_string(),
        title: title.to_string(),
        file_title,
        toc_url: format!("https://narou.rs/virtual/{ncode}"),
        sitename: site.to_string(),
        novel_type: 0,
        end: true,
        last_update: now,
        new_arrivals_date: None,
        use_subdirectory: NativeDownloaderSettings.download_use_subdirectory(),
        general_firstup: None,
        novelupdated_at: None,
        general_lastup: None,
        last_mail_date: None,
        tags: record_tags,
        ncode: Some(ncode.clone()),
        domain,
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

    repository.apply_batch_sync(vec![NovelMutation::Upsert(record)])?;
    let saved = narou_rs::native::virtual_novel::update(|members| {
        members
            .insert(id, member_ids.clone())
            .map_err(NarouError::Platform)
    });
    if let Err(error) = saved {
        let _ = repository.apply_batch_sync(vec![NovelMutation::Remove(NovelId(id))]);
        return Err(error);
    }
    println!("バーチャル小説を登録しました: {ncode} {title}");
    for (position, member) in member_ids.iter().enumerate() {
        println!("  {}. ID:{member}", position + 1);
    }
    Ok(())
}

fn list() -> Result<()> {
    let members = narou_rs::native::virtual_novel::load()?;
    if members.is_empty() {
        println!("バーチャル小説は登録されていません");
        return Ok(());
    }
    let repository = NativeNovelRepository::new();
    for (virtual_id, member_ids) in &members.groups {
        let ncode = virtual_novel::ncode(*virtual_id);
        let record = repository.get_sync(NovelId(*virtual_id))?.ok_or_else(|| {
            NarouError::Database(format!("仮想小説 {ncode} のレコードがありません"))
        })?;
        println!(
            "{ncode}  {}  ({} 作品, 非表示タグ: {})",
            record.title,
            member_ids.len(),
            virtual_novel::tag(*virtual_id)
        );
        for (position, member) in member_ids.iter().enumerate() {
            let title = repository
                .get_sync(NovelId(*member))?
                .map(|record| record.title)
                .unwrap_or_else(|| "<missing>".to_string());
            println!("  {}. ID:{member} {title}", position + 1);
        }
    }
    Ok(())
}

fn reorder(target: &str, order: &str) -> Result<()> {
    let virtual_id = resolve_virtual_id(target)?;
    let positions: Vec<usize> = order
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<usize>()
                .map_err(|_| NarouError::InvalidTarget(format!("順序が不正です: {part}")))
        })
        .collect::<Result<Vec<_>>>()?;
    let current = narou_rs::native::virtual_novel::load()?;
    let current_members = current
        .get(virtual_id)
        .ok_or_else(|| NarouError::Database(format!("仮想小説 vs_{virtual_id} がありません")))?
        .to_vec();
    if positions.len() != current_members.len() {
        return Err(NarouError::InvalidTarget(
            "現在のメンバーを全件指定してください".into(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut ordered = Vec::with_capacity(positions.len());
    for position in positions {
        if position == 0 || position > current_members.len() {
            return Err(NarouError::InvalidTarget(format!(
                "順序 {position} は 1〜{} の範囲で指定してください",
                current_members.len()
            )));
        }
        if !seen.insert(position) {
            return Err(NarouError::InvalidTarget(format!(
                "順序 {position} が重複しています"
            )));
        }
        ordered.push(current_members[position - 1]);
    }
    let after = narou_rs::native::virtual_novel::update(|members| {
        members
            .reorder(virtual_id, ordered)
            .map_err(NarouError::Platform)
    })?;
    let _ = after;
    println!("仮想小説 vs_{virtual_id} の順序を変更しました");
    Ok(())
}

fn remove(target: &str) -> Result<()> {
    let virtual_id = resolve_virtual_id(target)?;
    let repository = NativeNovelRepository::new();
    let _ = narou_rs::native::virtual_novel::update(|members| {
        if members.remove(virtual_id).is_none() {
            return Err(NarouError::Database(format!(
                "仮想小説 vs_{virtual_id} は登録されていません"
            )));
        }
        Ok(())
    })?;
    repository.apply_batch_sync(vec![NovelMutation::Remove(NovelId(virtual_id))])?;
    println!("仮想小説 vs_{virtual_id} を削除しました（メンバーは残します）");
    Ok(())
}

fn resolve_virtual_id(target: &str) -> Result<i64> {
    let target = target.trim();
    if let Some(id) = virtual_novel::id_from_ncode(target) {
        return Ok(id);
    }
    if let Ok(id) = target.parse::<i64>() {
        let record = NativeNovelRepository::new().get_sync(NovelId(id))?;
        if record
            .and_then(|record| record.ncode)
            .is_some_and(|ncode| virtual_novel::is_virtual_ncode(&ncode))
        {
            return Ok(id);
        }
    }
    if let Some(id) = super::resolve_target_to_id(target) {
        return Ok(id);
    }
    Err(NarouError::Database(format!(
        "仮想小説が見つかりません: {target}"
    )))
}

/// テスト・Web から使う、メンバーを表示一覧から除外するための ID 集合。
pub fn hidden_member_ids() -> std::collections::HashSet<i64> {
    narou_rs::native::virtual_novel::load()
        .map(|members| members.all_member_ids())
        .unwrap_or_default()
}

#[allow(dead_code)]
fn _members_type_check(_: &VirtualNovelMembers) {}
