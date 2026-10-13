//! Login credential actions shared by the native Web UI and the Worker.
//!
//! Both runtimes expose the same `/api/login*` surface (native
//! `src/web/login.rs`, Worker `worker_entry/src/webui/login_actions.rs`):
//! import an export envelope, rename a login, reorder a site's logins, and
//! clear a site, one login, or everything. The decisions and messages live
//! here; each runtime only supplies its cookie store and wraps the outcome in
//! its own response envelope.
//!
//! The CLI (`src/commands/login.rs`) keeps its 1-based positions and its own
//! messages, and uses the pure rules below.

use std::collections::BTreeMap;

use crate::error::Result;
use crate::login::{apply_import_name, parse_export};
use crate::platform::cookie_store::{CONCURRENT_EDIT_MESSAGE, replace_site_groups};
use crate::platform::{CookieStore, LoginGroup, normalize_cookie_host};

/// Message for an envelope that carries no logins.
pub const IMPORT_EMPTY_MESSAGE: &str = "書き出しファイルにログイン情報が含まれていません";

/// What an export envelope brought in, with the counts the Web API reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    /// Logins per site, ready to merge or replace.
    pub sites: BTreeMap<String, Vec<LoginGroup>>,
    /// Number of logins.
    pub logins: usize,
    /// Number of sites.
    pub sites_count: usize,
}

impl ImportPlan {
    /// Message for an import that stored `stored` sites.
    pub fn success_message(&self, stored: usize) -> String {
        format!(
            "{} 件を取り込みました ({} サイト / 保存済み {stored} サイト)",
            self.logins, self.sites_count
        )
    }
}

/// Parse an export envelope and name what it brought in.
pub fn plan_import(
    envelope: &str,
    passphrase: Option<&str>,
    name: Option<&str>,
) -> Result<ImportPlan> {
    let mut sites = parse_export(envelope, passphrase)?;
    if let Some(name) = name {
        apply_import_name(&mut sites, name);
    }
    let logins = sites.values().map(Vec::len).sum();
    let sites_count = sites.len();
    Ok(ImportPlan {
        sites,
        logins,
        sites_count,
    })
}

/// Apply a user-visible label to one login (trimmed; empty clears it).
///
/// Returns the name the login shows afterwards.
pub fn apply_login_label<'a>(group: &'a mut LoginGroup, label: &str) -> &'a str {
    let label = label.trim();
    group.label = (!label.is_empty()).then(|| label.to_string());
    group.display_name()
}

/// Why a requested trial order could not be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorderLoginsError {
    /// The request did not name as many positions as there are stored logins.
    CountMismatch,
    /// A position is outside the stored range.
    OutOfRange,
    /// A position is named more than once.
    Duplicate,
}

impl ReorderLoginsError {
    /// Message the Web API shows for this rejection.
    pub fn message(self) -> String {
        match self {
            ReorderLoginsError::CountMismatch => {
                "並び順の指定が保存済みの件数と一致しません".to_string()
            }
            ReorderLoginsError::OutOfRange | ReorderLoginsError::Duplicate => {
                "並び順の指定が不正です".to_string()
            }
        }
    }
}

/// Apply a new trial order — current 0-based positions — to a site's logins.
pub fn apply_login_order(
    stored: &[LoginGroup],
    order: &[usize],
) -> std::result::Result<Vec<LoginGroup>, ReorderLoginsError> {
    if order.len() != stored.len() {
        return Err(ReorderLoginsError::CountMismatch);
    }
    let mut seen = vec![false; stored.len()];
    let mut reordered = Vec::with_capacity(stored.len());
    for index in order {
        if *index >= stored.len() {
            return Err(ReorderLoginsError::OutOfRange);
        }
        if seen[*index] {
            return Err(ReorderLoginsError::Duplicate);
        }
        seen[*index] = true;
        reordered.push(stored[*index].clone());
    }
    Ok(reordered)
}

/// Store an import: merge it into the stored sites, or replace them.
///
/// A replace import never reads the stored map (the old entries are dropped),
/// so an unreadable stored value does not block it; a merge reads it because
/// the existing logins stay.
pub async fn import_logins(
    store: &dyn CookieStore,
    plan: &ImportPlan,
    replace: bool,
) -> std::result::Result<String, String> {
    let stored = if replace {
        let map = replace_site_groups(&plan.sites);
        store
            .save_all_groups(&map)
            .await
            .map_err(|error| error.to_string())?;
        map.len()
    } else {
        store
            .merge_all_groups(&plan.sites)
            .await
            .map_err(|error| error.to_string())?
    };
    Ok(plan.success_message(stored))
}

/// Name one stored login (`POST /api/login/rename`).
pub async fn rename_login(
    store: &dyn CookieStore,
    site: &str,
    index: usize,
    label: &str,
) -> std::result::Result<String, String> {
    let site = normalize_cookie_host(site);
    let expected = store
        .load_groups(&site)
        .await
        .map_err(|error| error.to_string())?;
    let mut groups = expected.clone();
    if index >= groups.len() {
        return Err(missing_login_message(&site, index));
    }
    let name = apply_login_label(&mut groups[index], label).to_string();
    if !store
        .compare_save_groups(&site, &expected, &groups)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err(CONCURRENT_EDIT_MESSAGE.to_string());
    }
    Ok(format!("{site} の {} 番目を「{name}」にしました", index + 1))
}

/// Reorder one site's logins (`POST /api/login/order`).
pub async fn reorder_logins(
    store: &dyn CookieStore,
    site: &str,
    order: &[usize],
) -> std::result::Result<String, String> {
    let site = normalize_cookie_host(site);
    let stored = store
        .load_groups(&site)
        .await
        .map_err(|error| error.to_string())?;
    let reordered = apply_login_order(&stored, order).map_err(|error| error.message())?;
    if !store
        .compare_save_groups(&site, &stored, &reordered)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err(CONCURRENT_EDIT_MESSAGE.to_string());
    }
    Ok(format!("{site} の試行順を変更しました"))
}

/// Drop one site (`DELETE /api/login/{site}`).
pub async fn clear_site_logins(
    store: &dyn CookieStore,
    site: &str,
) -> std::result::Result<String, String> {
    let site = normalize_cookie_host(site);
    if !store
        .clear(&site)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err(format!("{site} のログイン情報は保存されていません"));
    }
    Ok(format!("{site} のログイン情報を削除しました"))
}

/// Drop one login of a site (`DELETE /api/login/{site}/{index}`).
pub async fn clear_login(
    store: &dyn CookieStore,
    site: &str,
    index: usize,
) -> std::result::Result<String, String> {
    let site = normalize_cookie_host(site);
    let expected = store
        .load_groups(&site)
        .await
        .map_err(|error| error.to_string())?;
    let mut groups = expected.clone();
    if index >= groups.len() {
        return Err(missing_login_message(&site, index));
    }
    let removed = groups.remove(index);
    if !store
        .compare_save_groups(&site, &expected, &groups)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err(CONCURRENT_EDIT_MESSAGE.to_string());
    }
    Ok(format!("{site} の「{}」を削除しました", removed.display_name()))
}

/// Drop every stored site (`DELETE /api/login`).
pub async fn clear_all_logins(store: &dyn CookieStore) -> std::result::Result<String, String> {
    let count = store
        .list_groups()
        .await
        .map(|sites| sites.len())
        .unwrap_or(0);
    let empty = BTreeMap::new();
    store
        .save_all_groups(&empty)
        .await
        .map_err(|error| error.to_string())?;
    Ok(format!("ログイン情報をすべて削除しました ({count} サイト)"))
}

/// `{site} の {index} 番目のログイン情報はありません` (position shown 1-based).
fn missing_login_message(site: &str, index: usize) -> String {
    format!("{site} の {} 番目のログイン情報はありません", index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::HostCookie;

    fn login(cookie: &str) -> LoginGroup {
        LoginGroup::new(
            "example.com",
            vec![HostCookie {
                host: "example.com".to_string(),
                cookie: cookie.to_string(),
            }],
        )
    }

    #[test]
    fn apply_login_order_moves_each_position_once() {
        let stored = vec![login("a=1"), login("b=2")];
        let reordered = apply_login_order(&stored, &[1, 0]).unwrap();
        assert_eq!(reordered[0].cookies[0].cookie, "b=2");
        assert_eq!(reordered[1].cookies[0].cookie, "a=1");
        assert_eq!(
            apply_login_order(&stored, &[0]).unwrap_err(),
            ReorderLoginsError::CountMismatch
        );
        assert_eq!(
            apply_login_order(&stored, &[1, 1]).unwrap_err(),
            ReorderLoginsError::Duplicate
        );
        assert_eq!(
            apply_login_order(&stored, &[0, 2]).unwrap_err(),
            ReorderLoginsError::OutOfRange
        );
    }

    #[test]
    fn labels_are_trimmed_and_empty_clears() {
        let mut group = login("a=1");
        assert_eq!(apply_login_label(&mut group, " メイン "), "メイン");
        assert_eq!(group.label.as_deref(), Some("メイン"));
        assert_eq!(apply_login_label(&mut group, "  "), "example.com");
        assert!(group.label.is_none());
    }

    #[test]
    fn plan_import_names_and_counts() {
        let envelope = concat!(
            "version: 3\n",
            "exported_at: 2026-01-01T00:00:00+09:00\n",
            "encrypted: false\n",
            "sites:\n",
            "  example.com:\n",
            "  - site: example.com\n",
            "    cookies:\n",
            "    - host: example.com\n",
            "      cookie: sid=1\n",
        );
        let plan = plan_import(envelope, None, Some("本垢")).unwrap();
        assert_eq!(plan.logins, 1);
        assert_eq!(plan.sites_count, 1);
        assert_eq!(plan.sites["example.com"][0].display_name(), "本垢");
        assert_eq!(
            plan.success_message(1),
            "1 件を取り込みました (1 サイト / 保存済み 1 サイト)"
        );
    }

    /// Deterministically interleave a real second store mutation after an
    /// action captures its snapshot, without relying on scheduler timing.
    struct InterleavingStore {
        inner: crate::platform::mocks::MemoryCookieStore,
        replacement: parking_lot::Mutex<Option<Vec<LoginGroup>>>,
    }

    impl CookieStore for InterleavingStore {
        fn load_groups<'a>(
            &'a self, site: &'a str,
        ) -> crate::platform::PlatformFuture<'a, Result<Vec<LoginGroup>>> {
            Box::pin(async move {
                let expected = self.inner.load_groups(site).await?;
                let replacement = self.replacement.lock().take();
                if let Some(replacement) = replacement {
                    self.inner.save_groups(site, &replacement).await?;
                }
                Ok(expected)
            })
        }

        fn save_groups<'a>(
            &'a self, site: &'a str, groups: &'a [LoginGroup],
        ) -> crate::platform::PlatformFuture<'a, Result<()>> {
            self.inner.save_groups(site, groups)
        }

        fn compare_save_groups<'a>(
            &'a self, site: &'a str, expected: &'a [LoginGroup], replacement: &'a [LoginGroup],
        ) -> crate::platform::PlatformFuture<'a, Result<bool>> {
            self.inner.compare_save_groups(site, expected, replacement)
        }

        fn clear<'a>(
            &'a self, site: &'a str,
        ) -> crate::platform::PlatformFuture<'a, Result<bool>> {
            self.inner.clear(site)
        }

        fn list_groups(
            &self,
        ) -> crate::platform::PlatformFuture<'_, Result<BTreeMap<String, Vec<LoginGroup>>>> {
            self.inner.list_groups()
        }

        fn save_all_groups<'a>(
            &'a self, groups: &'a BTreeMap<String, Vec<LoginGroup>>,
        ) -> crate::platform::PlatformFuture<'a, Result<()>> {
            self.inner.save_all_groups(groups)
        }

        fn merge_all_groups<'a>(
            &'a self, groups: &'a BTreeMap<String, Vec<LoginGroup>>,
        ) -> crate::platform::PlatformFuture<'a, Result<usize>> {
            self.inner.merge_all_groups(groups)
        }

        fn renew_response_cookies<'a>(
            &'a self, sent_cookie: &'a str, set_cookie: &'a [String],
        ) -> crate::platform::PlatformFuture<'a, Result<bool>> {
            self.inner.renew_response_cookies(sent_cookie, set_cookie)
        }
    }

    #[test]
    fn positional_web_actions_reject_same_length_replacement_accounts() {
        futures::executor::block_on(async {
            for action in 0..3 {
                let inner = crate::platform::mocks::MemoryCookieStore::new();
                inner.save_groups("example.com", &[
                    login("sid=main").with_id("main"),
                    login("sid=other").with_id("other"),
                ]).await.unwrap();
                let replacement = vec![
                    login("sid=replacement").with_id("replacement"),
                    login("sid=other").with_id("other"),
                ];
                let store = InterleavingStore {
                    inner,
                    replacement: parking_lot::Mutex::new(Some(replacement.clone())),
                };
                let error = match action {
                    0 => rename_login(&store, "example.com", 0, "stale name").await,
                    1 => reorder_logins(&store, "example.com", &[1, 0]).await,
                    _ => clear_login(&store, "example.com", 0).await,
                }.unwrap_err();
                assert_eq!(error, CONCURRENT_EDIT_MESSAGE);
                assert_eq!(store.inner.load_groups("example.com").await.unwrap(), replacement);
            }
        });
    }

    #[test]
    fn web_import_merge_replace_and_clear_keep_explicit_semantics() {
        futures::executor::block_on(async {
            let store = crate::platform::mocks::MemoryCookieStore::new();
            store.save_groups("other.example", &[
                LoginGroup::new("other.example", vec![HostCookie {
                    host: "other.example".to_string(), cookie: "sid=other".to_string(),
                }]).with_id("other"),
            ]).await.unwrap();
            let plan = ImportPlan {
                sites: BTreeMap::from([(
                    "example.com".to_string(), vec![login("sid=imported").with_id("imported")],
                )]),
                logins: 1,
                sites_count: 1,
            };
            assert_eq!(import_logins(&store, &plan, false).await.unwrap(), plan.success_message(2));
            let merged = store.list_groups().await.unwrap();
            assert_eq!(merged.len(), 2);
            assert_eq!(merged["example.com"][0].id, "imported");
            assert_eq!(import_logins(&store, &plan, true).await.unwrap(), plan.success_message(1));
            let replaced = store.list_groups().await.unwrap();
            assert_eq!(replaced.len(), 1);
            assert!(!replaced.contains_key("other.example"));
            clear_all_logins(&store).await.unwrap();
            assert!(store.list_groups().await.unwrap().is_empty());
        });
    }
}
