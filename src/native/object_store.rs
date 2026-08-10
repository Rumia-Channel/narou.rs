//! Native ObjectStore and AssetStore adapters.
//!
//! Logical keys are mapped below the existing archive root. `novels/` is a
//! namespace in the logical key only; the native layout remains
//! `小説データ/<site>/<optional-subdirectory>/<title>/...`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;

use crate::error::{NarouError, Result};
use crate::platform::{
    AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
    ObjectStore, PlatformFuture,
};

const MAX_SMALL_OBJECT_BYTES: u64 = 16 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct NativeObjectStore {
    root: PathBuf,
}

impl NativeObjectStore {
    pub fn new() -> Result<Self> {
        let root = crate::db::with_database(|db| Ok(db.archive_root().to_path_buf()))?;
        Self::from_root(root)
    }

    pub fn from_root(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve_path(&self, key: &ObjectKey) -> Result<PathBuf> {
        key.validate()?;
        let relative = key
            .as_ref()
            .strip_prefix("novels/")
            .unwrap_or_else(|| key.as_ref());
        let relative = if key.as_ref().starts_with("novels/") {
            relative.to_string()
        } else {
            format!(".objects/{relative}")
        };
        let candidate = self
            .root
            .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        crate::db::paths::ensure_within_archive_root(&candidate, &self.root)
    }

    fn resolve_read_path(&self, key: &ObjectKey) -> Result<PathBuf> {
        let exact = self.resolve_path(key)?;
        if exact.exists() {
            return Ok(exact);
        }

        // Existing narou.rb layouts sometimes changed only the subtitle part
        // of a section filename. Keep that fallback native-only; normal core
        // paths always use the canonical ObjectKey generated from the TOC.
        let Some(parent) = exact.parent() else {
            return Ok(exact);
        };
        if parent.file_name().and_then(|name| name.to_str()) != Some("本文") {
            return Ok(exact);
        }
        let Some(filename) = exact.file_name().and_then(|name| name.to_str()) else {
            return Ok(exact);
        };
        let Some(index) = filename.split_once(' ').map(|(index, _)| index) else {
            return Ok(exact);
        };
        let prefix = format!("{index} ");
        for entry in fs::read_dir(parent)?.flatten() {
            let candidate = entry.path();
            let Some(name) = candidate.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.ends_with(".yaml")
                && (name == format!("{index}.yaml") || name.starts_with(&prefix))
            {
                return crate::db::paths::ensure_within_archive_root(&candidate, &self.root);
            }
        }
        Ok(exact)
    }

    fn metadata_for_path(&self, key: &ObjectKey, path: &Path) -> Result<Option<ObjectMetadata>> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let last_modified = metadata
            .modified()
            .ok()
            .map(chrono::DateTime::<chrono::Utc>::from);
        Ok(Some(ObjectMetadata {
            key: key.clone(),
            size: metadata.len(),
            etag: None,
            content_type: content_type_for_path(path),
            last_modified,
        }))
    }

    fn write_small_blocking(&self, key: &ObjectKey, data: Vec<u8>) -> Result<()> {
        if data.len() as u64 > MAX_SMALL_OBJECT_BYTES {
            return Err(NarouError::Platform(format!(
                "small object exceeds {} bytes: {key}",
                MAX_SMALL_OBJECT_BYTES
            )));
        }
        let path = self.resolve_path(key)?;
        let parent = path
            .parent()
            .ok_or_else(|| NarouError::Platform(format!("object has no parent: {key}")))?;
        fs::create_dir_all(parent)?;
        if let Ok(content) = std::str::from_utf8(&data) {
            crate::db::inventory::atomic_write(&path, content)?;
            return Ok(());
        }
        atomic_write_bytes(&path, &data)
    }

    fn recursive_files(root: &Path, current: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            let safe_path = crate::db::paths::ensure_within_archive_root(&path, root)?;
            if safe_path.is_dir() {
                Self::recursive_files(root, &safe_path, output)?;
            } else if safe_path.is_file() {
                output.push(safe_path);
            }
        }
        Ok(())
    }
}

fn logical_key_for_native_path(root: &Path, path: &Path) -> Option<ObjectKey> {
    let relative = path
        .strip_prefix(root)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");
    if let Some(object_relative) = relative.strip_prefix(".objects/") {
        ObjectKey::try_new(object_relative.to_string()).ok()
    } else {
        ObjectKey::try_new(format!("novels/{relative}")).ok()
    }
}

fn run_blocking<T, F>(operation: F) -> PlatformFuture<'static, Result<T>>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        Box::pin(async move {
            tokio::task::spawn_blocking(operation)
                .await
                .map_err(|error| NarouError::Platform(error.to_string()))?
        })
    } else {
        Box::pin(async move { operation() })
    }
}

fn atomic_write_bytes(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| NarouError::Platform(format!("object has no parent: {}", path.display())))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(
        ".narou-object-{}-{sequence}.tmp",
        std::process::id()
    ));
    {
        let mut file = fs::File::create(&temp_path)?;
        file.write_all(data)?;
        file.sync_all()?;
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp_path, path)?;
    Ok(())
}

fn content_type_for_path(path: &Path) -> Option<String> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("yaml" | "yml") => Some("application/yaml".to_string()),
        Some("ini" | "txt") => Some("text/plain; charset=utf-8".to_string()),
        Some("html") => Some("text/html; charset=utf-8".to_string()),
        Some("jpg" | "jpeg") => Some("image/jpeg".to_string()),
        Some("png") => Some("image/png".to_string()),
        Some("gif") => Some("image/gif".to_string()),
        Some("webp") => Some("image/webp".to_string()),
        Some("epub") => Some("application/epub+zip".to_string()),
        Some("zip") => Some("application/zip".to_string()),
        _ => None,
    }
}

impl ObjectStore for NativeObjectStore {
    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || {
            let path = this.resolve_read_path(&key)?;
            this.metadata_for_path(&key, &path)
        })
    }

    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || {
            let path = this.resolve_read_path(&key)?;
            let metadata = match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            if metadata.len() > MAX_SMALL_OBJECT_BYTES {
                return Err(NarouError::Platform(format!(
                    "small object exceeds {} bytes: {key}",
                    MAX_SMALL_OBJECT_BYTES
                )));
            }
            Ok(Some(fs::read(path)?))
        })
    }

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || this.write_small_blocking(&key, data))
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || {
            let path = this.resolve_path(&key)?;
            match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        })
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        let this = self.clone();
        let request = request.clone();
        run_blocking(move || {
            let mut paths = Vec::new();
            Self::recursive_files(&this.root, &this.root, &mut paths)?;
            let mut objects = paths
                .into_iter()
                .filter_map(|path| {
                    let key = logical_key_for_native_path(&this.root, &path)?;
                    if !request.prefix.matches(&key) {
                        return None;
                    }
                    this.metadata_for_path(&key, &path).ok().flatten()
                })
                .collect::<Vec<_>>();
            objects.sort_by(|left, right| left.key.cmp(&right.key));
            let start = request
                .cursor
                .as_deref()
                .and_then(|cursor| objects.iter().position(|item| item.key.as_ref() == cursor))
                .map(|index| index + 1)
                .unwrap_or(0);
            let end = (start + request.limit.get()).min(objects.len());
            let page = objects[start.min(objects.len())..end].to_vec();
            let next_cursor = (end < objects.len())
                .then(|| page.last().map(|item| item.key.0.clone()))
                .flatten();
            Ok(ObjectListPage {
                objects: page,
                next_cursor,
            })
        })
    }
}

impl AssetStore for NativeObjectStore {
    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        ObjectStore::stat(self, key)
    }

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        let this = self.clone();
        let key = key.clone();
        Box::pin(async move {
            let path = run_blocking(move || {
                let path = this.resolve_read_path(&key)?;
                match fs::metadata(&path) {
                    Ok(metadata) if metadata.is_file() => Ok(Some(path)),
                    Ok(_) => Ok(None),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error.into()),
                }
            })
            .await?;
            let Some(path) = path else {
                return Ok(None);
            };
            let file = tokio::fs::File::open(path).await?;
            let stream: AssetStream =
                Box::pin(futures::stream::unfold(file, |mut file| async move {
                    use tokio::io::AsyncReadExt;
                    let mut chunk = vec![0_u8; 64 * 1024];
                    match file.read(&mut chunk).await {
                        Ok(0) => None,
                        Ok(size) => {
                            chunk.truncate(size);
                            Some((Ok(chunk), file))
                        }
                        Err(error) => Some((Err(NarouError::Io(error)), file)),
                    }
                }));
            Ok(Some(stream))
        })
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        mut stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let key = key.clone();
        Box::pin(async move {
            let path = run_blocking({
                let this = this.clone();
                let key = key.clone();
                move || this.resolve_path(&key)
            })
            .await?;
            let parent = path
                .parent()
                .ok_or_else(|| NarouError::Platform(format!("asset has no parent: {key}")))?;
            tokio::fs::create_dir_all(parent).await?;
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temp_path = parent.join(format!(
                ".narou-asset-{}-{sequence}.tmp",
                std::process::id()
            ));
            let mut file = tokio::fs::File::create(&temp_path).await?;
            while let Some(chunk) = stream.next().await {
                use tokio::io::AsyncWriteExt;
                file.write_all(&chunk?).await?;
            }
            file.sync_all().await?;
            if tokio::fs::try_exists(&path).await? {
                tokio::fs::remove_file(&path).await?;
            }
            tokio::fs::rename(temp_path, path).await?;
            Ok(())
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        ObjectStore::delete(self, key)
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let source = source.clone();
        let destination = destination.clone();
        run_blocking(move || {
            let source = this.resolve_read_path(&source)?;
            let destination = this.resolve_path(&destination)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source, destination)?;
            Ok(())
        })
    }

    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let source_key = source.clone();
        let destination_key = destination.clone();
        run_blocking(move || {
            let source = this.resolve_read_path(&source_key)?;
            let destination = this.resolve_path(&destination_key)?;
            if !source.exists() {
                return Ok(());
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            match fs::rename(&source, &destination) {
                Ok(()) => Ok(()),
                Err(_) => {
                    fs::copy(&source, &destination)?;
                    fs::remove_file(source)?;
                    Ok(())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ObjectStore;

    #[test]
    fn native_store_maps_logical_novel_keys_to_existing_layout() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let keys = crate::platform::NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        let key = keys.toc();
        futures::executor::block_on(store.write_small(&key, b"title: test\n".to_vec())).unwrap();
        assert!(
            root.path()
                .join("site")
                .join("12")
                .join("n1234ab")
                .join("toc.yaml")
                .exists()
        );
        assert_eq!(
            futures::executor::block_on(store.read_small(&key))
                .unwrap()
                .unwrap(),
            b"title: test\n"
        );
    }

    #[test]
    fn native_store_rejects_escape_keys() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let key = ObjectKey::new("novels/../escape");
        assert!(futures::executor::block_on(store.read_small(&key)).is_err());
    }

    #[test]
    fn native_store_preserves_all_novel_object_layouts() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let keys = crate::platform::NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        let subtitle = "第一話";
        let section = keys.section("1", subtitle);
        let raw = keys.raw_section("1", subtitle);
        let setting = keys.setting();
        let replace = keys.replace();
        let cache = keys.illustration_cache();
        let illustration = keys.illustration("abc.png").unwrap();

        futures::executor::block_on(async {
            for (key, value) in [
                (keys.toc(), b"toc".as_slice()),
                (section.clone(), b"section".as_slice()),
                (raw.clone(), b"raw".as_slice()),
                (setting.clone(), b"setting".as_slice()),
                (replace.clone(), b"replace".as_slice()),
                (cache.clone(), b"cache".as_slice()),
                (illustration.clone(), b"png".as_slice()),
            ] {
                store.write_small(&key, value.to_vec()).await.unwrap();
            }
            let prefix = crate::platform::ObjectPrefix::new(keys.prefix().to_string()).unwrap();
            let page = store
                .list_page(&crate::platform::ObjectListRequest::new(
                    prefix,
                    std::num::NonZeroUsize::new(3).unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(page.objects.len(), 3);
            assert!(page.next_cursor.is_some());
        });

        let novel_dir = root.path().join("site").join("12").join("n1234ab");
        assert!(novel_dir.join("toc.yaml").is_file());
        assert!(novel_dir.join("本文").join("1 第一話.yaml").is_file());
        assert!(novel_dir.join("raw").join("1 第一話.html").is_file());
        assert!(novel_dir.join("setting.ini").is_file());
        assert!(novel_dir.join("replace.txt").is_file());
        assert!(novel_dir.join(".illustration_cache.yaml").is_file());
        assert!(novel_dir.join("挿絵").join("abc.png").is_file());
    }

    #[test]
    fn native_store_lists_non_novel_logical_objects() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let key = ObjectKey::new("generated/epub/book.epub");
        futures::executor::block_on(store.write_small(&key, b"epub".to_vec())).unwrap();
        let prefix = crate::platform::ObjectPrefix::new("generated").unwrap();
        let page =
            futures::executor::block_on(store.list_page(&crate::platform::ObjectListRequest::new(
                prefix,
                std::num::NonZeroUsize::new(10).unwrap(),
            )))
            .unwrap();
        assert_eq!(page.objects[0].key, key);
        assert_eq!(
            std::fs::read(
                root.path()
                    .join(".objects")
                    .join("generated")
                    .join("epub")
                    .join("book.epub")
            )
            .unwrap(),
            b"epub"
        );
    }

    #[test]
    fn native_store_loads_existing_layout_without_migration() {
        let root = tempfile::tempdir().unwrap();
        let novel_dir = root.path().join("site").join("12").join("n1234ab");
        std::fs::create_dir_all(novel_dir.join("本文")).unwrap();
        std::fs::write(novel_dir.join("本文").join("1 旧題.yaml"), b"legacy").unwrap();
        let store = NativeObjectStore::from_root(root.path().to_path_buf()).unwrap();
        let keys = crate::platform::NovelObjectKeys::new("site", "n1234ab", true).unwrap();
        let subtitle = crate::downloader::SubtitleInfo {
            index: "1".to_string(),
            href: String::new(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: "新題".to_string(),
            file_subtitle: "新題".to_string(),
            subdate: String::new(),
            subupdate: None,
            download_time: None,
        };
        let loaded = futures::executor::block_on(
            store.read_small(&keys.section(&subtitle.index, &subtitle.file_subtitle)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(loaded, b"legacy");
    }
}
