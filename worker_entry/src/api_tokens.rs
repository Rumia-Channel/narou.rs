//! Worker の外部 API トークンストア。
//!
//! native と同じ `app_state(scope='inv', key='api_tokens')` に保存する。
//! 同時更新でトークンを失わないよう、スナップショット CAS で書き込む。

use narou_rs::application::api_tokens::{ApiToken, ApiTokenChange, ApiTokenStore};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::PlatformFuture;
use serde::Deserialize;
use worker::wasm_bindgen::JsValue;

use crate::db_handle::DbHandle;

const SELECT_SQL: &str = "SELECT value_json FROM app_state WHERE scope='inv' AND key='api_tokens'";
const SAVE_SQL: &str = r#"
INSERT INTO app_state(scope,key,value_json,value_yaml)
SELECT 'inv','api_tokens',?1,?2
WHERE ?3 IS NULL OR EXISTS(SELECT 1 FROM app_state WHERE scope='inv' AND key='api_tokens' AND value_json=?3)
ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml
WHERE ?3 IS NOT NULL AND app_state.value_json=?3
RETURNING key
"#;

#[derive(Deserialize)]
struct Row {
    value_json: String,
}

#[derive(Deserialize)]
struct KeyRow {
    key: String,
}

fn db_error(value: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(value.to_string())
}

/// D1 の `app_state` を使うトークンストア。
pub struct D1ApiTokenStore {
    db: DbHandle,
}

impl D1ApiTokenStore {
    pub fn new(db: DbHandle) -> Self {
        Self { db }
    }

    async fn snapshot(&self) -> Result<Option<String>> {
        Ok(self
            .db
            .prepare(SELECT_SQL)
            .first::<Row>(None)
            .await
            .map_err(db_error)?
            .map(|row| row.value_json))
    }

    async fn tokens(&self) -> Result<Vec<ApiToken>> {
        let snapshot = self.snapshot().await?;
        let Some(snapshot) = snapshot else {
            return Ok(Vec::new());
        };
        if snapshot.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&snapshot).map_err(db_error)
    }

    async fn change(&self, change: ApiTokenChange) -> Result<bool> {
        for _ in 0..8 {
            let snapshot = self.snapshot().await?;
            let mut tokens: Vec<ApiToken> = match snapshot.as_deref() {
                Some(raw) if !raw.trim().is_empty() => serde_json::from_str(raw).map_err(db_error)?,
                _ => Vec::new(),
            };
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
            if !changed {
                return Ok(false);
            }
            let encoded = serde_json::to_string(&tokens).map_err(db_error)?;
            let yaml = serde_yaml::to_string(&tokens)?;
            let expected = snapshot.map(JsValue::from).unwrap_or(JsValue::NULL);
            let rows = self
                .db
                .prepare(SAVE_SQL)
                .bind(&[encoded.into(), yaml.into(), expected])
                .map_err(db_error)?
                .all()
                .await
                .map_err(db_error)?
                .results::<KeyRow>()
                .map_err(db_error)?;
            if rows.iter().any(|row| row.key == "api_tokens") {
                return Ok(true);
            }
        }
        Err(db_error(
            "外部 API のトークンが同時に更新されています。再試行してください",
        ))
    }
}

impl ApiTokenStore for D1ApiTokenStore {
    fn load(&self) -> PlatformFuture<'_, Result<Vec<ApiToken>>> {
        Box::pin(async move { self.tokens().await })
    }

    fn apply(&self, change: ApiTokenChange) -> PlatformFuture<'_, Result<bool>> {
        Box::pin(self.change(change))
    }
}
