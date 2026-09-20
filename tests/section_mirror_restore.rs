//! Regression: a section file removed outside narou is rebuilt from SQLite
//! storage before the conversion reports it as missing.
//!
//! SQLite mode keeps every section in the `objects` table with a filesystem
//! mirror for the converter. Deleting the mirror (manual cleanup, sync tool)
//! used to fail the conversion with "section file not found"; the read path now
//! restores the file from the stored copy. The test lives in its own binary
//! because SQLite mode is selected process-wide.

use std::sync::Arc;

use narou_rs::converter::{ConverterCapabilities, NovelConverter, settings::NovelSettings};
use narou_rs::db::{NovelRecord, init_database, with_database_mut};
use narou_rs::downloader::{SectionElement, SectionFile, SubtitleInfo, TocFile};
use narou_rs::native::object_store::NativeStore;
use narou_rs::platform::mocks::{FakeRateLimiter, MockHttpClient};
use narou_rs::platform::{NovelObjectKeys, ObjectStore};

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
    }
}

#[test]
fn conversion_rebuilds_section_files_deleted_outside_narou() {
    let temp = tempfile::tempdir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
    std::fs::write(
        temp.path().join(".narou").join("storage-backend"),
        "sqlite",
    )
    .unwrap();
    init_database().unwrap();

    let novel_dir = temp.path().join("小説データ").join("site").join("n1234ab");
    std::fs::create_dir_all(novel_dir.join("本文")).unwrap();
    let toc = TocFile {
        title: "title".to_string(),
        author: "author".to_string(),
        toc_url: "https://example.com/".to_string(),
        story: None,
        subtitles: vec![subtitle()],
        novel_type: Some(2),
    };
    std::fs::write(
        novel_dir.join("toc.yaml"),
        serde_yaml::to_string(&toc).unwrap(),
    )
    .unwrap();

    let record: NovelRecord = serde_yaml::from_str(
        "id: 1\ntitle: title\nauthor: author\nfile_title: n1234ab\nsitename: site\n\
         toc_url: https://example.com/\nnovel_type: 2\nlast_update: 2026-01-01T00:00:00Z\n",
    )
    .unwrap();
    with_database_mut(|db| {
        db.insert(record);
        Ok(())
    })
    .unwrap();

    let keys = NovelObjectKeys::new("site", "n1234ab", false).unwrap();
    let store = NativeStore::for_narou_root(temp.path()).unwrap();
    futures::executor::block_on(store.write_small(
        &keys.section("1", "第一話"),
        serde_yaml::to_string(&section()).unwrap().into_bytes(),
    ))
    .unwrap();
    let section_path = novel_dir.join("本文").join("1 第一話.yaml");
    std::fs::remove_file(&section_path).unwrap();
    assert!(!section_path.exists());

    let settings = NovelSettings::load_for_novel(1, "title", "author", &novel_dir);
    let capabilities = ConverterCapabilities::new(
        Arc::new(MockHttpClient::new()),
        Arc::new(FakeRateLimiter::new()),
    );
    let mut converter = NovelConverter::with_capabilities(settings, capabilities);
    converter
        .convert_novel_by_id(1, &novel_dir)
        .expect("conversion must rebuild the deleted section file");

    assert!(
        section_path.is_file(),
        "the section mirror file must be restored from storage"
    );
}
