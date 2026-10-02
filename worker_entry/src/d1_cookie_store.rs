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

use std::collections::BTreeMap;

use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    CookieStore, DecodedGroups, LoginGroup, ObjectStore, PlatformFuture, assign_group_ids,
    decode_groups, encode_groups, fold_per_host_lists, site_for_host_with, tidy_groups,
};
use worker::wasm_bindgen::JsValue;

use crate::db_handle::DbHandle;

/// native と同じ inventory 名（`.narou/login_cookie.yaml` 相当）。
pub const INVENTORY_NAME: &str = "login_cookie";
/// `InventoryScope::Local` の永続化スコープ（`src/db/inventory.rs`）。
const INVENTORY_SCOPE: &str = "inv";
/// 復号鍵の secret 名。native の環境変数と同名にする。
pub const LOGIN_KEY_SECRET: &str = "NAROU_RS_LOGIN_KEY";
/// 復号鍵のバイト長（`narou_rs::login::KEY_LEN`）。
const KEY_LEN: usize = 32;

fn db_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("D1 {INVENTORY_NAME}: {error}"))
}

/// `app_state` の 1 行（保存形式の読み出しに使う列だけ）。
#[derive(Debug, serde::Deserialize)]
struct StateRow {
    #[serde(default)]
    value_yaml: Option<String>,
    #[serde(default)]
    value_json: Option<String>,
}

fn cookie_payload(yaml: Option<String>, json: Option<String>) -> String {
    match yaml {
        Some(yaml) if !yaml.trim().is_empty() && yaml.trim() != "{}" => yaml,
        _ => json.unwrap_or_else(|| "{}".to_string()),
    }
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
        let value = crate::secrets::value(env, LOGIN_KEY_SECRET).await?;
        match narou_rs::login::parse_key_base64(&value) {
            Ok(key) => Some(key),
            Err(error) => {
                worker::console_log!("ignoring malformed {LOGIN_KEY_SECRET}: {error}");
                None
            }
        }
    }

    /// 保存されている payload（`value_yaml`、無ければ `value_json`）を返す。
    async fn load_payload(&self) -> Result<String> {
        let row: Option<StateRow> = self
            .db
            .prepare("SELECT value_yaml, value_json FROM app_state WHERE scope = ? AND key = ?")
            .bind(&[
                JsValue::from_str(INVENTORY_SCOPE),
                JsValue::from_str(INVENTORY_NAME),
            ])
            .map_err(db_error)?
            .first(None)
            .await
            .map_err(db_error)?;
        let Some(row) = row else {
            return Ok("{}".to_string());
        };
        // `value_yaml` を正とし、移行前の行 (`value_json` のみ) も読めるようにする。
        Ok(cookie_payload(row.value_yaml, row.value_json))
    }

    /// 保存されている生の YAML マップ（値は暗号化されたまま）。
    async fn load_raw(&self) -> Result<BTreeMap<String, String>> {
        let payload = self.load_payload().await?;
        if payload.trim() == "{}" || payload.trim().is_empty() {
            return Ok(BTreeMap::new());
        }
        serde_yaml::from_str(&payload).map_err(|error| {
            NarouError::Platform(format!("malformed {INVENTORY_NAME} payload: {error}"))
        })
    }

    async fn save_raw(&self, map: &BTreeMap<String, String>) -> Result<()> {
        let payload = serde_yaml::to_string(map).map_err(|error| {
            NarouError::Platform(format!("cannot serialize {INVENTORY_NAME}: {error}"))
        })?;
        let result = self
            .db
            .prepare(
                "INSERT INTO app_state (scope, key, value_yaml, value_json) VALUES (?, ?, ?, '{}')
                 ON CONFLICT(scope, key) DO UPDATE SET
                   value_yaml = excluded.value_yaml, value_json = excluded.value_json",
            )
            .bind(&[
                JsValue::from_str(INVENTORY_SCOPE),
                JsValue::from_str(INVENTORY_NAME),
                JsValue::from_str(&payload),
            ])
            .map_err(db_error)?
            .run()
            .await
            .map_err(db_error)?;
        if !result.success() {
            return Err(db_error(result.error().unwrap_or_else(|| "D1 write failed".to_string())));
        }
        Ok(())
    }

    /// そのサイトの保存値が暗号化されているか (診断用)。
    pub async fn is_encrypted(&self, site: &str) -> Result<bool> {
        let raw = self.load_raw().await?;
        Ok(raw
            .get(&site.trim().to_ascii_lowercase())
            .is_some_and(|value| narou_rs::login::is_encrypted_at_rest(value)))
    }

    /// 復号鍵の出所 (表示用)。
    pub fn key_source(&self) -> &'static str {
        if self.key.is_some() {
            LOGIN_KEY_SECRET
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
    async fn key_site_settings(&self) -> Vec<SiteSetting> {
        match crate::bundled_sites::load_site_settings(&self.objects).await {
            Ok(settings) => settings,
            Err(_) => crate::bundled_sites::load_bundled_site_settings().unwrap_or_default(),
        }
    }

    /// 全サイトのログインを復号して返す。旧形式はサイト名へ畳んで書き戻す
    /// (native `InventoryCookieStore::groups`)。
    async fn groups(&self) -> Result<BTreeMap<String, Vec<LoginGroup>>> {
        let raw = self.load_raw().await?;
        if raw.is_empty() {
            return Ok(BTreeMap::new());
        }
        // 保存キーが属するサイト。今日のキーはサイト名で、旧ビルドが書いた
        // ホスト名のキーは実効サイト定義で畳み先を決める (native `site_for_key`)。
        let site_settings = self.key_site_settings().await;
        let mut stored: BTreeMap<String, Vec<LoginGroup>> = BTreeMap::new();
        let mut legacy_per_site: BTreeMap<String, Vec<Vec<LoginGroup>>> = BTreeMap::new();
        let mut needs_rekey = false;
        for (key_name, value) in raw {
            // 暗号文の AAD は書き込み時のキー名 (サイトまたは旧ホスト名)。
            let plain = match narou_rs::login::decrypt_stored_value(
                self.key.as_ref(),
                &key_name,
                &value,
            )? {
                Some(cookie) => cookie,
                None => value,
            };
            let site = site_for_host_with(&site_settings, &key_name);
            if site != key_name {
                needs_rekey = true;
            }
            let Ok(decoded) = decode_groups(&plain, &site) else {
                continue;
            };
            match decoded {
                // 版 2 はホストごとに「同じアカウントの並び」を持っていた。
                DecodedGroups::PerHost(groups) => {
                    legacy_per_site.entry(site).or_default().push(groups);
                }
                DecodedGroups::Current(groups) => {
                    if !groups.is_empty() {
                        stored.entry(site).or_default().extend(groups);
                    }
                }
            }
        }
        for (site, lists) in legacy_per_site {
            let folded = fold_per_host_lists(lists, &site);
            stored.entry(site).or_default().extend(folded);
        }
        // 同じサイトのログインが複数のキーから来たら 1 つに畳む。
        let mut tidied: BTreeMap<String, Vec<LoginGroup>> = BTreeMap::new();
        for (site, groups) in stored {
            let before = groups.len();
            let groups = LoginGroup::merge_by_site(groups, &site);
            if groups.len() != before {
                // 断片が 1 ログインにまとまった分は書き戻す。
                needs_rekey = true;
            }
            tidied.insert(site, groups);
        }
        let mut stored = tidied;
        if assign_group_ids(&mut stored) {
            needs_rekey = true;
        }
        if needs_rekey {
            self.save_groups_map(&stored).await?;
        }
        Ok(stored)
    }

    /// サイトごとに暗号化して 1 行に書き戻す (native `save_groups_map`)。
    async fn save_groups_map(&self, map: &BTreeMap<String, Vec<LoginGroup>>) -> Result<()> {
        if map.values().all(|groups| groups.is_empty()) {
            return self.save_raw(&BTreeMap::new()).await;
        }
        // 保存値は常に暗号化する (native と同じ形式)。鍵が無ければ平文で
        // 書かずに失敗させる (fail-closed)。
        let Some(key) = self.key else {
            return Err(NarouError::Platform(format!(
                "{LOGIN_KEY_SECRET} is required to store credentials"
            )));
        };
        let mut encrypted = BTreeMap::new();
        for (site, groups) in map {
            if groups.is_empty() {
                continue;
            }
            let encoded = encode_groups(groups)?;
            encrypted.insert(
                site.clone(),
                narou_rs::login::encrypt_at_rest(&key, site, &encoded)?,
            );
        }
        self.save_raw(&encrypted).await
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
        let mut map = self.groups().await?;
        let site = site.trim().to_ascii_lowercase();
        let groups = tidy_groups(groups, &site);
        if groups.is_empty() {
            map.remove(&site);
        } else {
            map.insert(site, groups);
        }
        self.save_groups_map(&map).await
    }

    /// ログインを取り込み、既にあるサイトには後ろに足す
    /// (native `merge_groups`)。同じ Cookie のログインは二重に足さない。
    /// 戻り値は書き込まれたサイト数。
    pub async fn merge_groups(
        &self,
        entries: &BTreeMap<String, Vec<LoginGroup>>,
    ) -> Result<usize> {
        let mut map = self.groups().await?;
        for (site, groups) in entries {
            let site = site.trim().to_ascii_lowercase();
            let stored = map.entry(site.clone()).or_default();
            for group in tidy_groups(groups, &site) {
                if !stored.iter().any(|existing| existing.same_cookies(&group)) {
                    stored.push(group);
                }
            }
        }
        map.retain(|_, groups| !groups.is_empty());
        let written = map.len();
        self.save_groups_map(&map).await?;
        Ok(written)
    }

    /// ログインを取り込み、取り込みに含まれないサイトはすべて消す
    /// (native `replace_groups`)。戻り値は書き込まれたサイト数。
    pub async fn replace_groups(
        &self,
        entries: &BTreeMap<String, Vec<LoginGroup>>,
    ) -> Result<usize> {
        let mut map: BTreeMap<String, Vec<LoginGroup>> = BTreeMap::new();
        for (site, groups) in entries {
            let site = site.trim().to_ascii_lowercase();
            let groups = tidy_groups(groups, &site);
            if !groups.is_empty() {
                map.insert(site, groups);
            }
        }
        let written = map.len();
        self.save_groups_map(&map).await?;
        Ok(written)
    }

    /// 1 サイトのログインをすべて消す (native `remove`)。削除の有無を返す
    /// (無いときは書き込まない)。
    pub async fn remove(&self, site: &str) -> Result<bool> {
        let mut map = self.groups().await?;
        let removed = map.remove(&site.trim().to_ascii_lowercase()).is_some();
        if removed {
            self.save_groups_map(&map).await?;
        }
        Ok(removed)
    }

    /// すべてのサイトのログインを消す (native `clear_all`)。
    /// 復号不要 — 行ごと空のマップで上書きするので、壊れた値や鍵なしの
    /// 状態でも確実に消える。
    pub async fn clear_all(&self) -> Result<()> {
        self.save_raw(&BTreeMap::new()).await
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

    fn clear<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            self.remove(site).await?;
            Ok(())
        })
    }

    fn list_groups(&self) -> PlatformFuture<'_, Result<BTreeMap<String, Vec<LoginGroup>>>> {
        Box::pin(self.groups_by_site())
    }
}

#[cfg(test)]
mod tests {
    use super::cookie_payload;

    #[test]
    fn cleared_canonical_cookie_value_cannot_fall_back_to_stale_json() {
        let legacy = cookie_payload(
            Some("{}".to_string()),
            Some(r#"{"site":"old-cookie"}"#.to_string()),
        );
        assert_eq!(legacy, r#"{"site":"old-cookie"}"#);

        let cleared = cookie_payload(Some("{}".to_string()), Some("{}".to_string()));
        assert_eq!(cleared, "{}");
    }
}
