use std::collections::HashSet;

use narou_rs::db;
use narou_rs::application::novel_access::NovelWriteGuard;
use narou_rs::downloader::{RAW_DATA_DIR, SECTION_SAVE_DIR};

use super::download;
use super::log;

pub fn cmd_clean(targets: &[String], force: bool, dry_run: bool, all: bool) -> i32 {
    match cmd_clean_inner(targets, force, dry_run, all) {
        Ok(()) => 0,
        Err(err) => {
            log::report_error(&err);
            1
        }
    }
}

fn cmd_clean_inner(
    targets: &[String],
    force: bool,
    dry_run: bool,
    all: bool,
) -> Result<(), String> {
    db::init_database().map_err(|e| e.to_string())?;

    let remove = force && !dry_run;
    if all {
        clean_all(remove)?;
        return Ok(());
    }

    if targets.is_empty() {
        let Some(target) = super::latest_convert_target() else {
            return Ok(());
        };
        if let Some(write) = resolve_novel_origin(&target)? {
            clean_novel(write, remove)?;
        } else {
            // Ruby は直前に変換した小説が解決できない場合も error を出す。
            log::report_error(&format!("{} は存在しません", target));
        }
        return Ok(());
    }

    let expanded = download::tagname_to_ids(targets);
    for target in expanded {
        let Some(write) = resolve_novel_origin(&target)? else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };
        clean_novel(write, remove)?;
    }

    Ok(())
}

fn clean_all(remove: bool) -> Result<(), String> {
    let frozen_ids = narou_rs::compat::load_frozen_ids().map_err(|e| e.to_string())?;
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let ids = novels
        .scan_ids_sync(&narou_rs::platform::NovelFilter::all(), None, usize::MAX)
        .map_err(|e| e.to_string())?;
    let mut origins = Vec::new();
    for record in novels.get_many_current_sync(&ids).map_err(|error| error.to_string())?.into_iter().flatten() {
        if narou_rs::compat::record_is_frozen(&record, &frozen_ids) { continue; }
        origins.push(narou_rs::native::materialization::capture_write_for_record(&record).map_err(|error| error.to_string())?);
    }
    for write in origins { clean_novel(write, remove)?; }
    Ok(())
}

fn resolve_novel_origin(target: &str) -> Result<Option<NovelWriteGuard>, String> {
    let Some(id) = super::resolve_target_to_id(target) else { return Ok(None); };
    narou_rs::native::materialization::capture_write_for_id(id).map(Some).map_err(|error| error.to_string())
}

fn clean_novel(write: NovelWriteGuard, remove: bool) -> Result<(), String> {
    let stores = narou_rs::native::object_store::NativeStores::for_current_root()
        .map_err(|error| error.to_string())?;
    let prefix_key = narou_rs::platform::ObjectKey::try_new(write.prefix().as_ref())
        .map_err(|error| error.to_string())?;
    let bytes = narou_rs::native::materialization::block_on(async {
        let mut lease = narou_rs::application::novel_access::NovelReadLease::admit(
            &stores.objects,
            write.prefix(),
            "clean-content-check",
        )
        .await?;
        let result = async {
            if lease.epoch() != write.epoch() {
                return Err(narou_rs::error::NarouError::Platform(
                    "novel_busy: clean origin changed".into(),
                ));
            }
            lease.read_small(&prefix_key.join("toc.yaml")?).await
        }
        .await;
        let released = lease.release().await;
        match result {
            Ok(value) => {
                released?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    })
    .map_err(|error| error.to_string())?;
    let Some(bytes) = bytes else { return Ok(()) };
    let toc: narou_rs::downloader::TocFile = serde_yaml::from_slice(&bytes)
        .map_err(|error| format!("目次の読み込みに失敗しました: {error}"))?;
    let expected = expected_section_stems(&toc);
    for orphan in find_orphan_keys_with_expected(&stores, &write, &prefix_key, &expected)
        .map_err(|error| error.to_string())?
    {
        let display_path = stores
            .primary
            .path_for_canonical_key(&orphan)
            .map_err(|error| error.to_string())?;
        narou_rs::logger::emit_stdout(&format!("{}", display_path.display()), true);
        if remove {
            narou_rs::native::materialization::block_on(write.delete(&orphan))
                .map_err(|error| error.to_string())?;
            if !narou_rs::native::sqlite::state::mirror_files_enabled() {
                stores
                    .primary
                    .with_open_write(write.fence(), || {
                        match std::fs::remove_file(&display_path) {
                            Ok(()) => Ok(()),
                            Err(error)
                                if error.kind() == std::io::ErrorKind::NotFound =>
                            {
                                Ok(())
                            }
                            Err(error) => Err(error.into()),
                        }
                    })
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}

/// 目次 (TOC) に現れる話数 (本文/raw) の期待ファイル名 stem。
///
/// narou.rb の `Command::Clean#find_orphans` と同じく `index` と `file_subtitle`
/// をそのまま連結した stem に加え、Rust の正規ライター (`section_filename` /
/// `NovelObjectKeys::section`) が生成するサニタイズ済み stem も両方許可する。
/// これにより Ruby 版の末尾空白付きファイルと更新途中の正規化済みファイルの
/// どちらも誤って孤児削除しない。
fn expected_section_stems(toc: &narou_rs::downloader::TocFile) -> HashSet<String> {
    let mut stems = HashSet::with_capacity(toc.subtitles.len());
    let sample_keys = narou_rs::platform::NovelObjectKeys::new("s", "n", false).ok();
    for subtitle in &toc.subtitles {
        stems.insert(format!("{} {}", subtitle.index, subtitle.file_subtitle));
        let canonical = narou_rs::downloader::persistence::section_filename(subtitle);
        if let Some(stem) = canonical.strip_suffix(".yaml") {
            stems.insert(stem.to_string());
        }
        if let Some(keys) = sample_keys.as_ref() {
            for key in std::iter::once(keys.section(&subtitle.index, &subtitle.file_subtitle))
                .chain(keys.legacy_section_fallbacks(&subtitle.index, &subtitle.file_subtitle))
            {
                if let Some(stem) = key.as_ref().rsplit('/').next().and_then(|name| name.strip_suffix(".yaml")) {
                    stems.insert(stem.to_string());
                }
            }
        }
    }
    stems
}

/// Split a direct-child filename the same way `Path::file_stem` /
/// `Path::extension` split it (a leading-dot-only stem has no extension).
fn split_stem_ext(file_name: &str) -> (&str, Option<&str>) {
    let trimmed = file_name.strip_prefix('.').unwrap_or(file_name);
    if trimmed.is_empty() || trimmed.chars().all(|ch| ch == '.') {
        return (file_name, None);
    }
    match trimmed.rsplit_once('.') {
        Some((stem, ext)) => (stem, Some(ext)),
        None => (file_name, None),
    }
}

fn find_orphan_keys_with_expected(
    stores: &narou_rs::native::object_store::NativeStores,
    write: &NovelWriteGuard,
    prefix_key: &narou_rs::platform::ObjectKey,
    expected: &HashSet<String>,
) -> narou_rs::error::Result<Vec<narou_rs::platform::ObjectKey>> {
    let raw_dir = prefix_key.join(RAW_DATA_DIR)?;
    let section_dir = prefix_key.join(SECTION_SAVE_DIR)?;
    stores.primary.with_open_write(write.fence(), || {
        let mut orphans = collect_orphan_keys(stores, &raw_dir, expected, &["html", "txt"])?;
        orphans.extend(collect_orphan_keys(stores, &section_dir, expected, &["yaml"])?);
        orphans.sort();
        Ok(orphans)
    })
}

/// Collect orphaned files stored directly under `dir_key` (the `raw/` and
/// `本文/` directories only — history caches and illustrations are never
/// scanned). SQLite rows and filesystem-only leftovers are both listed by
/// `list_direct_files_sync`.
fn collect_orphan_keys(
    stores: &narou_rs::native::object_store::NativeStores,
    dir_key: &narou_rs::platform::ObjectKey,
    expected: &HashSet<String>,
    exts: &[&str],
) -> narou_rs::error::Result<Vec<narou_rs::platform::ObjectKey>> {
    let mut orphans = Vec::new();
    for key in stores.primary.list_direct_files_sync(dir_key)? {
        let file_name = key.as_ref().rsplit('/').next().unwrap_or_default();
        let (stem, ext) = split_stem_ext(file_name);
        let Some(ext) = ext else { continue };
        if !exts.iter().any(|candidate| ext.eq_ignore_ascii_case(candidate)) {
            continue;
        }
        if stem.is_empty() || expected.contains(stem) {
            continue;
        }
        orphans.push(key);
    }
    Ok(orphans)
}

#[cfg(test)]
mod tests {
    use narou_rs::downloader::{SubtitleInfo, TocFile};
    use narou_rs::platform::{NovelAccessFence, NovelObjectKeys, ObjectStore};

    use super::{clean_novel, expected_section_stems, split_stem_ext};

    #[test]
    fn split_stem_ext_matches_path_semantics() {
        assert_eq!(split_stem_ext("1 keep.yaml"), ("1 keep", Some("yaml")));
        assert_eq!(split_stem_ext("65 テスト .yaml"), ("65 テスト ", Some("yaml")));
        assert_eq!(split_stem_ext(".yaml"), (".yaml", None));
        assert_eq!(split_stem_ext(".."), ("..", None));
        assert_eq!(split_stem_ext("a."), ("a", Some("")));
        assert_eq!(split_stem_ext("plain"), ("plain", None));
    }

    fn test_toc(file_subtitle: &str) -> TocFile {
        TocFile {
            title: "title".into(),
            author: "author".into(),
            toc_url: "https://example.com".into(),
            story: None,
            subtitles: vec![SubtitleInfo {
                index: "1".into(),
                href: "1".into(),
                chapter: String::new(),
                subchapter: String::new(),
                subtitle: file_subtitle.into(),
                file_subtitle: file_subtitle.into(),
                subdate: "2024-01-01".into(),
                subupdate: None,
                download_time: None,
                download_revision: 0,
            }],
            novel_type: None,
        }
    }

    #[test]
    fn expected_section_stems_keeps_raw_index_and_file_subtitle() {
        let stems = expected_section_stems(&test_toc("9　お店妖精からの依頼 "));
        assert!(stems.contains("1 9　お店妖精からの依頼 "));
    }

    struct RestoreDatabase(Option<narou_rs::db::Database>);
    impl Drop for RestoreDatabase {
        fn drop(&mut self) {
            *narou_rs::db::DATABASE.lock() = self.0.take();
        }
    }

    /// A private YAML-mode (filesystem) library: `.narou` exists but carries
    /// no `storage-backend` marker, so the store is `NativeStore::Fs`.
    fn yaml_library() -> (tempfile::TempDir, RestoreDatabase, crate::test_support::CurrentDirGuard) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".narou")).unwrap();
        let cwd = crate::test_support::set_current_dir_for_test(root.path());
        let database = RestoreDatabase(narou_rs::db::DATABASE.lock().take());
        narou_rs::db::init_database().unwrap();
        (root, database, cwd)
    }

    /// A private SQLite library whose `小説データ/` mirror is disabled: the
    /// object rows are the only durable copies unless a stray file was left
    /// behind while mirroring was still enabled.
    fn sqlite_library_without_mirror() -> (tempfile::TempDir, RestoreDatabase, crate::test_support::CurrentDirGuard) {
        let root = tempfile::tempdir().unwrap();
        let narou_dir = root.path().join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        narou_rs::native::sqlite::state::write_mode(
            &narou_dir,
            narou_rs::native::sqlite::state::StorageMode::Sqlite,
        )
        .unwrap();
        let cwd = crate::test_support::set_current_dir_for_test(root.path());
        let database = RestoreDatabase(narou_rs::db::DATABASE.lock().take());
        narou_rs::db::init_database().unwrap();
        narou_rs::db::settings::update(narou_rs::setting_core::SettingScope::Local, |settings| {
            settings.insert("sqlite.mirror-files".to_string(), serde_yaml::Value::Bool(false));
            Ok(())
        })
        .unwrap();
        (root, database, cwd)
    }

    fn capture(
        stores: &narou_rs::native::object_store::NativeStores,
        keys: &NovelObjectKeys,
    ) -> narou_rs::application::novel_access::NovelWriteGuard {
        narou_rs::native::materialization::block_on(
            narou_rs::application::novel_access::NovelWriteGuard::capture_for(
                &stores.objects,
                Some(&stores.assets),
                keys,
            ),
        )
        .unwrap()
    }

    fn put(
        write: &narou_rs::application::novel_access::NovelWriteGuard,
        key: narou_rs::platform::ObjectKey,
    ) {
        narou_rs::native::materialization::block_on(
            write.write_small(&key, b"stored".to_vec()),
        )
        .unwrap();
    }

    fn exists(
        stores: &narou_rs::native::object_store::NativeStores,
        key: &narou_rs::platform::ObjectKey,
    ) -> bool {
        narou_rs::native::materialization::block_on(
            ObjectStore::stat(stores.objects.as_ref(), key),
        )
        .unwrap()
        .is_some()
    }

    /// Orphan detection must stay inside `raw/` and the direct children of
    /// `本文/`: history caches, illustrations and `setting.ini` are not
    /// scan targets even when they are stored under the same prefix.
    #[test]
    fn clean_removes_only_direct_raw_and_section_orphans() {
        let (root, _database, _cwd) = yaml_library();
        let stores = narou_rs::native::object_store::NativeStores::for_narou_root(root.path()).unwrap();
        let keys = NovelObjectKeys::new("example.test", "clean-test", false).unwrap();
        let write = capture(&stores, &keys);
        let prefix = keys.prefix();

        let toc = test_toc("keep");
        narou_rs::native::materialization::block_on(
            write.write_small(
                &keys.toc(),
                serde_yaml::to_string(&toc).unwrap().into_bytes(),
            ),
        )
        .unwrap();
        put(&write, keys.section("1", "keep"));
        put(&write, prefix.join("raw").unwrap().join("1 keep.html").unwrap());
        put(&write, keys.setting());
        put(&write, prefix.join("挿絵").unwrap().join("cover.png").unwrap());
        put(&write, prefix.join("本文").unwrap().join("cache").unwrap().join("old").unwrap().join("9 keep.yaml").unwrap());
        let orphan_section = prefix.join("本文").unwrap().join("9 orphan.yaml").unwrap();
        let orphan_raw = prefix.join("raw").unwrap().join("9 orphan.html").unwrap();
        put(&write, orphan_section.clone());
        put(&write, orphan_raw.clone());

        clean_novel(write, true).unwrap();

        assert!(!exists(&stores, &orphan_section));
        assert!(!exists(&stores, &orphan_raw));
        for kept in [
            keys.toc(),
            keys.section("1", "keep"),
            prefix.join("raw").unwrap().join("1 keep.html").unwrap(),
            keys.setting(),
            prefix.join("挿絵").unwrap().join("cover.png").unwrap(),
            prefix.join("本文").unwrap().join("cache").unwrap().join("old").unwrap().join("9 keep.yaml").unwrap(),
        ] {
            assert!(exists(&stores, &kept), "{kept}");
        }
    }

    /// `mirror-files=false` still has to remove orphans that exist only as
    /// SQLite rows (or only as stray mirror files), including Ruby-era names
    /// with a trailing space that `ObjectKey` cannot represent sanitized.
    #[test]
    fn clean_removes_sqlite_only_and_stray_mirror_orphans_without_mirror() {
        let (root, _database, _cwd) = sqlite_library_without_mirror();
        let stores = narou_rs::native::object_store::NativeStores::for_narou_root(root.path()).unwrap();
        let keys = NovelObjectKeys::new("example.test", "clean-test", false).unwrap();
        let write = capture(&stores, &keys);
        let prefix = keys.prefix();

        let toc = test_toc("keep");
        narou_rs::native::materialization::block_on(
            write.write_small(&keys.toc(), serde_yaml::to_string(&toc).unwrap().into_bytes()),
        )
        .unwrap();
        put(&write, keys.section("1", "keep"));
        put(&write, keys.setting());

        // SQLite-only orphans: rows with no mirror file at all.
        let orphan_section = prefix.join("本文").unwrap().join("9 orphan.yaml").unwrap();
        let orphan_trailing = prefix.join("本文").unwrap().join("66 テスト .yaml").unwrap();
        put(&write, orphan_section.clone());
        put(&write, orphan_trailing.clone());

        // Stray mirror leftovers: files that exist on disk even though the
        // mirror is disabled (left behind while mirroring was on).
        let novel_dir = stores
            .primary
            .path_for_canonical_key(&prefix.join("x").unwrap())
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let stray = novel_dir.join("本文").join("7 stray.yaml");
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, b"stray").unwrap();

        clean_novel(write, true).unwrap();

        assert!(!exists(&stores, &orphan_section));
        assert!(!exists(&stores, &orphan_trailing));
        assert!(!stray.exists());
        assert!(exists(&stores, &keys.section("1", "keep")));
        assert!(exists(&stores, &keys.setting()));
    }

    /// Ruby 版が作った末尾空白付きセクション名 (`本文/65 テスト .yaml`) は
    /// TOC の `index` + `file_subtitle` をそのまま連結した期待 stem に
    /// 一致するので孤児として消してはいけない。
    #[test]
    fn clean_keeps_sections_with_trailing_space_in_file_subtitle() {
        let (root, _database, _cwd) = yaml_library();
        let stores = narou_rs::native::object_store::NativeStores::for_narou_root(root.path()).unwrap();
        let keys = NovelObjectKeys::new("example.test", "clean-test", false).unwrap();
        let write = capture(&stores, &keys);
        let prefix = keys.prefix();

        let mut toc = test_toc("テスト ");
        toc.subtitles[0].index = "65".into();
        narou_rs::native::materialization::block_on(
            write.write_small(&keys.toc(), serde_yaml::to_string(&toc).unwrap().into_bytes()),
        )
        .unwrap();
        let kept_section = prefix.join("本文").unwrap().join("65 テスト .yaml").unwrap();
        let kept_raw = prefix.join("raw").unwrap().join("65 テスト .html").unwrap();
        let kept_canonical_section = keys.section("65", "テスト ");
        let kept_canonical_raw = keys.raw_section("65", "テスト ");
        let orphan = prefix.join("本文").unwrap().join("99 削除済み.yaml").unwrap();
        put(&write, kept_section.clone());
        put(&write, kept_raw.clone());
        put(&write, kept_canonical_section.clone());
        put(&write, kept_canonical_raw.clone());
        put(&write, orphan.clone());

        clean_novel(write, true).unwrap();

        assert!(exists(&stores, &kept_section));
        assert!(exists(&stores, &kept_raw));
        assert!(exists(&stores, &kept_canonical_section));
        assert!(exists(&stores, &kept_canonical_raw));
        assert!(!exists(&stores, &orphan));
    }

    /// A `NovelWriteGuard` whose epoch was superseded (gate closed and
    /// reopened) must not be able to delete anything.
    #[test]
    fn clean_refuses_stale_writer_epoch() {
        let (root, _database, _cwd) = yaml_library();
        let stores = narou_rs::native::object_store::NativeStores::for_narou_root(root.path()).unwrap();
        let keys = NovelObjectKeys::new("example.test", "clean-test", false).unwrap();
        let write = capture(&stores, &keys);
        let prefix = keys.prefix();

        let toc = test_toc("keep");
        narou_rs::native::materialization::block_on(
            write.write_small(&keys.toc(), serde_yaml::to_string(&toc).unwrap().into_bytes()),
        )
        .unwrap();
        put(&write, keys.section("1", "keep"));
        let orphan = prefix.join("本文").unwrap().join("9 orphan.yaml").unwrap();
        put(&write, orphan.clone());

        // Advance the gate epoch so the captured writer is stale.
        let gate_prefix = write.prefix().clone();
        narou_rs::native::materialization::block_on(async {
            let epoch = stores.primary.close_gate(&gate_prefix, "test-publisher").await?;
            stores.primary.open_gate(&gate_prefix, "test-publisher", epoch).await
        })
        .unwrap();

        assert!(clean_novel(write, true).is_err());
        assert!(exists(&stores, &orphan));
        assert!(exists(&stores, &keys.section("1", "keep")));
    }
}
