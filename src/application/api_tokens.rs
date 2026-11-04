//! Bearer tokens for the external (public) API.
//! 外部公開 API の Bearer トークン管理モジュール。
//!
//! 外部公開 API に入るための Bearer トークンをライブラリに持たせる。トークン
//! は名前つきで、作成・確認・削除は CLI (`narou api token`) と Web UI
//! (設定ページの「外部API」タブ) から行える。
//!
//! 保存先は native なら在庫 (`api_tokens`)、Worker なら D1 の `app_state`。
//! どちらも同じ [`ApiToken`] を使う。
//!
//! トークン本体は平文で保存する。`server-basic-auth.password` と同じ扱いで、
//! 「確認できる」ことを優先する。表示時は先頭だけを残した指紋も出す。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::platform::{PlatformFuture, PlatformService};

/// 外部 API の Bearer トークン。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiToken {
    /// 識別子。削除や表示に使う。
    pub id: String,
    /// 利用者が付ける名前。
    pub name: String,
    /// トークン本体。
    pub token: String,
    /// 作成日時。
    pub created_at: String,
}

impl ApiToken {
    /// 表示用の指紋 (先頭 8 文字)。
    pub fn fingerprint(&self) -> String {
        self.token.chars().take(8).collect()
    }
}

/// 外部 API のトークン情報 (表示用)。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiTokenInfo {
    /// 識別子。
    pub id: String,
    /// 利用者が付ける名前。
    pub name: String,
    /// トークン本体。
    pub token: String,
    /// 先頭 8 文字だけ表示した指紋。
    pub fingerprint: String,
    /// 作成日時。
    pub created_at: String,
}

impl ApiToken {
    /// 表示用の情報に直す。
    pub fn to_info(&self) -> ApiTokenInfo {
        ApiTokenInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            token: self.token.clone(),
            fingerprint: self.fingerprint(),
            created_at: self.created_at.clone(),
        }
    }
}

/// トークン一覧への変更。
///
/// 変更を「命令」として渡すので、保存側が 1 回の書き込みで読み込みと変更を
/// まとめられる (同時作成で 1 件失う事故を避ける)。
#[derive(Debug, Clone)]
pub enum ApiTokenChange {
    /// トークンを追加する。
    Create(ApiToken),
    /// ID を指定して削除する。
    Delete(String),
}

/// トークンの保存先。
pub trait ApiTokenStore: PlatformService {
    /// 保存されているトークンをすべて読む。
    fn load(&self) -> PlatformFuture<'_, Result<Vec<ApiToken>>>;
    /// 変更を適用する。対象が無く変更が不要なら `false`。
    fn apply(&self, change: ApiTokenChange) -> PlatformFuture<'_, Result<bool>>;
}

/// トークンの管理サービス。
pub struct ApiTokenService {
    store: Arc<dyn ApiTokenStore>,
}

impl ApiTokenService {
    pub fn new(store: Arc<dyn ApiTokenStore>) -> Self {
        Self { store }
    }

    /// 保存されているトークン。
    pub async fn list(&self) -> Result<Vec<ApiToken>> {
        self.store.load().await
    }

    /// 新しいトークンを作る。
    ///
    /// 名前が空なら既定名 (`token`) を使う。同じ名前は許可する (ID で管理
    /// するため)。
    pub async fn create(&self, name: Option<&str>) -> Result<ApiToken> {
        let name = name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("token")
            .to_string();
        let token = ApiToken {
            id: random_id(),
            name,
            token: generate_token()?,
            created_at: crate::db::ruby_time::now_string(),
        };
        self.store
            .apply(ApiTokenChange::Create(token.clone()))
            .await?;
        Ok(token)
    }

    /// トークンを削除する。存在しなければ `false`。
    pub async fn delete(&self, id: &str) -> Result<bool> {
        self.store.apply(ApiTokenChange::Delete(id.to_string())).await
    }

    /// 表示用のトークン情報。
    pub async fn infos(&self) -> Result<Vec<ApiTokenInfo>> {
        Ok(self.list().await?.iter().map(ApiToken::to_info).collect())
    }

    /// 提示された Bearer トークンが有効か。
    ///
    /// 比較は一定時間で行う。
    pub async fn verify(&self, presented: &str) -> Result<bool> {
        if presented.is_empty() {
            return Ok(false);
        }
        let tokens = self.store.load().await?;
        Ok(tokens
            .iter()
            .any(|token| constant_time_eq(token.token.as_bytes(), presented.as_bytes())))
    }
}

/// メモリ上のトークンストア (テストとモック用)。
#[derive(Debug, Default)]
pub struct MemoryApiTokenStore {
    tokens: parking_lot::Mutex<Vec<ApiToken>>,
}

impl MemoryApiTokenStore {
    pub fn new(tokens: Vec<ApiToken>) -> Self {
        Self {
            tokens: parking_lot::Mutex::new(tokens),
        }
    }
}

impl ApiTokenStore for MemoryApiTokenStore {
    fn load(&self) -> PlatformFuture<'_, Result<Vec<ApiToken>>> {
        let tokens = self.tokens.lock().clone();
        Box::pin(async move { Ok(tokens) })
    }

    fn apply(&self, change: ApiTokenChange) -> PlatformFuture<'_, Result<bool>> {
        let mut tokens = self.tokens.lock();
        let changed = match &change {
            ApiTokenChange::Create(token) => {
                tokens.push(token.clone());
                true
            }
            ApiTokenChange::Delete(id) => {
                let before = tokens.len();
                tokens.retain(|token| &token.id != id);
                tokens.len() != before
            }
        };
        Box::pin(async move { Ok(changed) })
    }
}

/// 一定時間比較。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// トークン文字列を作る (`nrt_` + base64url 32 バイト)。
fn generate_token() -> Result<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let bytes: [u8; 32] = crate::login::crypto::random_bytes()?;
    Ok(format!("nrt_{}", URL_SAFE_NO_PAD.encode(bytes)))
}

/// トークンの識別子 (16 バイトの base64url)。
fn random_id() -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    match crate::login::crypto::random_bytes::<16>() {
        Ok(bytes) => URL_SAFE_NO_PAD.encode(bytes),
        Err(_) => String::from("unknown"),
    }
}
