//! ログイン資格情報の D1 実装（`narou_rs::platform::CookieStore`）。
//!
//! 保存形式は native と同じで、`app_state(scope='inv', key='login_cookie')` の
//! 1 行に「サイト → 値」の YAML マップを入れる。値は `enc:v1:...`（復号鍵は
//! secret `NAROU_RS_LOGIN_KEY`、native の環境変数と同じ名前）か、旧ビルドが
//! 書いた平文。1 サイトの値は名前つきログイン（`LoginGroup`）の並びで、1 つの
//! ログインが複数ホストの Cookie を持つ。native が書いた行をそのまま読める
//! ことが要件なので、形式を変えない。
//!
//! ホスト単位で書かれた旧形式（版 1 / 版 2）は読み込み時に実効サイト定義
//! （bundle + ユーザー定義を version gate でマージしたもの。Worker runtime の
//! fetch policy と同じ一覧）でサイト名へ畳み、版 2 の「ホストごとの並び」は
//! 同じ位置のエントリを 1 ログインにまとめてから書き戻す。暗号化も鍵生成も
//! wasm 上で動く（`getrandom` の `wasm_js` バックエンド）ので、取り込みや
//! `Set-Cookie` の書き戻しも D1 だけで完結する。

use std::collections::{BTreeMap, BTreeSet};

use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::error::{NarouError, Result};
use narou_rs::login::{KEY_LEN, LOGIN_KEY_ENV_VAR};
use narou_rs::platform::cookie_store::{
    CONCURRENT_EDIT_MESSAGE, DecodedCookieMap, INVENTORY_NAME, compare_save_site_groups,
    decode_cookie_map, encode_cookie_map, merge_site_groups, remove_site_groups,
    renew_sent_login, replace_site_groups, save_site_groups, sent_login_cookie_target,
};
use narou_rs::platform::{CookieStore, LoginGroup, ObjectStore, PlatformFuture};
use worker::wasm_bindgen::JsValue;

use crate::db_handle::DbHandle;

/// `InventoryScope::Local` の永続化スコープ（`src/db/inventory.rs`）。
const INVENTORY_SCOPE: &str = "inv";

fn db_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("D1 {INVENTORY_NAME}: {error}"))
}

/// `app_state` の 1 行（保存形式の読み出しに使う列だけ）。
#[derive(serde::Deserialize)]
struct StateRow {
    #[serde(default)]
    value_yaml: Option<String>,
    #[serde(default)]
    value_json: Option<String>,
}

fn cookie_payload<'a>(yaml: Option<&'a str>, json: Option<&'a str>) -> &'a str {
    match yaml {
        Some(yaml) if !yaml.trim().is_empty() && yaml.trim() != "{}" => yaml,
        _ => json.unwrap_or("{}"),
    }
}

fn concurrent_edit_error() -> NarouError {
    NarouError::Login(CONCURRENT_EDIT_MESSAGE.to_string())
}

fn decode_snapshot(
    row: &Option<StateRow>,
    key: Option<&[u8; KEY_LEN]>,
    settings: &[SiteSetting],
) -> Result<DecodedCookieMap> {
    let payload = row.as_ref().map_or("{}", |row| {
        cookie_payload(row.value_yaml.as_deref(), row.value_json.as_deref())
    });
    let raw = if payload.trim().is_empty() || payload.trim() == "{}" {
        BTreeMap::new()
    } else {
        serde_yaml::from_str(payload)
            .map_err(|_| db_error("malformed stored credential payload"))?
    };
    decode_cookie_map(&raw, key, settings)
}

fn site_snapshots_match(
    sites: &BTreeMap<String, Vec<LoginGroup>>,
    expected: &BTreeMap<String, Vec<LoginGroup>>,
) -> bool {
    expected.iter().all(|(site, groups)| {
        sites.get(site).map(Vec::as_slice).unwrap_or_default() == groups.as_slice()
    })
}

/// D1 の `app_state` 1 行を介した資格情報ストア。
#[derive(Clone)]
pub struct D1CookieStore {
    db: DbHandle,
    key: Option<[u8; KEY_LEN]>,
    /// 古いホストキーを畳むための実効サイト定義を読む場所。`key_site_settings`
    /// が Worker runtime と同じ一覧 (`bundled_sites::load_site_settings`) を
    /// 手で持つためだけに注入する。
    ///
    /// wasm では `PlatformService` が `Send + Sync` を要求しないため
    /// `dyn ObjectStore` はそのままでは `CookieStore: Send + Sync` を満たせない。
    /// ここは単一スレッドの wasm でしか動かないので、リポジトリの他箇所
    /// (`budget.rs`) と同じ `SendWrapper` で包む。
    objects: send_wrapper::SendWrapper<std::sync::Arc<dyn ObjectStore>>,
}

/// `objects` は `Debug` を実装しないので、鍵の中身も出さない手書きにする。
impl std::fmt::Debug for D1CookieStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("D1CookieStore")
            .field("db", &self.db)
            .field("key_source", &self.key_source())
            .finish()
    }
}

impl D1CookieStore {
    pub fn new(
        db: DbHandle,
        key: Option<[u8; KEY_LEN]>,
        objects: std::sync::Arc<dyn ObjectStore>,
    ) -> Self {
        Self {
            db,
            key,
            objects: send_wrapper::SendWrapper::new(objects),
        }
    }

    /// secret（または Secrets Store）から復号鍵を読む。未設定なら平文の行だけを扱う。
    pub async fn key_from_env(env: &worker::Env) -> Option<[u8; KEY_LEN]> {
        let value = crate::secrets::value(env, LOGIN_KEY_ENV_VAR).await?;
        match narou_rs::login::parse_key_base64(&value) {
            Ok(key) => Some(key),
            Err(error) => {
                worker::console_log!("ignoring malformed {LOGIN_KEY_ENV_VAR}: {error}");
                None
            }
        }
    }

    /// Both raw columns from PRIMARY are the compare-and-swap token; a
    /// replica or a decoded/cached map is never a mutation's write source.
    async fn load_snapshot(&self) -> Result<Option<StateRow>> {
        self.db
            .primary_view()
            .prepare("SELECT value_yaml, value_json FROM app_state WHERE scope = ? AND key = ?")
            .bind(&[
                JsValue::from_str(INVENTORY_SCOPE),
                JsValue::from_str(INVENTORY_NAME),
            ])
            .map_err(db_error)?
            .first(None)
            .await
            .map_err(db_error)
    }

    /// Guard presence and BOTH exact raw columns, including NULL. Every
    /// successful write also clears the old JSON placeholder/fallback.
    async fn compare_save_payload(
        &self,
        expected: &Option<StateRow>,
        payload: &str,
    ) -> Result<bool> {
        let db = self.db.primary_view();
        let statement = if let Some(row) = expected {
            db.prepare(
                "UPDATE app_state SET value_yaml = ?, value_json = '{}'
                 WHERE scope = ? AND key = ?
                   AND value_yaml IS ? AND value_json IS ?",
            )
            .bind(&[
                JsValue::from_str(payload),
                JsValue::from_str(INVENTORY_SCOPE),
                JsValue::from_str(INVENTORY_NAME),
                row.value_yaml.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL),
                row.value_json.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL),
            ])
        } else {
            db.prepare(
                "INSERT INTO app_state (scope, key, value_yaml, value_json)
                 VALUES (?, ?, ?, '{}') ON CONFLICT(scope, key) DO NOTHING",
            )
            .bind(&[
                JsValue::from_str(INVENTORY_SCOPE),
                JsValue::from_str(INVENTORY_NAME),
                JsValue::from_str(payload),
            ])
        }
        .map_err(db_error)?;
        let result = statement.run().await.map_err(db_error)?;
        if !result.success() {
            return Err(db_error(result.error().unwrap_or_else(|| "D1 write failed".to_string())));
        }
        Ok(result
            .meta()
            .map_err(db_error)?
            .and_then(|meta| meta.changes)
            .unwrap_or(0) == 1)
    }

    /// 復号鍵の出所 (表示用)。
    pub fn key_source(&self) -> &'static str {
        if self.key.is_some() {
            LOGIN_KEY_ENV_VAR
        } else {
            "none"
        }
    }

    /// 畳み先を決める実効サイト定義 (bundle + ユーザー定義、version gate 済み)。
    ///
    /// Worker runtime の fetch policy と同じ `bundled_sites::load_site_settings`
    /// (L1 キャッシュ共有) を使うので、ユーザー定義がサイト集合を変えていても
    /// UI 経路とジョブ経路の両方で同じ畳み先になる。同梱定義だけを使うと
    /// ユーザー定義に追従できず、経路ごとに保存キーが食い違う (キーが往復
    /// 書き換えになる) ので、bundle 専用にはしない。
    ///
    /// オブジェクトストアの読み出しに失敗したときだけ同梱定義へ下がる。
    /// 差が出るのは「ユーザー定義が存在する かつ 読み出しが失敗した」場合
    /// のみで、そうならなければ fix 前 (常に同梱定義) と同じ畳みになる。
    async fn key_site_settings(&self) -> std::sync::Arc<Vec<SiteSetting>> {
        match crate::bundled_sites::load_site_settings(&self.objects).await {
            Ok(settings) => settings,
            Err(_) => crate::bundled_sites::load_bundled_site_settings().unwrap_or_default(),
        }
    }

    /// Read-time IDs and legacy folding are published only after their exact
    /// snapshot commits. A lost CAS discards generated IDs and reloads.
    async fn load_decoded(&self) -> Result<DecodedCookieMap> {
        let settings = self.key_site_settings().await;
        for _ in 0..4 {
            let row = self.load_snapshot().await?;
            let decoded = decode_snapshot(&row, self.key.as_ref(), &settings)?;
            if !decoded.needs_rewrite {
                return Ok(decoded);
            }
            let payload = self.encode_sites(&decoded.sites)?;
            if self.compare_save_payload(&row, &payload).await? {
                return Ok(decoded);
            }
        }
        Err(concurrent_edit_error())
    }

    async fn groups(&self) -> Result<BTreeMap<String, Vec<LoginGroup>>> {
        Ok(self.load_decoded().await?.sites)
    }

    /// Sites and at-rest flags always describe a committed snapshot.
    pub async fn groups_and_encryption(
        &self,
    ) -> Result<(BTreeMap<String, Vec<LoginGroup>>, BTreeSet<String>)> {
        let decoded = self.load_decoded().await?;
        Ok((decoded.sites, decoded.encrypted))
    }

    fn encode_sites(&self, sites: &BTreeMap<String, Vec<LoginGroup>>) -> Result<String> {
        let raw = if sites.values().all(|groups| groups.is_empty()) {
            BTreeMap::new()
        } else {
            let key = self.key.as_ref().ok_or_else(|| {
                NarouError::Platform(format!(
                    "{LOGIN_KEY_ENV_VAR} is required to store credentials"
                ))
            })?;
            encode_cookie_map(sites, key)?
        };
        serde_yaml::to_string(&raw)
            .map_err(|_| db_error("cannot serialize stored credential payload"))
    }

    /// Reapply a site delta at most four times, and only while every targeted
    /// site's logical snapshot is unchanged. Re-encryption by another site's
    /// update changes raw ciphertext, but is not a same-site edit.
    /// `None` means a targeted site changed and must not be overwritten.
    async fn update_sites<T, S, F>(&self, targets: S, update: F) -> Result<Option<T>>
    where
        S: FnOnce(&BTreeMap<String, Vec<LoginGroup>>) -> Vec<String>,
        F: Fn(&mut BTreeMap<String, Vec<LoginGroup>>) -> Result<(T, bool)>,
    {
        let settings = self.key_site_settings().await;
        let mut targets = Some(targets);
        let mut expected = None;
        for _ in 0..4 {
            let row = self.load_snapshot().await?;
            let mut decoded = decode_snapshot(&row, self.key.as_ref(), &settings)?;
            if let Some(expected) = &expected {
                if !site_snapshots_match(&decoded.sites, expected) {
                    return Ok(None);
                }
            } else {
                expected = Some(targets.take().expect("first credential snapshot")(&decoded.sites)
                    .into_iter()
                    .map(|site| {
                        let groups = decoded.sites.get(&site).cloned().unwrap_or_default();
                        (site, groups)
                    })
                    .collect());
            }
            let (result, changed) = update(&mut decoded.sites)?;
            if !changed && !decoded.needs_rewrite {
                return Ok(Some(result));
            }
            let payload = self.encode_sites(&decoded.sites)?;
            if self.compare_save_payload(&row, &payload).await? {
                return Ok(Some(result));
            }
        }
        Err(concurrent_edit_error())
    }

    /// An authoritative replace/clear-all never decrypts or merges old
    /// credentials. It is one guarded write of the complete replacement.
    async fn replace_groups_map(&self, groups: &BTreeMap<String, Vec<LoginGroup>>) -> Result<()> {
        let sites = replace_site_groups(groups);
        let payload = self.encode_sites(&sites)?;
        let row = self.load_snapshot().await?;
        if self.compare_save_payload(&row, &payload).await? {
            Ok(())
        } else {
            Err(concurrent_edit_error())
        }
    }

    /// 保存済みの全サイトと順序つきログイン (native `groups_by_site`)。
    pub async fn groups_by_site(&self) -> Result<BTreeMap<String, Vec<LoginGroup>>> {
        self.groups().await
    }

    /// 1 サイトのログインを試行順で返す (native `groups_for`)。
    pub async fn groups_for(&self, site: &str) -> Result<Vec<LoginGroup>> {
        let site = site.trim().to_ascii_lowercase();
        Ok(self.groups().await?.get(&site).cloned().unwrap_or_default())
    }

    /// 1 サイトのログインを置き換える。空スライスはサイトごと削除
    /// (native `save_groups_for`)。
    pub async fn save_groups_for(&self, site: &str, groups: &[LoginGroup]) -> Result<()> {
        self.update_sites(
            |_| vec![site.trim().to_ascii_lowercase()],
            |map| {
                save_site_groups(map, site, groups);
                Ok(((), true))
            },
        )
        .await?
        .ok_or_else(concurrent_edit_error)
    }

    pub async fn compare_save_groups_for(
        &self,
        site: &str,
        expected: &[LoginGroup],
        replacement: &[LoginGroup],
    ) -> Result<bool> {
        Ok(self.update_sites(
            |_| vec![site.trim().to_ascii_lowercase()],
            |map| {
                let saved = compare_save_site_groups(map, site, expected, replacement);
                Ok((saved, saved))
            },
        )
        .await?
        .unwrap_or(false))
    }

    /// 1 サイトのログインをすべて消す (native `remove`)。削除の有無を返す
    /// (無いときは書き込まない)。
    pub async fn remove(&self, site: &str) -> Result<bool> {
        self.update_sites(
            |_| vec![site.trim().to_ascii_lowercase()],
            |map| {
                let removed = remove_site_groups(map, site);
                Ok((removed, removed))
            },
        )
        .await?
        .ok_or_else(concurrent_edit_error)
    }
}

impl CookieStore for D1CookieStore {
    fn load_groups<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<Vec<LoginGroup>>> {
        Box::pin(async move { self.groups_for(site).await })
    }

    fn save_groups<'a>(
        &'a self,
        site: &'a str,
        groups: &'a [LoginGroup],
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { self.save_groups_for(site, groups).await })
    }

    fn compare_save_groups<'a>(
        &'a self,
        site: &'a str,
        expected: &'a [LoginGroup],
        replacement: &'a [LoginGroup],
    ) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move { self.compare_save_groups_for(site, expected, replacement).await })
    }

    fn clear<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move { self.remove(site).await })
    }

    fn list_groups(&self) -> PlatformFuture<'_, Result<BTreeMap<String, Vec<LoginGroup>>>> {
        Box::pin(self.groups_by_site())
    }

    fn save_all_groups<'a>(
        &'a self,
        groups: &'a BTreeMap<String, Vec<LoginGroup>>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { self.replace_groups_map(groups).await })
    }

    fn merge_all_groups<'a>(
        &'a self,
        groups: &'a BTreeMap<String, Vec<LoginGroup>>,
    ) -> PlatformFuture<'a, Result<usize>> {
        Box::pin(async move {
            self.update_sites(
                |_| groups.keys().map(|site| site.trim().to_ascii_lowercase()).collect(),
                |map| Ok((merge_site_groups(map, groups), true)),
            )
            .await?
            .ok_or_else(concurrent_edit_error)
        })
    }

    fn renew_response_cookies<'a>(
        &'a self,
        sent_cookie: &'a str,
        set_cookie: &'a [String],
    ) -> PlatformFuture<'a, Result<bool>> {
        Box::pin(async move {
            Ok(self.update_sites(
                |map| sent_login_cookie_target(map, sent_cookie)
                    .map(|(site_index, _, _)| vec![
                        map.keys().nth(site_index).expect("matched stored site").clone(),
                    ])
                    .unwrap_or_default(),
                |map| {
                    let changed = renew_sent_login(map, sent_cookie, set_cookie);
                    Ok((changed, changed))
                },
            )
            .await?
            .unwrap_or(false))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use narou_rs::platform::HostCookie;

    #[test]
    fn cleared_canonical_cookie_value_cannot_fall_back_to_stale_json() {
        let legacy = cookie_payload(
            Some("{}"),
            Some(r#"{"site":"old-cookie"}"#),
        );
        assert_eq!(legacy, r#"{"site":"old-cookie"}"#);

        let cleared = cookie_payload(Some("{}"), Some("{}"));
        assert_eq!(cleared, "{}");
    }

    fn login(site: &str, id: &str, cookie: &str) -> LoginGroup {
        LoginGroup::new(site, vec![HostCookie {
            host: site.to_string(), cookie: cookie.to_string(),
        }]).with_id(id)
    }

    fn snapshot(raw: &BTreeMap<String, String>) -> Option<StateRow> {
        Some(StateRow {
            value_yaml: Some(serde_yaml::to_string(raw).unwrap()),
            value_json: Some("{}".to_string()),
        })
    }

    #[test]
    fn retry_guard_ignores_other_site_edits_and_fresh_encryption_nonces() {
        let key = [7; KEY_LEN];
        let b = login("b.example", "b", "sid=b");
        let original = BTreeMap::from([
            ("a.example".to_string(), vec![login("a.example", "a", "sid=a")]),
            ("b.example".to_string(), vec![b.clone()]),
        ]);
        let original_raw = encode_cookie_map(&original, &key).unwrap();
        let latest = BTreeMap::from([("b.example".to_string(), vec![b.clone()])]);
        let latest_raw = encode_cookie_map(&latest, &key).unwrap();
        assert_ne!(original_raw["b.example"], latest_raw["b.example"]);
        let decoded = decode_snapshot(&snapshot(&latest_raw), Some(&key), &[]).unwrap();
        let expected = BTreeMap::from([("b.example".to_string(), vec![b.clone()])]);
        assert!(site_snapshots_match(&decoded.sites, &expected));
        let mut sites = decoded.sites;
        let replacement = b.with_label(Some("renamed".to_string()));
        assert!(compare_save_site_groups(
            &mut sites, "b.example", &expected["b.example"], &[replacement],
        ));
        assert!(!sites.contains_key("a.example"));

        save_site_groups(&mut sites, "b.example", &[
            login("b.example", "replacement", "sid=replacement"),
        ]);
        assert!(!site_snapshots_match(&sites, &expected));
        assert!(!compare_save_site_groups(
            &mut sites, "b.example", &expected["b.example"], &[],
        ));
        assert_eq!(sites["b.example"][0].id, "replacement");
    }

    #[test]
    fn snapshots_decode_native_ciphertext_and_keep_committed_ids() {
        let key = [7; KEY_LEN];
        let sites = BTreeMap::from([(
            "example.com".to_string(), vec![login("example.com", "committed", "sid=secret")],
        )]);
        let raw = encode_cookie_map(&sites, &key).unwrap();
        let row = snapshot(&raw);
        let decoded = decode_snapshot(&row, Some(&key), &[]).unwrap();
        assert_eq!(decoded.sites, sites);
        assert!(!decoded.needs_rewrite);
        assert!(decoded.encrypted.contains("example.com"));
        assert!(decode_snapshot(&row, None, &[]).is_err());

        let legacy_columns = Some(StateRow {
            value_yaml: Some("{}".to_string()),
            value_json: Some(serde_json::to_string(&raw).unwrap()),
        });
        let fallback = decode_snapshot(&legacy_columns, Some(&key), &[]).unwrap();
        assert_eq!(fallback.sites, decoded.sites);
    }

    #[test]
    fn malformed_snapshot_diagnostics_do_not_contain_plaintext_credentials() {
        let row = Some(StateRow {
            value_yaml: Some("- private-cookie-value".to_string()),
            value_json: Some("{}".to_string()),
        });
        let message = decode_snapshot(&row, None, &[]).unwrap_err().to_string();
        assert!(!message.contains("private-cookie-value"));
        assert!(message.contains("malformed stored credential payload"));
    }
}
