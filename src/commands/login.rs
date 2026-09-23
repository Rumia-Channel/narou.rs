//! `narou login` — login credentials shared with a browser machine.
//!
//! The browser half lives in the separate `narou_rs_login` executable, which
//! runs where the browser is and writes an export envelope. This command is the
//! receiving end: it imports that envelope into the library (encrypted at rest
//! with the library login key), exports the stored credentials for another
//! machine, lists them, or clears them.

use std::io::Read as _;

use narou_rs::error::{NarouError, Result};
use narou_rs::login::{build_export, parse_export};
use narou_rs::login::group_credentials;
use narou_rs::native::cookie_store::InventoryCookieStore;

/// Subcommands of `narou login`.
#[derive(clap::Subcommand, Debug)]
pub enum LoginAction {
    /// List stored hosts (values are masked).
    List,
    /// Import an export written by `narou_rs_login`.
    Import {
        /// Export file (YAML) to read.
        file: String,
        /// Passphrase of an encrypted export.
        #[arg(long)]
        passphrase: Option<String>,
        /// Replace every stored host instead of merging.
        #[arg(long, default_value_t = false)]
        replace: bool,
    },
    /// Write the stored credentials to an export file.
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
    /// Replace one host's login credential (reads stdin when --cookie is omitted).
    Set {
        /// Request host, for example `ncode.syosetu.com`.
        host: String,
        /// `Cookie:` header value as copied from the browser.
        #[arg(long)]
        cookie: Option<String>,
        /// Label shown in the list ("メイン", "R18用", …).
        #[arg(long)]
        label: Option<String>,
    },
    /// Append another credential to a host, tried after the ones already there.
    Add {
        /// Request host, for example `ncode.syosetu.com`.
        host: String,
        /// `Cookie:` header value as copied from the browser.
        #[arg(long)]
        cookie: Option<String>,
        /// Label shown in the list ("メイン", "R18用", …).
        #[arg(long)]
        label: Option<String>,
    },
    /// Reorder a host's credentials, e.g. `-- 1,2,0` (current positions).
    Order {
        /// Request host whose order is being changed.
        host: String,
        /// Current positions in their new order, comma separated.
        order: String,
    },
    /// Drop one credential, one host, or every host.
    Clear {
        /// Request host; omitted clears everything.
        host: Option<String>,
        /// Drop only this credential (1-based position in `narou login list`).
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
        } => import(&store, &file, passphrase.as_deref(), replace),
        LoginAction::Export {
            file,
            passphrase,
            clear_text,
        } => export(&store, &file, passphrase.as_deref(), clear_text),
        LoginAction::Set {
            host,
            cookie,
            label,
        } => set(&store, &host, cookie, label, false),
        LoginAction::Add {
            host,
            cookie,
            label,
        } => set(&store, &host, cookie, label, true),
        LoginAction::Order { host, order } => reorder(&store, &host, &order),
        LoginAction::Clear { host, index } => clear(&store, host.as_deref(), index),
    }
}

fn list(store: &InventoryCookieStore) -> Result<()> {
    let stored = store.credentials_by_host()?;
    if stored.is_empty() {
        println!("保存されたログイン情報はありません。");
        println!("  narou_rs_login <サイト> で取得し、narou login import <ファイル> で取り込めます。");
        return Ok(());
    }
    let total: usize = stored.values().map(Vec::len).sum();
    println!("保存されたログイン情報: {} サイト / {total} 件", stored.len());
    for (host, credentials) in &stored {
        let state = if store.is_encrypted(host)? {
            "暗号化済み"
        } else {
            "平文(次回保存時に暗号化)"
        };
        let encrypted = [""; 0];
        let _ = encrypted;
        println!("  {host:<32} [{state}]");
        for (index, credential) in credentials.iter().enumerate() {
            println!(
                "    {}. {:<16} [{}] {}",
                index + 1,
                credential.display_name(),
                credential.short_id(),
                mask_cookie(&credential.cookie)
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
) -> Result<()> {
    let text = read_file(file)?;
    let credentials = parse_export(&text, passphrase)?;
    if credentials.is_empty() {
        println!("{file} に Cookie が含まれていません。");
        return Ok(());
    }
    let count = credentials.len();
    let grouped = group_credentials(credentials);
    let hosts = grouped.len();
    let stored = if replace {
        store.replace_credentials(&grouped)?
    } else {
        store.merge_credentials(&grouped)?
    };
    println!("ログイン情報を取り込みました: {count} 件 / {hosts} サイト");
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
    let stored = store.credentials_by_host()?;
    let credentials: Vec<_> = stored.into_values().flatten().collect();
    if credentials.is_empty() {
        return Err(NarouError::Login(
            "保存されたログイン情報がありません。".to_string(),
        ));
    }
    let passphrase = if clear_text { None } else { passphrase };
    let exported_at = chrono::Local::now().to_rfc3339();
    let library = std::env::current_dir()
        .ok()
        .and_then(|dir| dir.file_name().map(|name| name.to_string_lossy().into_owned()));
    let text = build_export(
        &credentials,
        passphrase,
        &exported_at,
        library.as_deref(),
    )?;
    std::fs::write(file, text)?;
    println!(
        "ログイン情報を書き出しました: {file} ({} 件)",
        credentials.len()
    );
    if passphrase.is_none() {
        println!("  平文で書き出しました。移動先では速やかに取り込んでください。");
        println!("  暗号化するには --passphrase を指定します。");
    }
    Ok(())
}

fn read_cookie_argument(cookie: Option<String>) -> Result<String> {
    let cookie = match cookie {
        Some(cookie) => cookie,
        None => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .map_err(|error| NarouError::Login(format!("Cookie を読み込めません: {error}")))?;
            buffer
        }
    };
    if cookie.trim().is_empty() {
        return Err(NarouError::Login("Cookie が空です。".to_string()));
    }
    Ok(cookie.trim().to_string())
}

fn set(
    store: &InventoryCookieStore,
    host: &str,
    cookie: Option<String>,
    label: Option<String>,
    append: bool,
) -> Result<()> {
    let cookie = read_cookie_argument(cookie)?;
    let host = narou_rs::platform::normalize_cookie_host(host);
    let credential = narou_rs::platform::LoginCredential::new(host.clone(), cookie)
        .with_label(label)
        .with_added_at(Some(chrono::Local::now().to_rfc3339()));
    let credentials = if append {
        let mut credentials = store.credentials_for(&host)?;
        if credentials.iter().any(|seen| seen.same_cookie(&credential)) {
            println!("{host} には同じログイン情報が既に保存されています。");
            return Ok(());
        }
        credentials.push(credential);
        credentials
    } else {
        vec![credential]
    };
    let count = credentials.len();
    store.save_credentials_for(&host, &credentials)?;
    println!(
        "{host} のログイン情報を保存しました ({count} 件, {})",
        store.key_source()?.describe()
    );
    Ok(())
}

/// 現在の並び（1 始まり）を新しい順序に並べ替える。
fn reorder(store: &InventoryCookieStore, host: &str, order: &str) -> Result<()> {
    let host = narou_rs::platform::normalize_cookie_host(host);
    let stored = store.credentials_for(&host)?;
    if stored.is_empty() {
        return Err(NarouError::Login(format!(
            "{host} のログイン情報は保存されていません。"
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
    store.save_credentials_for(&host, &reordered)?;
    println!("{host} の試行順を変更しました:");
    for (index, credential) in reordered.iter().enumerate() {
        println!("  {}. {}", index + 1, credential.display_name());
    }
    Ok(())
}

fn clear(store: &InventoryCookieStore, host: Option<&str>, index: Option<usize>) -> Result<()> {
    match host {
        Some(host) => {
            let host = narou_rs::platform::normalize_cookie_host(host);
            match index {
                Some(index) => {
                    let mut credentials = store.credentials_for(&host)?;
                    if index == 0 || index > credentials.len() {
                        return Err(NarouError::Login(format!(
                            "{host} の {index} 番目のログイン情報はありません。"
                        )));
                    }
                    credentials.remove(index - 1);
                    store.save_credentials_for(&host, &credentials)?;
                    println!("{host} の {index} 番目のログイン情報を削除しました。");
                }
                None => {
                    if store.remove(&host)? {
                        println!("{host} のログイン情報を削除しました。");
                    } else {
                        println!("{host} のログイン情報は保存されていません。");
                    }
                }
            }
        }
        None => {
            let count = store.credentials_by_host()?.len();
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
    use narou_rs::platform::LoginCredential;
    use super::*;
    use crate::test_support::{legacy_yaml_guard, set_current_dir_for_test};


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
        let source = vec![LoginCredential::new("example.com", "sid=abc")];
        let exported = build_export(
            &source,
            Some("hunter2"),
            "2026-09-20T00:00:00+09:00",
            None,
        )
        .unwrap();
        std::fs::write(&file, exported).unwrap();

        assert!(import(&store, &file, Some("wrong"), false).is_err());
        import(&store, &file, Some("hunter2"), false).unwrap();
        let stored = store.credentials_for("example.com").unwrap();
        assert_eq!(stored.len(), source.len());
        assert_eq!(stored[0].cookie, source[0].cookie);
        assert!(!stored[0].id.is_empty(), "取り込み時に識別子が振られる");
        assert!(store.is_encrypted("example.com").unwrap());

        let out = temp.path().join("out.yaml");
        let out = out.to_string_lossy().into_owned();
        export(&store, &out, Some("hunter2"), false).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(!text.contains("sid=abc"), "the export is encrypted");
        let exported = parse_export(&text, Some("hunter2")).unwrap();
        assert_eq!(exported[0].cookie, source[0].cookie);
        assert_eq!(exported[0].id, stored[0].id, "書き出しにも識別子が乗る");

        // The clear-text form is what a machine without a passphrase reads.
        export(&store, &out, Some("hunter2"), true).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("sid=abc"));
        let exported = parse_export(&text, None).unwrap();
        assert_eq!(exported[0].cookie, source[0].cookie);
    }

    #[test]
    fn set_clear_and_list_follow_the_stored_hosts() {
        let _legacy = legacy_yaml_guard();
        let (_temp, _guard, store) = library();

        set(
            &store,
            "Ncode.Syosetu.com",
            Some(" over18=yes; ses=1 ".to_string()),
            None,
            false,
        )
        .unwrap();
        let stored = store.credentials_for("ncode.syosetu.com").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].cookie, "over18=yes; ses=1");
        assert!(set(&store, "example.com", Some("   ".to_string()), None, false).is_err());

        // 追加は末尾に積まれ、並べ替えで順序を変えられる。
        set(
            &store,
            "ncode.syosetu.com",
            Some("over18=yes; ses=2".to_string()),
            Some("サブ".to_string()),
            true,
        )
        .unwrap();
        let stored = store.credentials_for("ncode.syosetu.com").unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1].display_name(), "サブ");
        reorder(&store, "ncode.syosetu.com", "2,1").unwrap();
        let stored = store.credentials_for("ncode.syosetu.com").unwrap();
        assert_eq!(stored[0].display_name(), "サブ");
        assert!(reorder(&store, "ncode.syosetu.com", "1,3").is_err());

        clear(&store, Some("ncode.syosetu.com"), Some(1)).unwrap();
        let stored = store.credentials_for("ncode.syosetu.com").unwrap();
        assert_eq!(stored.len(), 1);

        assert!(store.remove("ncode.syosetu.com").unwrap());
        assert!(!store.remove("ncode.syosetu.com").unwrap());
        assert!(store.credentials_by_host().unwrap().is_empty());
    }
}
