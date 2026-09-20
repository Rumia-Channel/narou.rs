//! `narou login` — login credentials shared with a browser machine.
//!
//! The browser half lives in the separate `narou_rs_login` executable, which
//! runs where the browser is and writes an export envelope. This command is the
//! receiving end: it imports that envelope into the library (encrypted at rest
//! with the library login key), exports the stored credentials for another
//! machine, lists them, or clears them.

use std::collections::BTreeMap;
use std::io::Read as _;

use narou_rs::error::{NarouError, Result};
use narou_rs::login::{build_export, parse_export};
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
    /// Store one host's cookie header (reads stdin when --cookie is omitted).
    Set {
        /// Request host, for example `ncode.syosetu.com`.
        host: String,
        /// `Cookie:` header value as copied from the browser.
        #[arg(long)]
        cookie: Option<String>,
    },
    /// Drop one host, or every host when none is given.
    Clear {
        /// Request host; omitted clears everything.
        host: Option<String>,
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
        LoginAction::Set { host, cookie } => set(&store, &host, cookie),
        LoginAction::Clear { host } => clear(&store, host.as_deref()),
    }
}

fn list(store: &InventoryCookieStore) -> Result<()> {
    let cookies = store.load_all()?;
    if cookies.is_empty() {
        println!("保存されたログイン情報はありません。");
        println!("  narou_rs_login <サイト> で取得し、narou login import <ファイル> で取り込めます。");
        return Ok(());
    }
    println!("保存されたログイン情報: {} サイト", cookies.len());
    for (host, cookie) in &cookies {
        let state = if store.is_encrypted(host)? {
            "暗号化済み"
        } else {
            "平文(次回保存時に暗号化)"
        };
        println!("  {host:<32} {} [{state}]", mask_cookie(cookie));
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
    let cookies = parse_export(&text, passphrase)?;
    if cookies.is_empty() {
        println!("{file} に Cookie が含まれていません。");
        return Ok(());
    }
    let hosts = cookies.len();
    let stored = if replace {
        store.replace_all(&cookies)?
    } else {
        store.merge(&cookies)?
    };
    println!("ログイン情報を取り込みました: {hosts} サイト");
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
    let cookies = store.load_all()?;
    if cookies.is_empty() {
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
        &cookies,
        passphrase,
        &exported_at,
        library.as_deref(),
    )?;
    std::fs::write(file, text)?;
    println!("ログイン情報を書き出しました: {file} ({} サイト)", cookies.len());
    if passphrase.is_none() {
        println!("  平文で書き出しました。移動先では速やかに取り込んでください。");
        println!("  暗号化するには --passphrase を指定します。");
    }
    Ok(())
}

fn set(store: &InventoryCookieStore, host: &str, cookie: Option<String>) -> Result<()> {
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
    let host = narou_rs::platform::normalize_cookie_host(host);
    store.merge(&BTreeMap::from([(host.clone(), cookie.trim().to_string())]))?;
    println!("{host} のログイン情報を保存しました ({})", store.key_source()?.describe());
    Ok(())
}

fn clear(store: &InventoryCookieStore, host: Option<&str>) -> Result<()> {
    match host {
        Some(host) => {
            let host = narou_rs::platform::normalize_cookie_host(host);
            if store.remove(&host)? {
                println!("{host} のログイン情報を削除しました。");
            } else {
                println!("{host} のログイン情報は保存されていません。");
            }
        }
        None => {
            let count = store.load_all()?.len();
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
        let source = BTreeMap::from([("example.com".to_string(), "sid=abc".to_string())]);
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
        assert_eq!(store.load_all().unwrap(), source);
        assert!(store.is_encrypted("example.com").unwrap());

        let out = temp.path().join("out.yaml");
        let out = out.to_string_lossy().into_owned();
        export(&store, &out, Some("hunter2"), false).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(!text.contains("sid=abc"), "the export is encrypted");
        assert_eq!(parse_export(&text, Some("hunter2")).unwrap(), source);

        // The clear-text form is what a machine without a passphrase reads.
        export(&store, &out, Some("hunter2"), true).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("sid=abc"));
        assert_eq!(parse_export(&text, None).unwrap(), source);
    }

    #[test]
    fn set_clear_and_list_follow_the_stored_hosts() {
        let _legacy = legacy_yaml_guard();
        let (_temp, _guard, store) = library();

        set(&store, "Ncode.Syosetu.com", Some(" over18=yes; ses=1 ".to_string())).unwrap();
        assert_eq!(
            store.load_all().unwrap().get("ncode.syosetu.com").map(String::as_str),
            Some("over18=yes; ses=1")
        );
        assert!(set(&store, "example.com", Some("   ".to_string())).is_err());

        assert!(store.remove("ncode.syosetu.com").unwrap());
        assert!(!store.remove("ncode.syosetu.com").unwrap());
        assert!(store.load_all().unwrap().is_empty());
    }
}
