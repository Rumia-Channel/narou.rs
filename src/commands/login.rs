//! `narou login` — login credentials shared with a browser machine.
//!
//! The browser half lives in the separate `narou_rs_login` executable, which
//! runs where the browser is and writes an export envelope. This command is the
//! receiving end: it imports that envelope into the library (encrypted at rest
//! with the library login key), exports the stored credentials for another
//! machine, lists them, or clears them.


use narou_rs::error::{NarouError, Result};
use narou_rs::login::{build_export, parse_export};
use narou_rs::native::cookie_store::InventoryCookieStore;

/// Subcommands of `narou login`.
#[derive(clap::Subcommand, Debug)]
pub enum LoginAction {
    /// List stored sites and their logins (values are masked).
    List,
    /// Import an export written by `narou_rs_login`.
    Import {
        /// Export file (YAML) to read.
        file: String,
        /// Passphrase of an encrypted export.
        #[arg(long)]
        passphrase: Option<String>,
        /// Replace every stored site instead of merging.
        #[arg(long, default_value_t = false)]
        replace: bool,
        /// Name the logins this file brings in ("本垢", "サブ垢", …).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// Write the stored logins to an export file.
    Export {
        /// Export file (YAML) to write.
        file: String,
        /// Encrypt the export with this passphrase.
        #[arg(long)]
        passphrase: Option<String>,
        /// Write clear text even though a passphrase was given.
        #[arg(long = "clear-text", default_value_t = false)]
        clear_text: bool,
    },
    /// Name a stored login, e.g. `narou login rename www.pixiv.net 2 サブ垢`.
    Rename {
        /// Site the login belongs to, for example `www.pixiv.net`.
        site: String,
        /// Position in `narou login list` (1 始まり).
        index: usize,
        /// Name to show ("Pixiv1", "メインアカウント", …). Empty clears it.
        label: String,
    },
    /// Reorder a site's logins, e.g. `-- 2,1` (current positions).
    Order {
        /// Site whose order is being changed.
        site: String,
        /// Current positions in their new order, comma separated.
        order: String,
    },
    /// Drop one login, one site, or everything.
    Clear {
        /// Site; omitted clears everything.
        site: Option<String>,
        /// Drop only this login (position in `narou login list`, 1 始まり).
        #[arg(long)]
        index: Option<usize>,
    },
}

/// Run `narou login`.
pub fn cmd_login(action: LoginAction) -> Result<()> {
    let store = InventoryCookieStore::for_current_root()?;
    match action {
        LoginAction::List => list(&store),
        LoginAction::Import {
            file,
            passphrase,
            replace,
            name,
        } => import(&store, &file, passphrase.as_deref(), replace, name.as_deref()),
        LoginAction::Export {
            file,
            passphrase,
            clear_text,
        } => export(&store, &file, passphrase.as_deref(), clear_text),
        LoginAction::Rename { site, index, label } => rename(&store, &site, index, &label),
        LoginAction::Order { site, order } => reorder(&store, &site, &order),
        LoginAction::Clear { site, index } => clear(&store, site.as_deref(), index),
    }
}

fn list(store: &InventoryCookieStore) -> Result<()> {
    let stored = store.groups_by_site()?;
    if stored.is_empty() {
        println!("保存されたログイン情報はありません。");
        println!("  narou_rs_login <サイト> --export <ファイル> で取得し、narou login import <ファイル> で取り込めます。");
        return Ok(());
    }
    let total: usize = stored.values().map(Vec::len).sum();
    println!("保存されたログイン情報: {} サイト / {total} 件", stored.len());
    for (site, groups) in &stored {
        let state = if store.is_encrypted(site)? {
            "暗号化済み"
        } else {
            "平文(次回保存時に暗号化)"
        };
        println!("  {site} [{state}]");
        for (index, group) in groups.iter().enumerate() {
            let hosts: Vec<&str> = group.cookies.iter().map(|entry| entry.host.as_str()).collect();
            println!(
                "    {}. {} [{}] {} ホスト: {}",
                index + 1,
                group.display_name(),
                group.short_id(),
                group.cookies.len(),
                hosts.join(", ")
            );
        }
    }
    println!();
    println!("鍵: {}", store.key_source()?.describe());
    Ok(())
}

fn import(
    store: &InventoryCookieStore,
    file: &str,
    passphrase: Option<&str>,
    replace: bool,
    name: Option<&str>,
) -> Result<()> {
    let text = read_file(file)?;
    let mut sites = parse_export(&text, passphrase)?;
    if let Some(name) = name {
        narou_rs::login::apply_import_name(&mut sites, name);
    }
    if sites.is_empty() {
        println!("{file} にログイン情報が含まれていません。");
        return Ok(());
    }
    let logins: usize = sites.values().map(Vec::len).sum();
    let sites_count = sites.len();
    let stored = if replace {
        store.replace_groups(&sites)?
    } else {
        store.merge_groups(&sites)?
    };
    println!("ログイン情報を取り込みました: {logins} 件 / {sites_count} サイト");
    println!("  保存済み: {stored} サイト ({})", store.key_source()?.describe());
    if replace {
        println!("  取り込みに含まれないサイトの情報は削除されました。");
    }
    Ok(())
}

fn export(
    store: &InventoryCookieStore,
    file: &str,
    passphrase: Option<&str>,
    clear_text: bool,
) -> Result<()> {
    let sites = store.groups_by_site()?;
    let logins: usize = sites.values().map(Vec::len).sum();
    if logins == 0 {
        return Err(NarouError::Login(
            "保存されたログイン情報がありません。".to_string(),
        ));
    }
    let passphrase = if clear_text { None } else { passphrase };
    let exported_at = chrono::Local::now().to_rfc3339();
    let library = std::env::current_dir()
        .ok()
        .and_then(|dir| dir.file_name().map(|name| name.to_string_lossy().into_owned()));
    let text = build_export(&sites, passphrase, &exported_at, library.as_deref())?;
    std::fs::write(file, text)?;
    println!("ログイン情報を書き出しました: {file} ({logins} 件)");
    if passphrase.is_none() {
        println!("  平文で書き出しました。移動先では速やかに取り込んでください。");
        println!("  暗号化するには --passphrase を指定します。");
    }
    Ok(())
}

/// 保存済みのログインの位置 (1 始まり) を解決する。
fn group_at(
    store: &InventoryCookieStore,
    site: &str,
    index: usize,
) -> Result<(Vec<narou_rs::platform::LoginGroup>, usize)> {
    let site = site.trim().to_ascii_lowercase();
    let groups = store.groups_for(&site)?;
    if index == 0 || index > groups.len() {
        return Err(NarouError::Login(format!(
            "{site} の {index} 番目のログイン情報はありません。"
        )));
    }
    Ok((groups, index - 1))
}

fn rename(store: &InventoryCookieStore, site: &str, index: usize, label: &str) -> Result<()> {
    let (mut groups, position) = group_at(store, site, index)?;
    let label = label.trim();
    groups[position].label = (!label.is_empty()).then(|| label.to_string());
    let name = groups[position].display_name().to_string();
    store.save_groups_for(site, &groups)?;
    println!("{site} の {index} 番目を「{name}」にしました。");
    Ok(())
}

fn reorder(store: &InventoryCookieStore, site: &str, order: &str) -> Result<()> {
    let site = site.trim().to_ascii_lowercase();
    let stored = store.groups_for(&site)?;
    if stored.is_empty() {
        return Err(NarouError::Login(format!(
            "{site} のログイン情報は保存されていません。"
        )));
    }
    let positions: Vec<usize> = order
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<usize>()
                .ok()
                .filter(|position| *position >= 1 && *position <= stored.len())
                .ok_or_else(|| {
                    NarouError::Login(format!(
                        "並び順は 1〜{} の番号をカンマ区切りで指定してください: {order}",
                        stored.len()
                    ))
                })
        })
        .collect::<Result<Vec<_>>>()?;
    if positions.len() != stored.len() {
        return Err(NarouError::Login(format!(
            "並び順は保存済みの {} 件すべてを指定してください: {order}",
            stored.len()
        )));
    }
    let mut seen = vec![false; stored.len()];
    let mut reordered = Vec::with_capacity(stored.len());
    for position in &positions {
        if seen[position - 1] {
            return Err(NarouError::Login(format!(
                "同じ番号が複数回指定されています: {order}"
            )));
        }
        seen[position - 1] = true;
        reordered.push(stored[position - 1].clone());
    }
    store.save_groups_for(&site, &reordered)?;
    println!("{site} の試行順を変更しました:");
    for (index, group) in reordered.iter().enumerate() {
        println!("  {}. {}", index + 1, group.display_name());
    }
    Ok(())
}

fn clear(store: &InventoryCookieStore, site: Option<&str>, index: Option<usize>) -> Result<()> {
    match site {
        Some(site) => {
            let site = site.trim().to_ascii_lowercase();
            match index {
                Some(index) => {
                    let (mut groups, position) = group_at(store, &site, index)?;
                    let removed = groups.remove(position);
                    store.save_groups_for(&site, &groups)?;
                    println!("{site} の「{}」を削除しました。", removed.display_name());
                }
                None => {
                    if store.remove(&site)? {
                        println!("{site} のログイン情報を削除しました。");
                    } else {
                        println!("{site} のログイン情報は保存されていません。");
                    }
                }
            }
        }
        None => {
            let count = store.groups_by_site()?.len();
            store.clear_all()?;
            println!("ログイン情報をすべて削除しました ({count} サイト)。");
        }
    }
    Ok(())
}

fn read_file(path: &str) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|error| NarouError::Login(format!("{path} を読み込めません: {error}")))
}

/// Show which cookies a header carries without printing their values.
#[cfg(test)]
fn mask_cookie(cookie: &str) -> String {
    let pairs = narou_rs::platform::parse_cookie_header(cookie);
    let length = cookie.chars().count();
    if pairs.is_empty() {
        return format!("({length} 文字)");
    }
    let names = pairs
        .iter()
        .map(|(name, _)| format!("{name}=…"))
        .collect::<Vec<_>>()
        .join("; ");
    format!("{names} ({} 件, {length} 文字)", pairs.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use narou_rs::platform::cookie_store::{HostCookie, LoginGroup};
    use crate::test_support::{legacy_yaml_guard, set_current_dir_for_test};
    use std::collections::BTreeMap;

    /// A throwaway library with the process working directory pointed at it.
    ///
    /// The guard must be bound before the store is built: the store captures
    /// the library root from the working directory.
    fn library() -> (
        tempfile::TempDir,
        crate::test_support::CurrentDirGuard,
        InventoryCookieStore,
    ) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let guard = set_current_dir_for_test(temp.path());
        let store = InventoryCookieStore::for_current_root().unwrap();
        (temp, guard, store)
    }

    fn login(site: &str, cookie: &str, label: Option<&str>) -> LoginGroup {
        LoginGroup::new(
            site,
            vec![HostCookie {
                host: site.to_string(),
                cookie: cookie.to_string(),
            }],
        )
        .with_label(label.map(str::to_string))
    }

    #[test]
    fn masks_cookie_values_but_keeps_the_names() {
        let masked = mask_cookie("over18=yes; ses=abcdef");
        assert_eq!(masked, "over18=…; ses=… (2 件, 22 文字)");
        assert!(!masked.contains("abcdef"));
        assert_eq!(mask_cookie(""), "(0 文字)");
    }

    #[test]
    fn import_and_export_round_trip_through_a_passphrase() {
        let _legacy = legacy_yaml_guard();
        let (temp, _guard, store) = library();

        let file = temp.path().join("login.yaml");
        let file = file.to_string_lossy().into_owned();
        let sites = BTreeMap::from([(
            "example.com".to_string(),
            vec![login("example.com", "sid=abc", Some("本垢"))],
        )]);
        let exported = build_export(
            &sites,
            Some("hunter2"),
            "2026-09-20T00:00:00+09:00",
            None,
        )
        .unwrap();
        std::fs::write(&file, exported).unwrap();

        assert!(import(&store, &file, Some("wrong"), false, None).is_err());
        import(&store, &file, Some("hunter2"), false, None).unwrap();
        let stored = store.groups_for("example.com").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].cookies[0].cookie, "sid=abc");
        assert_eq!(stored[0].display_name(), "本垢");
        assert!(!stored[0].id.is_empty(), "取り込み時に識別子が振られる");
        assert!(store.is_encrypted("example.com").unwrap());

        let out = temp.path().join("out.yaml");
        let out = out.to_string_lossy().into_owned();
        export(&store, &out, Some("hunter2"), false).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(!text.contains("sid=abc"), "the export is encrypted");
        let exported = parse_export(&text, Some("hunter2")).unwrap();
        assert_eq!(exported["example.com"][0].cookies[0].cookie, "sid=abc");
        assert_eq!(
            exported["example.com"][0].id, stored[0].id,
            "書き出しにも識別子が乗る"
        );

        // パスフレーズ無しで読む平文形式。
        export(&store, &out, Some("hunter2"), true).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("sid=abc"));
        let exported = parse_export(&text, None).unwrap();
        assert_eq!(exported["example.com"][0].cookies[0].cookie, "sid=abc");
    }

    #[test]
    fn rename_order_and_remove_follow_the_stored_logins() {
        let _legacy = legacy_yaml_guard();
        let (_temp, _guard, store) = library();

        store
            .save_groups_for(
                "www.pixiv.net",
                &[
                    login("www.pixiv.net", "PHPSESSID=main", None),
                    login("www.pixiv.net", "PHPSESSID=sub", None),
                ],
            )
            .unwrap();

        // 名前は利用者が後から付けられる (番号は 1 始まり)。
        rename(&store, "www.pixiv.net", 2, "サブ").unwrap();
        let stored = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(stored[0].display_name(), "www.pixiv.net", "名前なしはサイト名");
        assert_eq!(stored[1].display_name(), "サブ");
        assert!(rename(&store, "www.pixiv.net", 9, "").is_err());

        // 並べ替えは現在の並びの添字 (1 始まり) で指定する。
        reorder(&store, "www.pixiv.net", "2,1").unwrap();
        let stored = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(stored[0].display_name(), "サブ");
        assert!(reorder(&store, "www.pixiv.net", "1").is_err(), "件数が合わない");

        clear(&store, Some("www.pixiv.net"), Some(1)).unwrap();
        let stored = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].display_name(), "www.pixiv.net");

        clear(&store, Some("www.pixiv.net"), Some(1)).unwrap();
        assert!(store.groups_for("www.pixiv.net").unwrap().is_empty());
    }

    #[test]
    fn import_applies_the_name_it_was_given() {
        let _legacy = legacy_yaml_guard();
        let (temp, _guard, store) = library();

        // 1 回の取得がホストごとに分かれた版 2 のファイル。
        let file = temp.path().join("capture.yaml");
        std::fs::write(
            &file,
            "version: 2\nexported_at: 2026-09-24T00:00:00+09:00\nencrypted: false\n\
             credentials:\n- host: pixiv.net\n  cookie: PHPSESSID=abc\n\
             - host: www.pixiv.net\n  cookie: yuid_b=1\n",
        )
        .unwrap();
        let file = file.to_string_lossy().into_owned();

        import(&store, &file, None, false, Some("本垢")).unwrap();
        let stored = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(stored.len(), 1, "1 ファイル = 1 ログイン: {stored:?}");
        assert_eq!(stored[0].display_name(), "本垢");
        assert_eq!(stored[0].cookies.len(), 2, "ホストは分かれたまま 1 本にまとまる");
        assert_eq!(stored[0].merged_cookie(), "yuid_b=1; PHPSESSID=abc");
    }

    #[test]
    fn list_shows_every_site_without_values() {
        let _legacy = legacy_yaml_guard();
        let (_temp, _guard, store) = library();

        store
            .merge_groups(&BTreeMap::from([
                (
                    "www.pixiv.net".to_string(),
                    vec![login("www.pixiv.net", "PHPSESSID=secret", None)],
                ),
                (
                    "ncode.syosetu.com".to_string(),
                    vec![login("ncode.syosetu.com", "over18=yes; ses=hidden", None)],
                ),
            ]))
            .unwrap();

        list(&store).unwrap();
    }
}
