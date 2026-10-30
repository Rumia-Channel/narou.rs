//! `narou virtual` — 複数の実小説を 1 つのバーチャル小説として登録する。
//!
//! 仮想小説自体も `novels` の通常行で、`ncode = vs_<ID>`。本文は各メンバーの
//! まま保持し、変換時にだけ登録順へ結合する。タグ `vs_<ID>` が必ず付くので、
//! 既存の hidden tag で非表示にできる。

use narou_rs::db;
use narou_rs::error::{NarouError, Result};
use narou_rs::native::novel_repository::NativeNovelRepository;
use narou_rs::native::virtual_novel::{self, NewVirtualNovel};
use narou_rs::platform::NovelId;
use narou_rs::virtual_novel as core;

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
        /// Attribute tag (小説 / 漫画) for a mixed set of members.
        #[arg(long = "attribute")]
        attribute: Option<String>,
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
    /// Add novels to an existing virtual novel (they join at the end, in order).
    Append {
        /// Virtual novel (`vs_1` or its ID).
        target: String,
        /// Member novels (ID / URL / title / N-code / alias).
        members: Vec<String>,
    },
    /// Change the attribute tag (小説 / 漫画) of a mixed virtual novel.
    Attribute {
        /// Virtual novel (`vs_1` or its ID).
        target: String,
        /// Attribute tag (小説 / 漫画).
        attribute: String,
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
            attribute,
            members,
        } => add(&title, &author, &site, domain, &tags, attribute, &members),
        VirtualAction::List => list(),
        VirtualAction::Order { target, order } => reorder(&target, &order),
        VirtualAction::Append { target, members } => append(&target, &members),
        VirtualAction::Attribute { target, attribute } => set_attribute(&target, &attribute),
        VirtualAction::Remove { target } => remove(&target),
    }
}

fn add(
    title: &str,
    author: &str,
    site: &str,
    domain: Option<String>,
    tags: &[String],
    attribute: Option<String>,
    members: &[String],
) -> Result<()> {
    let mut member_ids = Vec::with_capacity(members.len());
    for target in members {
        let id = super::resolve_target_to_id(target)
            .ok_or_else(|| NarouError::Database(format!("作品が見つかりません: {target}")))?;
        if !member_ids.contains(&id) {
            member_ids.push(id);
        }
    }
    let record = virtual_novel::create(NewVirtualNovel {
        title: title.to_string(),
        author: author.to_string(),
        sitename: site.to_string(),
        domain,
        tags: tags.to_vec(),
        members: member_ids,
        attribute,
    })?;
    let ncode = core::ncode(record.id);
    println!("バーチャル小説を登録しました: {ncode} {}", record.title);
    if let Some(members) = virtual_novel::load()?.get(record.id) {
        for (position, member) in members.iter().enumerate() {
            println!("  {}. ID:{member}", position + 1);
        }
    }
    Ok(())
}

fn list() -> Result<()> {
    let members = virtual_novel::load()?;
    if members.is_empty() {
        println!("バーチャル小説は登録されていません");
        return Ok(());
    }
    let repository = NativeNovelRepository::new();
    // 分類は表示機能の ON / OFF とは独立に保持する。
    let states = virtual_novel::attribute_states_in(&members);
    for (virtual_id, member_ids) in &members.groups {
        let ncode = core::ncode(*virtual_id);
        let record = repository.get_sync(NovelId(*virtual_id))?.ok_or_else(|| {
            NarouError::Database(format!("仮想小説 {ncode} のレコードがありません"))
        })?;
        let state = states.get(virtual_id);
        let attribute = state
            .map(|state| state.attribute_tag)
            .unwrap_or(narou_rs::application::attribute_tags::NOVEL_TAG);
        let mixed = state.map(|state| state.mixed).unwrap_or(false);
        println!(
            "{ncode}  {}  ({} 作品, 属性: {attribute}{}, 非表示タグ: {})",
            record.title,
            member_ids.len(),
            if mixed { " (混合)" } else { "" },
            core::tag(*virtual_id)
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

/// 混在するバーチャル小説の属性タグを変更する。
fn set_attribute(target: &str, attribute: &str) -> Result<()> {
    let virtual_id = resolve_virtual_id(target)?;
    let state = virtual_novel::set_attribute(virtual_id, attribute)?;
    println!(
        "仮想小説 vs_{virtual_id} の属性タグを {} に変更しました",
        state.attribute_tag
    );
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
    virtual_novel::reorder_by_positions(virtual_id, &positions)?;
    println!("仮想小説 vs_{virtual_id} の順序を変更しました");
    Ok(())
}

fn append(target: &str, members: &[String]) -> Result<()> {
    let virtual_id = resolve_virtual_id(target)?;
    let mut member_ids = Vec::with_capacity(members.len());
    for member in members {
        let id = super::resolve_target_to_id(member)
            .ok_or_else(|| NarouError::Database(format!("作品が見つかりません: {member}")))?;
        if !member_ids.contains(&id) {
            member_ids.push(id);
        }
    }
    let added = member_ids.len();
    virtual_novel::add_members(virtual_id, member_ids)?;
    println!("仮想小説 vs_{virtual_id} に {added} 作品を追加しました");
    Ok(())
}

fn remove(target: &str) -> Result<()> {
    let virtual_id = resolve_virtual_id(target)?;
    virtual_novel::remove(virtual_id)?;
    println!("仮想小説 vs_{virtual_id} を削除しました（メンバーは残します）");
    Ok(())
}

fn resolve_virtual_id(target: &str) -> Result<i64> {
    let target = target.trim();
    if let Some(id) = core::id_from_ncode(target) {
        return Ok(id);
    }
    if let Ok(id) = target.parse::<i64>() {
        let record = NativeNovelRepository::new().get_sync(NovelId(id))?;
        if record
            .and_then(|record| record.ncode)
            .is_some_and(|ncode| core::is_virtual_ncode(&ncode))
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

/// 表示一覧から除外するメンバー ID。
pub fn hidden_member_ids() -> std::collections::HashSet<i64> {
    virtual_novel::load()
        .map(|members| members.all_member_ids())
        .unwrap_or_default()
}
