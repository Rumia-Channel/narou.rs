//! 保存先の振り分け。
//!
//! 「挿絵（うごイラ含む）のバイナリだけ S3 互換ストレージへ、それ以外は
//! 構造化ストア（D1 / SQLite / FS）へ」という決定を 1 箇所に閉じ込める。
//! 判定はキーの形だけを見るので、どの実装を挿しても同じ規則で動く。
//!
//! 挿絵かどうかの判定は **キーの末尾 2 セグメントが `挿絵/<file>`** かどうかで
//! 行う。小説ディレクトリ名が偶然 `挿絵` の場合（`novels/<site>/挿絵/本文/1.yaml`）
//! を挿絵と誤判定しないためで、`本文/` などが続くキーは必ず構造化ストア側に残る。

use std::sync::Arc;

use super::{
    AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
    ObjectPrefix, ObjectStore, PlatformFuture,
};
use crate::error::{NarouError, Result};

/// 挿絵のバイナリを置くディレクトリ名（narou.rb 互換のレイアウト）。
pub const ILLUSTRATION_SEGMENT: &str = "挿絵";

/// 挿絵として保存される拡張子（うごイラの APNG を含む）。
///
/// 小説ディレクトリ名が `挿絵` の場合（`novels/<site>/挿絵/toc.yaml`）を
/// 挿絵と誤判定しないため、末尾セグメントの拡張子まで見る。
const ILLUSTRATION_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "apng", "avif", "tif", "tiff",
];

/// 挿絵（うごイラの APNG を含む）のキーか。
pub fn is_illustration_key(key: &ObjectKey) -> bool {
    let mut segments = key.as_ref().rsplit('/');
    let Some(file) = segments.next() else {
        return false;
    };
    if segments.next() != Some(ILLUSTRATION_SEGMENT) {
        return false;
    }
    let extension = file
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    extension.is_some_and(|extension| ILLUSTRATION_EXTENSIONS.contains(&extension.as_str()))
}

/// 挿絵ディレクトリを指す一覧プレフィックスか。
///
/// 一覧は `<小説プレフィックス>/挿絵` の形だけを挿絵側へ回す。小説ディレクトリ
/// 名が `挿絵` の場合（`novels/<site>/挿絵` を一覧する）は構造化ストア側に
/// 残るよう、セグメント数でも区別する。
fn is_illustration_prefix(prefix: &ObjectPrefix) -> bool {
    let segments: Vec<&str> = prefix.as_ref().trim_end_matches('/').split('/').collect();
    segments.last() == Some(&ILLUSTRATION_SEGMENT) && segments.len() >= 4
}

#[derive(Clone)]
struct StorePair {
    objects: Arc<dyn ObjectStore>,
    assets: Arc<dyn AssetStore>,
}

/// 挿絵だけ別のストアへ流すルーター。
///
/// 挿絵の読み書きは `AssetStore`（ストリーム）、制御オブジェクトは
/// `ObjectStore`（bounded read/write）という使い分けはそのまま保たれる。
#[derive(Clone)]
pub struct SplitStore {
    primary: StorePair,
    illustrations: StorePair,
}

impl SplitStore {
    pub fn new(
        primary: (Arc<dyn ObjectStore>, Arc<dyn AssetStore>),
        illustrations: (Arc<dyn ObjectStore>, Arc<dyn AssetStore>),
    ) -> Self {
        Self {
            primary: StorePair {
                objects: primary.0,
                assets: primary.1,
            },
            illustrations: StorePair {
                objects: illustrations.0,
                assets: illustrations.1,
            },
        }
    }

    /// 振り分け無し（両方を同じ実装にする）。
    pub fn single(objects: Arc<dyn ObjectStore>, assets: Arc<dyn AssetStore>) -> Self {
        let pair = (objects, assets);
        Self::new(pair.clone(), pair)
    }

    fn pair_for(&self, key: &ObjectKey) -> &StorePair {
        if is_illustration_key(key) {
            &self.illustrations
        } else {
            &self.primary
        }
    }

    fn pair_for_prefix(&self, prefix: &ObjectPrefix) -> &StorePair {
        if is_illustration_prefix(prefix) {
            &self.illustrations
        } else {
            &self.primary
        }
    }
}

impl ObjectStore for SplitStore {
    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        self.pair_for(key).objects.stat(key)
    }

    fn exists<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<bool>> {
        self.pair_for(key).objects.exists(key)
    }

    fn read_small<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        self.pair_for(key).objects.read_small(key)
    }

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>> {
        self.pair_for(key).objects.write_small(key, data)
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        self.pair_for(key).objects.delete(key)
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        self.pair_for_prefix(&request.prefix)
            .objects
            .list_page(request)
    }
}

impl AssetStore for SplitStore {
    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        self.pair_for(key).assets.stat(key)
    }

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        self.pair_for(key).assets.read_stream(key)
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        self.pair_for(key).assets.write_stream(key, stream)
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        self.pair_for(key).assets.delete(key)
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        // 片方だけ挿絵という交差コピーは経路が決まらないので拒否する。
        if is_illustration_key(source) != is_illustration_key(destination) {
            return Box::pin(async move {
                Err(NarouError::Platform(format!(
                    "cross-store copy is not supported: {} -> {}",
                    source.as_ref(),
                    destination.as_ref()
                )))
            });
        }
        self.pair_for(source).assets.copy(source, destination)
    }

    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        if is_illustration_key(source) != is_illustration_key(destination) {
            return Box::pin(async move {
                Err(NarouError::Platform(format!(
                    "cross-store move is not supported: {} -> {}",
                    source.as_ref(),
                    destination.as_ref()
                )))
            });
        }
        self.pair_for(source)
            .assets
            .move_or_copy(source, destination)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::mocks::MemoryObjectStore;

    fn key(value: &str) -> ObjectKey {
        ObjectKey::try_new(value).unwrap()
    }

    fn prefix(value: &str) -> ObjectPrefix {
        ObjectPrefix::new(value).unwrap()
    }

    #[test]
    fn illustration_keys_are_the_ones_under_the_illustration_directory() {
        assert!(is_illustration_key(&key("novels/site/title/挿絵/0001.jpg")));
        assert!(is_illustration_key(&key(
            "novels/site/title/挿絵/0001.apng"
        )));
        // 小説ディレクトリ名が `挿絵` でも、本文や目次は挿絵ではない。
        assert!(!is_illustration_key(&key("novels/site/挿絵/本文/1 x.yaml")));
        assert!(!is_illustration_key(&key("novels/site/挿絵/toc.yaml")));
        // 挿絵の索引 (`.illustration_cache.yaml`) はメタデータなので残る。
        assert!(!is_illustration_key(&key(
            "novels/site/title/illustration_cache.yaml"
        )));
    }

    #[test]
    fn listing_prefix_routing_matches_the_storage_rule() {
        assert!(is_illustration_prefix(&prefix("novels/site/title/挿絵")));
        assert!(is_illustration_prefix(&prefix(
            "novels/site/sub/title/挿絵"
        )));
        // 小説ディレクトリ名が `挿絵` の一覧 (novels/<site>/挿絵) は構造化側。
        assert!(!is_illustration_prefix(&prefix("novels/site/挿絵")));
        assert!(!is_illustration_prefix(&prefix("novels/site/title")));
    }

    /// 書き込みが振り分けられ、読み出しも同じ側から返ること。
    #[test]
    fn writes_and_reads_follow_the_route() {
        futures::executor::block_on(async {
            let primary = Arc::new(MemoryObjectStore::new());
            let illustrations = Arc::new(MemoryObjectStore::new());
            let store = SplitStore::new(
                (primary.clone(), primary.clone()),
                (illustrations.clone(), illustrations.clone()),
            );

            let section = key("novels/site/title/本文/1 x.yaml");
            let picture = key("novels/site/title/挿絵/0001.jpg");
            store
                .write_small(&section, b"section".to_vec())
                .await
                .unwrap();
            store
                .write_small(&picture, b"picture".to_vec())
                .await
                .unwrap();

            assert!(primary.read_small(&section).await.unwrap().is_some());
            assert!(primary.read_small(&picture).await.unwrap().is_none());
            assert!(
                illustrations
                    .read_small(&picture)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert!(
                illustrations
                    .read_small(&section)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                store.read_small(&picture).await.unwrap().unwrap(),
                b"picture".to_vec()
            );

            // 一覧も挿絵だけ別ストアから返る。
            let page = store
                .list_page(&ObjectListRequest::new(
                    prefix("novels/site/title/挿絵"),
                    10.try_into().unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(page.objects.len(), 1);
            assert_eq!(page.objects[0].key, picture);

            let page = store
                .list_page(&ObjectListRequest::new(
                    prefix("novels/site/title"),
                    10.try_into().unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(page.objects.len(), 1);
            assert_eq!(page.objects[0].key, section);

            // ストアをまたぐコピーは経路が決まらないので明示的に失敗させる。
            assert!(store.copy(&section, &picture).await.is_err());
        });
    }
}
