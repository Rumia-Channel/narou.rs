//! `narou hidden` — 作品 / タグ / 作者 / サイトの非表示設定。
//!
//! 非表示は一覧表示だけを制御する。更新・変換・バックアップなどの内部処理は
//! 非表示作品も対象にする (`list --show-hidden` で確認できる)。

use narou_rs::error::{NarouError, Result};
use narou_rs::hidden::HiddenState;

/// Subcommands of `narou hidden`.
#[derive(clap::Subcommand, Debug)]
pub enum HiddenAction {
    /// Hide novels (targets) and/or tags/authors/sites.
    Add {
        /// Tag names to hide (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Author names to hide (repeatable).
        #[arg(long = "author")]
        authors: Vec<String>,
        /// Site domains or site names to hide (repeatable).
        #[arg(long = "site")]
        sites: Vec<String>,
        /// Novel IDs, URLs, titles, n-codes, or aliases.
        targets: Vec<String>,
    },
    /// Unhide novels (targets) and/or tags/authors/sites.
    Remove {
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long = "author")]
        authors: Vec<String>,
        #[arg(long = "site")]
        sites: Vec<String>,
        targets: Vec<String>,
    },
    /// Show every hidden entry.
    List,
    /// Clear every hidden entry.
    Clear,
}

/// Run `narou hidden`.
pub fn cmd_hidden(action: HiddenAction) -> Result<()> {
    match action {
        HiddenAction::Add {
            tags,
            authors,
            sites,
            targets,
        } => change(true, &tags, &authors, &sites, &targets),
        HiddenAction::Remove {
            tags,
            authors,
            sites,
            targets,
        } => change(false, &tags, &authors, &sites, &targets),
        HiddenAction::List => list(),
        HiddenAction::Clear => {
            narou_rs::native::hidden::save(&HiddenState::default())?;
            println!("非表示設定をすべて解除しました");
            Ok(())
        }
    }
}

fn change(
    hide: bool,
    tags: &[String],
    authors: &[String],
    sites: &[String],
    targets: &[String],
) -> Result<()> {
    let ids = targets
        .iter()
        .map(|target| {
            super::resolve_target_to_id(target)
                .ok_or_else(|| NarouError::Database(format!("作品が見つかりません: {target}")))
        })
        .collect::<Result<Vec<_>>>()?;

    let state = narou_rs::native::hidden::update(|state| {
        for &id in &ids {
            if hide {
                state.add_novel(id);
            } else {
                state.remove_novel(id);
            }
        }
        for tag in tags {
            if hide {
                state.add_tag(tag);
            } else {
                state.remove_tag(tag);
            }
        }
        for author in authors {
            if hide {
                state.add_author(author);
            } else {
                state.remove_author(author);
            }
        }
        for site in sites {
            if hide {
                state.add_site(site);
            } else {
                state.remove_site(site);
            }
        }
    })?;

    let verb = if hide { "非表示にしました" } else { "表示に戻しました" };
    for &id in &ids {
        println!("ID:{id} を{verb}");
    }
    for tag in tags {
        println!("タグ「{tag}」を{verb}");
    }
    for author in authors {
        println!("作者「{author}」を{verb}");
    }
    for site in sites {
        println!("サイト「{site}」を{verb}");
    }
    if state.is_empty() {
        println!("非表示設定は空です");
    }
    Ok(())
}

fn list() -> Result<()> {
    let state = narou_rs::native::hidden::load()?;
    if state.is_empty() {
        println!("非表示設定はありません");
        return Ok(());
    }
    println!("非表示の作品: {}", state.novels.len());
    for id in &state.novels {
        println!("  ID:{id}");
    }
    println!("非表示のタグ: {}", state.tags.len());
    for tag in &state.tags {
        println!("  {tag}");
    }
    println!("非表示の作者: {}", state.authors.len());
    for author in &state.authors {
        println!("  {author}");
    }
    println!("非表示のサイト: {}", state.sites.len());
    for site in &state.sites {
        println!("  {site}");
    }
    Ok(())
}
