//! ライブラリの保存方式に応じたサイト定義ストア。
//!
//! - **YAML モード** (既定): ライブラリの `webnovel/` フォルダ。narou.rb と同じ場所を
//!   そのまま使うので、Ruby 版と相互に差し替えられる。
//! - **SQLite モード** (`storage-backend = sqlite`): オブジェクトストア (SQLite) 上の
//!   `webnovel/<name>.yaml`。切り替え時に既存の `webnovel/` から一度だけ取り込むので、
//!   SQLite 構成は YAML ファイルに依存しない。
//!
//! どちらの場合も Web UI / CLI は `SiteDefinitions` (core) 越しに同じ操作をする。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::application::site_definitions::{
    ObjectStoreSiteDefinitions, SiteDefinitionStore, SiteDefinitions,
};
use crate::db::inventory::Inventory;
use crate::error::Result;
use crate::platform::PlatformFuture;

/// `webnovel/` フォルダを置き場にする実装（YAML モード）。
pub struct FsSiteDefinitions {
    dir: PathBuf,
}

impl FsSiteDefinitions {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl SiteDefinitionStore for FsSiteDefinitions {
    fn list(&self) -> PlatformFuture<'_, Result<Vec<String>>> {
        Box::pin(async move {
            let mut names = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&self.dir) {
                for entry in entries.flatten() {
                    let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if name.ends_with(".yaml") {
                        names.push(name);
                    }
                }
            }
            names.sort();
            Ok(names)
        })
    }

    fn get<'a>(&'a self, name: &'a str) -> PlatformFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            let path = self.path(name);
            if !path.is_file() {
                return Ok(None);
            }
            Ok(Some(std::fs::read_to_string(path)?))
        })
    }

    fn put<'a>(&'a self, name: &'a str, yaml: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            std::fs::create_dir_all(&self.dir)?;
            crate::db::inventory::atomic_write(&self.path(name), yaml)?;
            Ok(())
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let path = self.path(name);
            if path.is_file() {
                std::fs::remove_file(path)?;
            }
            Ok(())
        })
    }
}

/// 取り込み済みマーカー (`app_state('inv','webnovel_imported')`)。
const IMPORT_MARKER: &str = "webnovel_imported";

/// 現在のライブラリに合ったストアを返す。
///
/// SQLite モードでは、初回だけ `webnovel/` の内容をオブジェクトストアへ取り込む
/// (元ファイルは残す: narou.rb 側の運用を壊さないため)。以後はファイルを読まない。
pub async fn store_for_current_library() -> Result<Arc<dyn SiteDefinitionStore>> {
    let root = Inventory::with_default_root()?.root_dir().to_path_buf();
    let narou_dir = root.join(".narou");
    match crate::native::sqlite::state::active_for(&narou_dir) {
        Some(state) => {
            let objects: Arc<dyn crate::platform::ObjectStore> =
                Arc::new(crate::native::object_store::NativeStore::for_narou_root(&root)?);
            let store: Arc<dyn SiteDefinitionStore> =
                Arc::new(ObjectStoreSiteDefinitions::new(objects));
            import_once(&state, &store, &root.join("webnovel")).await?;
            Ok(store)
        }
        None => Ok(Arc::new(FsSiteDefinitions::new(root.join("webnovel")))),
    }
}

/// 配布物の `webnovel/*.yaml` を bundle として読む（実行ファイル隣 + dev の manifest）。
///
/// ユーザーが `webnovel/` を差し替えていない設定でも一覧に出るようにするための
/// 土台で、実効定義はここにユーザー定義を重ねて決める。
fn shipped_bundle() -> Result<Vec<(String, String)>> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        dirs.push(parent.join("webnovel"));
    }
    #[cfg(debug_assertions)]
    dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("webnovel"));

    let mut bundle = Vec::new();
    for dir in dirs {
        let store = FsSiteDefinitions::new(dir);
        for name in futures::executor::block_on(store.list())? {
            if bundle.iter().any(|(existing, _): &(String, String)| existing == &name) {
                continue;
            }
            if let Some(yaml) = futures::executor::block_on(store.get(&name))? {
                bundle.push((name, yaml));
            }
        }
    }
    Ok(bundle)
}

/// 管理（Web UI / CLI）が使うサービス。保存方式に応じたストアを選ぶ。
pub async fn site_definition_service() -> Result<SiteDefinitions> {
    Ok(SiteDefinitions::new(
        shipped_bundle()?,
        store_for_current_library().await?,
    ))
}

/// 起動時に有効なサイト定義を確定させる。
///
/// YAML モードでは `webnovel/` を、SQLite モードではオブジェクトストアを読み、
/// どちらも bundle + ユーザー定義のマージ結果をプロセス内に固定する。以後の同期
/// コードは [`crate::downloader::site_setting::effective_site_settings`] を使う。
pub async fn install_effective_site_settings() -> Result<()> {
    let store = store_for_current_library().await?;
    let service = SiteDefinitions::new(shipped_bundle()?, store);
    let effective = service.effective().await?;
    let contents: Vec<&str> = effective.iter().map(|(_, yaml)| yaml.as_str()).collect();
    let settings = crate::downloader::site_setting::SiteSetting::load_bundled(&contents)?;
    crate::downloader::site_setting::install_effective_site_settings(settings);
    Ok(())
}

/// YAML モードからの一回きりの取り込み。
///
/// オブジェクトストア側が空のときだけ `webnovel/` の内容を写す。元ファイルは
/// 残すので narou.rb 側の運用は壊れない。取り込み済みは `app_state` のマーカーで
/// 覚えるので、以後はファイルを読まない (SQLite 構成が YAML に依存しない)。
async fn import_once(
    state: &crate::native::sqlite::state::StateDb,
    store: &Arc<dyn SiteDefinitionStore>,
    folder: &Path,
) -> Result<()> {
    if state.get_raw("inv", IMPORT_MARKER)?.is_some() {
        return Ok(());
    }
    if store.list().await?.is_empty() {
        let source = FsSiteDefinitions::new(folder.to_path_buf());
        for name in source.list().await? {
            if let Some(yaml) = source.get(&name).await? {
                store.put(&name, &yaml).await?;
            }
        }
    }
    state.set_raw("inv", IMPORT_MARKER, "true")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::site_definitions::{SiteDefinitions, SiteDefinitionOrigin};

    #[test]
    fn fs_store_round_trips_definitions() {
        let dir = std::env::temp_dir().join(format!("narou_rs_sites_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = FsSiteDefinitions::new(dir.clone());

        let yaml = "name: Example\ndomain: example.com\ntop_url: https://example.com\nsitename: Example\ntoc_url: https://example.com/\\k<url>\n";
        futures::executor::block_on(async {
            store.list().await.map(|names| assert!(names.is_empty())).unwrap();
            store.put("example.com.yaml", yaml).await.unwrap();
            let names = store.list().await.unwrap();
            assert_eq!(names, vec!["example.com.yaml".to_string()]);
            assert_eq!(store.get("example.com.yaml").await.unwrap().unwrap(), yaml);
            store.delete("example.com.yaml").await.unwrap();
            assert!(store.get("example.com.yaml").await.unwrap().is_none());
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    /// YAML モードのストアでも、core のサービス越しに bundle とマージできる。
    #[test]
    fn service_over_the_fs_store_overrides_and_restores() {
        let dir = std::env::temp_dir().join(format!("narou_rs_sites_svc_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let bundled = vec![(
            "example.com.yaml".to_string(),
            "name: Example\ndomain: example.com\ntop_url: https://example.com\nsitename: Bundled\ntoc_url: https://example.com/\\k<url>\n"
                .to_string(),
        )];
        let service =
            SiteDefinitions::new(bundled, Arc::new(FsSiteDefinitions::new(dir.clone())));

        futures::executor::block_on(async {
            let user = "name: Example\ndomain: example.com\ntop_url: https://example.com\nsitename: User\ntoc_url: https://example.com/\\k<url>\n";
            service.put("example.com", user).await.unwrap();
            let listed = service.list().await.unwrap();
            assert_eq!(listed[0].origin, SiteDefinitionOrigin::User);
            let effective = service.effective().await.unwrap();
            assert_eq!(effective[0].1, user);

            service.delete("example.com.yaml").await.unwrap();
            let listed = service.list().await.unwrap();
            assert_eq!(listed[0].origin, SiteDefinitionOrigin::Bundled);
        });
        std::fs::remove_dir_all(&dir).ok();
    }
}
