//! Missing SQLite mirrors are read through a leased private conversion snapshot.
//! Conversion must preserve the stored body without recreating canonical files.
//! This isolated test binary owns its process-wide SQLite mode and cwd.

use std::sync::Arc;

use narou_rs::converter::{ConverterCapabilities, NovelConverter, settings::NovelSettings};
use narou_rs::db::{NovelRecord, init_database};
use narou_rs::downloader::{SectionElement, SectionFile, SubtitleInfo, TocFile};
use narou_rs::platform::mocks::{FakeRateLimiter, MockHttpClient};
use narou_rs::platform::{NovelObjectKeys, NovelRepository};

fn subtitle() -> SubtitleInfo {
    SubtitleInfo {
        index: "1".to_string(),
        href: "/1/".to_string(),
        chapter: String::new(),
        subchapter: String::new(),
        subtitle: "第一話".to_string(),
        file_subtitle: "第一話".to_string(),
        subdate: "2026-01-01 00:00:00".to_string(),
        subupdate: None,
        download_time: None,
        download_revision: 0,
    }
}

fn section() -> SectionFile {
    SectionFile {
        index: "1".to_string(),
        href: "/1/".to_string(),
        chapter: String::new(),
        subchapter: String::new(),
        subtitle: "第一話".to_string(),
        file_subtitle: "第一話".to_string(),
        subdate: "2026-01-01 00:00:00".to_string(),
        subupdate: None,
        download_time: Some("2026-01-01 00:00:00 +0900".to_string()),
        element: SectionElement {
            data_type: "text".to_string(),
            introduction: String::new(),
            postscript: String::new(),
            body: "本文".to_string(),
        },
        download_revision: 0,
    }
}

#[test]
fn conversion_rebuilds_section_files_deleted_outside_narou() {
    let libraries = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-libraries");
    std::fs::create_dir_all(&libraries).unwrap();
    let temp = tempfile::Builder::new().prefix("section-mirror-").tempdir_in(libraries).unwrap();
    struct RestoreLibrary {
        cwd: std::path::PathBuf,
        database: Option<narou_rs::db::Database>,
    }
    impl Drop for RestoreLibrary {
        fn drop(&mut self) {
            *narou_rs::db::DATABASE.lock() = self.database.take();
            let _ = std::env::set_current_dir(&self.cwd);
        }
    }
    let _restore = RestoreLibrary {
        cwd: std::env::current_dir().unwrap(),
        database: narou_rs::db::DATABASE.lock().take(),
    };
    std::env::set_current_dir(temp.path()).unwrap();
    std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
    std::fs::write(
        temp.path().join(".narou").join("storage-backend"),
        "sqlite",
    )
    .unwrap();
    init_database().unwrap();

    let novel_dir = temp.path().join("小説データ").join("site").join("n1234ab");
    let toc = TocFile {
        title: "title".to_string(),
        author: "author".to_string(),
        toc_url: "https://example.com/".to_string(),
        story: None,
        subtitles: vec![subtitle()],
        novel_type: Some(2),
    };

    let mut record: NovelRecord = serde_yaml::from_str(
        "id: 1\ntitle: title\nauthor: author\nfile_title: n1234ab\nsitename: site\n\
         toc_url: https://example.com/\nnovel_type: 2\nlast_update: 2026-01-01T00:00:00Z\n",
    )
    .unwrap();

    let keys = NovelObjectKeys::new("site", "n1234ab", false).unwrap();
    let write = narou_rs::native::materialization::block_on(async {
        let repository = narou_rs::native::novel_repository::NativeNovelRepository::new();
        let reservation = repository.allocate_id().await?;
        assert_eq!(reservation.id().0, 1, "mirror fixture requires an empty library");
        record.id = reservation.id().0;
        let stores = narou_rs::native::object_store::NativeStores::for_current_root()?;
        let mut write = narou_rs::application::novel_access::NovelWriteGuard::capture_for(&stores.objects, Some(&stores.assets), &keys).await?;
        write.retain_reservation(&reservation)?;
        repository.reserve_create(write.fence(), &reservation, &record.toc_url).await?;
        write.write_small(&keys.toc(), serde_yaml::to_string(&toc)?.into_bytes()).await?;
        write.write_small(&keys.section("1", "第一話"), serde_yaml::to_string(&section())?.into_bytes()).await?;
        repository.apply_create_guarded(write.fence(), &reservation, record).await?;
        reservation.adopt();
        Ok::<_, narou_rs::error::NarouError>(write)
    }).unwrap();
    let section_path = novel_dir.join("本文").join("1 第一話.yaml");
    for device in [None, Some(narou_rs::converter::device::Device::Text)] {
        if section_path.exists() { std::fs::remove_file(&section_path).unwrap(); }
        assert!(!section_path.exists());

        let settings = NovelSettings::load_for_novel(1, "title", "author", &novel_dir).unwrap();
        let capabilities = ConverterCapabilities::new(
            Arc::new(MockHttpClient::new()),
            Arc::new(FakeRateLimiter::new()),
        );
        let mut converter = NovelConverter::with_capabilities(settings, capabilities);
        let output = match device {
            Some(device) => converter
                .convert_novel_by_id_with_device(1, &novel_dir, device, false, false)
                .map(|path| path.display().to_string()),
            None => converter.convert_novel_by_id(1, &novel_dir),
        }
        .expect("conversion must use the stored section through a private snapshot");
        let rendered = std::fs::read_to_string(output).unwrap();
        assert!(rendered.contains("本文"), "stored section body is missing: {rendered}");
        assert!(!section_path.exists(), "conversion must not create a canonical helper mirror");
    }

    let image_key = keys.illustration("fixture.png").unwrap();
    let image_bytes = b"stored illustration bytes";
    narou_rs::native::materialization::block_on(write.write_small(&image_key, image_bytes.to_vec())).unwrap();
    let image_path = novel_dir.join("挿絵").join("fixture.png");
    std::fs::remove_file(&image_path).unwrap();
    let snapshot = narou_rs::native::materialization::snapshot_with_origin(&novel_dir, &write).unwrap();
    assert_eq!(std::fs::read(snapshot.path().join("挿絵/fixture.png")).unwrap(), image_bytes);
    assert!(!image_path.exists(), "snapshot images must stay private");
    drop(snapshot);
    assert!(!image_path.exists(), "dropping a snapshot must not touch canonical files");
}
