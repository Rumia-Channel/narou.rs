//! サイト定義（`webnovel/*.yaml`）の差し替え経路。
//!
//! native ではライブラリの `webnovel/` フォルダ、Worker では D1 上のオブジェクトが
//! ユーザー定義の置き場になる。どちらも「**bundle 済み定義が種（seed）+ フォール
//! バック**、ユーザー定義が同名で上書き」という同じ規則で解決するので、Web UI と
//! CLI から同じ API を叩ける。
//!
//! 名前は `webnovel/` のファイル名そのもの（`ncode.syosetu.com.yaml`）。拡張子込み
//! なので、native のフォルダをそのまま持ち込んでも対応が崩れない。

use std::sync::Arc;

use crate::downloader::site_setting::SiteSetting;
use crate::error::{NarouError, Result};
use crate::platform::{
    ObjectKey, ObjectListRequest, ObjectPrefix, ObjectStore, PlatformFuture, PlatformService,
};

/// ユーザー定義を置くキーの接頭辞（Worker / オブジェクトストア経路）。
pub const DEFINITION_PREFIX: &str = "webnovel";

/// 定義がどこから来たか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteDefinitionOrigin {
    /// bundle（配布物）にのみ存在する。
    Bundled,
    /// ユーザーが置いた定義（bundle を上書きしている場合も含む）。
    User,
}

impl SiteDefinitionOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::User => "user",
        }
    }
}

/// 一覧に出す 1 件。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SiteDefinitionMeta {
    pub name: String,
    pub origin: SiteDefinitionOrigin,
}

/// 本文込みの 1 件（編集用）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SiteDefinition {
    pub name: String,
    pub origin: SiteDefinitionOrigin,
    pub yaml: String,
}

/// ユーザー定義の置き場（native はフォルダ、Worker はオブジェクトストア）。
///
/// 読み出しは「保存されているもの」をそのまま返すだけで、bundle とのマージは
/// [`SiteDefinitions`] が行う。
pub trait SiteDefinitionStore: PlatformService {
    /// 保存されている定義の名前（`webnovel/` のファイル名）。
    fn list(&self) -> PlatformFuture<'_, Result<Vec<String>>>;

    fn get<'a>(&'a self, name: &'a str) -> PlatformFuture<'a, Result<Option<String>>>;

    fn put<'a>(&'a self, name: &'a str, yaml: &'a str) -> PlatformFuture<'a, Result<()>>;

    fn delete<'a>(&'a self, name: &'a str) -> PlatformFuture<'a, Result<()>>;
}


/// 名前を `webnovel/` のファイル名に正規化する。
///
/// `syosetu.org` のように拡張子を省いて渡されたら `.yaml` を補う。パス区切りや
/// 先頭のドットは拒否する（別ディレクトリへ抜けられないようにする）。
pub fn normalize_definition_name(name: &str) -> Result<String> {
    let name = name.trim();
    let file = if name.ends_with(".yaml") {
        name.to_string()
    } else {
        format!("{name}.yaml")
    };
    let stem = file.strip_suffix(".yaml").unwrap_or(&file);
    let valid = !stem.is_empty()
        && !stem.starts_with('.')
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !valid || file.contains('/') || file.contains('\\') {
        return Err(NarouError::Platform(format!(
            "invalid site definition name: {name:?}"
        )));
    }
    Ok(file)
}

/// オブジェクトストアを置き場にする実装（Worker の D1、native の SQLite でも使える）。
pub struct ObjectStoreSiteDefinitions {
    objects: Arc<dyn ObjectStore>,
}

impl ObjectStoreSiteDefinitions {
    pub fn new(objects: Arc<dyn ObjectStore>) -> Self {
        Self { objects }
    }

    fn key(name: &str) -> Result<ObjectKey> {
        ObjectKey::try_new(format!("{DEFINITION_PREFIX}/{name}"))
            .map_err(|error| NarouError::Platform(error.to_string()))
    }
}

impl SiteDefinitionStore for ObjectStoreSiteDefinitions {
    fn list(&self) -> PlatformFuture<'_, Result<Vec<String>>> {
        Box::pin(async move {
            let prefix = ObjectPrefix::new(format!("{DEFINITION_PREFIX}/"))
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            let mut names = Vec::new();
            let mut request =
                ObjectListRequest::new(prefix, std::num::NonZeroUsize::new(100).unwrap());
            loop {
                let page = self.objects.list_page(&request).await?;
                for object in &page.objects {
                    let Some(file) = object
                        .key
                        .as_ref()
                        .strip_prefix(&format!("{DEFINITION_PREFIX}/"))
                    else {
                        continue;
                    };
                    if file.ends_with(".yaml") && !file.contains('/') {
                        names.push(file.to_string());
                    }
                }
                match page.next_cursor {
                    Some(cursor) => request = request.after(cursor),
                    None => break,
                }
            }
            names.sort();
            Ok(names)
        })
    }

    fn get<'a>(&'a self, name: &'a str) -> PlatformFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            let key = Self::key(name)?;
            Ok(self
                .objects
                .read_small(&key)
                .await?
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
        })
    }

    fn put<'a>(&'a self, name: &'a str, yaml: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let key = Self::key(name)?;
            self.objects.write_small(&key, yaml.as_bytes().to_vec()).await
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let key = Self::key(name)?;
            self.objects.delete(&key).await
        })
    }
}

/// bundle とユーザー定義をまとめて扱うサービス。
///
/// Web UI / CLI が使う入り口を 1 つにし、`put` の検証・`list` の origin 判定・
/// 実効定義の解決を全プラットフォームで同じにする。
pub struct SiteDefinitions {
    bundled: Vec<(String, String)>,
    store: Arc<dyn SiteDefinitionStore>,
}

impl SiteDefinitions {
    /// `bundled` は `(ファイル名, YAML)` の並び（`webnovel/*.yaml` を埋め込んだもの）。
    pub fn new(bundled: Vec<(String, String)>, store: Arc<dyn SiteDefinitionStore>) -> Self {
        Self { bundled, store }
    }

    fn bundled_yaml(&self, name: &str) -> Option<&str> {
        self.bundled
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, yaml)| yaml.as_str())
    }

    /// bundle とユーザー定義を名前でまとめた一覧（ユーザー側の origin を立てる）。
    pub async fn list(&self) -> Result<Vec<SiteDefinitionMeta>> {
        let user = self.store.list().await?;
        let mut metas: Vec<SiteDefinitionMeta> = self
            .bundled
            .iter()
            .map(|(name, _)| SiteDefinitionMeta {
                name: name.clone(),
                origin: if user.iter().any(|existing| existing == name) {
                    SiteDefinitionOrigin::User
                } else {
                    SiteDefinitionOrigin::Bundled
                },
            })
            .collect();
        for name in &user {
            if self.bundled_yaml(name).is_none() {
                metas.push(SiteDefinitionMeta {
                    name: name.clone(),
                    origin: SiteDefinitionOrigin::User,
                });
            }
        }
        metas.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(metas)
    }

    /// 実効定義（ユーザー定義 → bundle の順に探す）。
    pub async fn get(&self, name: &str) -> Result<Option<SiteDefinition>> {
        let name = normalize_definition_name(name)?;
        if let Some(yaml) = self.store.get(&name).await? {
            return Ok(Some(SiteDefinition {
                name,
                origin: SiteDefinitionOrigin::User,
                yaml,
            }));
        }
        Ok(self.bundled_yaml(&name).map(|yaml| SiteDefinition {
            name,
            origin: SiteDefinitionOrigin::Bundled,
            yaml: yaml.to_string(),
        }))
    }

    /// 検証してから保存する。壊れた定義は readiness を落とすのでここで弾く。
    pub async fn put(&self, name: &str, yaml: &str) -> Result<SiteDefinition> {
        let name = normalize_definition_name(name)?;
        if yaml.trim().is_empty() {
            return Err(NarouError::Platform(
                "the site definition must not be empty".to_string(),
            ));
        }
        SiteSetting::load_bundled(&[yaml]).map_err(|error| {
            NarouError::Platform(format!("invalid site definition {name}: {error}"))
        })?;
        self.store.put(&name, yaml).await?;
        Ok(SiteDefinition {
            name,
            origin: SiteDefinitionOrigin::User,
            yaml: yaml.to_string(),
        })
    }

    /// ユーザー定義を消す（bundle があればそれに戻る）。
    pub async fn delete(&self, name: &str) -> Result<()> {
        let name = normalize_definition_name(name)?;
        self.store.delete(&name).await
    }

    /// 実効定義の並び（`<(name, yaml)>`）。bundle を土台にユーザー定義で上書きする。
    pub async fn effective(&self) -> Result<Vec<(String, String)>> {
        let user = self.store.list().await?;
        let mut merged: Vec<(String, String)> = self.bundled.clone();
        for name in user {
            let Some(yaml) = self.store.get(&name).await? else {
                continue;
            };
            match merged.iter_mut().find(|(existing, _)| existing == &name) {
                Some(entry) => entry.1 = yaml,
                None => merged.push((name, yaml)),
            }
        }
        Ok(merged)
    }
}

/// Web UI / CLI が使う一覧の JSON（native と Worker で同じ形）。
pub fn list_payload(definitions: &[SiteDefinitionMeta]) -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "data": {
            "sites": definitions
                .iter()
                .map(|definition| serde_json::json!({
                    "name": definition.name,
                    "origin": definition.origin.as_str(),
                }))
                .collect::<Vec<_>>(),
            "count": definitions.len(),
        },
    })
}

/// 1 件（本文込み）の JSON。
pub fn show_payload(definition: &SiteDefinition) -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "data": {
            "name": definition.name,
            "origin": definition.origin.as_str(),
            "yaml": definition.yaml,
        },
    })
}

/// 失敗の JSON（HTTP のステータスは呼び出し側が決める）。
pub fn error_payload(message: &str) -> serde_json::Value {
    serde_json::json!({ "success": false, "message": message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::mocks::MemoryObjectStore;

    const BUNDLED_EXAMPLE: &str = "name: Example\ndomain: example.com\ntop_url: https://example.com\nsitename: Bundled\ntoc_url: https://example.com/\\k<url>\n";
    const USER_EXAMPLE: &str = "name: Example\ndomain: example.com\ntop_url: https://example.com\nsitename: User\ntoc_url: https://example.com/\\k<url>\n";
    const USER_NEW: &str = "name: New\ndomain: new.example\ntop_url: https://new.example\nsitename: New\ntoc_url: https://new.example/\\k<url>\n";

    fn service() -> (SiteDefinitions, Arc<MemoryObjectStore>) {
        let store = Arc::new(MemoryObjectStore::new());
        let bundled = vec![("example.com.yaml".to_string(), BUNDLED_EXAMPLE.to_string())];
        (
            SiteDefinitions::new(bundled, Arc::new(ObjectStoreSiteDefinitions::new(store.clone()))),
            store,
        )
    }

    #[test]
    fn list_reports_origin_and_get_returns_the_effective_yaml() {
        futures::executor::block_on(async {
            let (service, _store) = service();
            let listed = service.list().await.unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].origin, SiteDefinitionOrigin::Bundled);

            let bundled = service.get("example.com").await.unwrap().unwrap();
            assert_eq!(bundled.origin, SiteDefinitionOrigin::Bundled);
            assert_eq!(bundled.yaml, BUNDLED_EXAMPLE);

            // 上書きすると origin が user になり、本文も差し替わる。
            service.put("example.com", USER_EXAMPLE).await.unwrap();
            let listed = service.list().await.unwrap();
            assert_eq!(listed[0].origin, SiteDefinitionOrigin::User);
            let overridden = service.get("example.com.yaml").await.unwrap().unwrap();
            assert_eq!(overridden.origin, SiteDefinitionOrigin::User);
            assert_eq!(overridden.yaml, USER_EXAMPLE);

            // 消せば bundle に戻る。
            service.delete("example.com").await.unwrap();
            let restored = service.get("example.com").await.unwrap().unwrap();
            assert_eq!(restored.origin, SiteDefinitionOrigin::Bundled);
        });
    }

    #[test]
    fn new_definitions_are_added_and_invalid_ones_are_rejected() {
        futures::executor::block_on(async {
            let (service, store) = service();
            service.put("new.example", USER_NEW).await.unwrap();
            let listed = service.list().await.unwrap();
            assert_eq!(listed.len(), 2);
            assert_eq!(listed[1].name, "new.example.yaml");
            assert_eq!(listed[1].origin, SiteDefinitionOrigin::User);

            // 壊れた YAML は保存しない (readiness を落とさない)。
            assert!(service.put("broken", "name: Broken\n").await.is_err());
            assert!(
                store
                    .read_small(&ObjectKey::try_new("webnovel/broken.yaml").unwrap())
                    .await
                    .unwrap()
                    .is_none()
            );
            // パスを抜けようとする名前も拒否する。
            assert!(service.put("../../etc/passwd", USER_NEW).await.is_err());
        });
    }

    #[test]
    fn effective_merges_the_bundle_with_user_definitions() {
        futures::executor::block_on(async {
            let (service, _store) = service();
            service.put("example.com.yaml", USER_EXAMPLE).await.unwrap();
            service.put("new.example.yaml", USER_NEW).await.unwrap();

            let effective = service.effective().await.unwrap();
            assert_eq!(effective.len(), 2);
            let example = effective
                .iter()
                .find(|(name, _)| name == "example.com.yaml")
                .unwrap();
            assert_eq!(example.1, USER_EXAMPLE);
            assert!(effective.iter().any(|(name, _)| name == "new.example.yaml"));

            let settings = SiteSetting::load_bundled(
                &effective
                    .iter()
                    .map(|(_, yaml)| yaml.as_str())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            assert_eq!(settings.len(), 2);
            assert_eq!(
                settings
                    .iter()
                    .find(|setting| setting.domain == "example.com")
                    .unwrap()
                    .sitename,
                "User"
            );
        });
    }
}
