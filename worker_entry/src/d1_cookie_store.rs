//! ログイン資格情報の D1 実装（`narou_rs::platform::CookieStore`）。
//!
//! 保存形式は native と同じで、`app_state(scope='inv', key='login_cookie')` の
//! 1 行に「host → 値」の YAML マップを入れる。値は `enc:v1:...`（復号鍵は
//! secret `NAROU_RS_LOGIN_KEY`、native の環境変数と同じ名前）か、旧ビルドが
//! 書いた平文。native が書いた行をそのまま読めることが要件なので、形式を
//! 変えない。
//!
//! 書き込みのうち `save_all` は wasm に乱数源が無いため未対応にしてある
//! （値の暗号化に nonce が要る）。資格情報の取り込みや Set-Cookie の書き戻しは
//! native 側で行う。Downloader が使うのは `load_all` だけなので、読めれば
//! DL・更新は動く。

use std::collections::BTreeMap;
use std::sync::Arc;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    CookieStore, LoginCredential, PlatformFuture, cookie_lookup_hosts, merge_credentials_for,
    normalize_cookie_host,
};
use worker::{D1Database, wasm_bindgen::JsValue};

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

/// D1 の `app_state` 1 行を介した資格情報ストア。
#[derive(Debug, Clone)]
pub struct D1CookieStore {
    db: Arc<D1Database>,
    key: Option<[u8; KEY_LEN]>,
}

impl D1CookieStore {
    pub fn new(db: Arc<D1Database>, key: Option<[u8; KEY_LEN]>) -> Self {
        Self { db, key }
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
        let Some(StateRow {
            value_yaml: yaml,
            value_json: json,
        }) = row
        else {
            return Ok("{}".to_string());
        };
        // `value_yaml` を正とし、移行前の行 (`value_json` のみ) も読めるようにする。
        Ok(match yaml {
            Some(yaml) if !yaml.trim().is_empty() && yaml.trim() != "{}" => yaml,
            _ => json.unwrap_or_else(|| "{}".to_string()),
        })
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
        // 行が無いときは作る（`app_state` の主キーは (scope, key)）。
        self.db
            .prepare(
                "INSERT INTO app_state (scope, key, value_yaml, value_json) VALUES (?, ?, ?, '{}')
                 ON CONFLICT(scope, key) DO UPDATE SET value_yaml = excluded.value_yaml",
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
        Ok(())
    }

    /// そのホストの保存値が暗号化されているか (診断用)。
    pub async fn is_encrypted(&self, host: &str) -> Result<bool> {
        let raw = self.load_raw().await?;
        Ok(raw
            .get(&normalize_cookie_host(host))
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

    /// 復号して「host → 資格情報」を返す。
    async fn load_decoded(&self) -> Result<BTreeMap<String, Vec<LoginCredential>>> {
        let payload = self.load_payload().await?;
        narou_rs::platform::decode_stored_credentials(&payload, self.key.as_ref())
    }
}

impl CookieStore for D1CookieStore {
    fn load_all<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<Vec<LoginCredential>>> {
        Box::pin(async move {
            let stored = self.load_decoded().await?;
            Ok(merge_credentials_for(&stored, host))
        })
    }

    fn save_all<'a>(
        &'a self,
        host: &'a str,
        credentials: &'a [LoginCredential],
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            // 保存値は常に暗号化する (native と同じ形式)。鍵が無ければ
            // 平文で書かずに失敗させる。
            let Some(key) = self.key else {
                return Err(NarouError::Platform(format!(
                    "{LOGIN_KEY_SECRET} is required to store credentials"
                )));
            };
            let host = normalize_cookie_host(host);
            let mut raw = self.load_raw().await?;
            let cleaned: Vec<LoginCredential> =
                narou_rs::platform::tidy_credentials(credentials)
                    .into_iter()
                    .map(|credential| LoginCredential {
                        host: host.clone(),
                        ..credential
                    })
                    .collect();
            if cleaned.is_empty() {
                raw.remove(&host);
            } else {
                let encoded = narou_rs::platform::encode_credentials(&cleaned)?;
                raw.insert(
                    host.clone(),
                    narou_rs::login::encrypt_at_rest(&key, &host, &encoded)?,
                );
            }
            self.save_raw(&raw).await
        })
    }

    fn clear<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut raw = self.load_raw().await?;
            let mut changed = false;
            for key in cookie_lookup_hosts(host) {
                changed |= raw.remove(&key).is_some();
            }
            if changed {
                // 値は保存されたまま（復号もしない）なので、暗号文はそのまま残る。
                self.save_raw(&raw).await?;
            }
            Ok(())
        })
    }

    fn list(&self) -> PlatformFuture<'_, Result<BTreeMap<String, Vec<LoginCredential>>>> {
        Box::pin(self.load_decoded())
    }
}
