//! In-process EPUB generation backed by [AozoraEpub3_Lite].
//!
//! Compiled only with the `lite` cargo feature. The Worker runtime enables it
//! implicitly (`worker-runtime = ["lite"]`); native builds opt in explicitly.
//! Binaries linked against AozoraEpub3_Lite are GPL-3.0-only — see
//! `assets/aozora_lite/LICENSE.md`.
//!
//! The conversion pipeline mirrors what the external Java/Lite CLI would do:
//! a converted 青空文庫 text plus optional illustrations go in, a streamed
//! EPUB 3 container comes out. Nothing here touches the filesystem or the
//! network; callers supply bytes through the asset resolver closure so the
//! same code path works on wasm32 and on desktop runtimes.
//!
//! [AozoraEpub3_Lite]: https://github.com/Rumia-Channel/AozoraEpub3_Lite

use std::io::Write;

use aozora_epub3_lite::{
    AozoraConfig, EpubAsset, EpubBook, EpubMetadata, NavChapter, TitleType,
    aozora_text_to_xhtml_sections_with_chapters, detect_meta_with_gaiji, remove_metadata_lines,
};
use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};

/// chuki tables vendored from the Lite crate's bundled assets so that wasm
/// builds (no filesystem) see the same note definitions as CLI runs.
mod embedded {
    pub const CHUKI_TAG: &str = include_str!("../assets/aozora_lite/chuki_tag.txt");
    pub const CHUKI_TAG_SUF: &str = include_str!("../assets/aozora_lite/chuki_tag_suf.txt");
    pub const CHUKI_UTF: &str = include_str!("../assets/aozora_lite/chuki_utf.txt");
    pub const CHUKI_IVS: &str = include_str!("../assets/aozora_lite/chuki_ivs.txt");
    pub const CHUKI_ALT: &str = include_str!("../assets/aozora_lite/chuki_alt.txt");
    pub const CHUKI_LATIN: &str = include_str!("../assets/aozora_lite/chuki_latin.txt");
    pub const REPLACE: &str = include_str!("../assets/aozora_lite/replace.txt");
}

/// `AozoraConfig` equivalent to running the Lite CLI with its bundled assets
/// plus the narou `AozoraEpub3.ini` chapter/TOC flags (`preset/AozoraEpub3.ini`).
///
/// Built from strings on every call; cheap relative to text conversion and
/// keeps the config free of shared mutable state.
pub fn embedded_config() -> AozoraConfig {
    let mut config = AozoraConfig::default();
    config.load_tag_text(embedded::CHUKI_TAG);
    config.load_suffix_text(embedded::CHUKI_TAG_SUF);
    config.load_utf_text(embedded::CHUKI_UTF);
    config.load_ivs_text(embedded::CHUKI_IVS);
    config.load_alt_text(embedded::CHUKI_ALT);
    config.load_latin_text(embedded::CHUKI_LATIN);
    config.load_replace_text(embedded::REPLACE);
    // narou.rb `preset/AozoraEpub3.ini`: TocPage/NavNest/NcxNest/TitleToc=1,
    // ChapterSection empty (off), ChapterH1/H2/H3=1.
    config.toc_page = true;
    config.nav_nest = true;
    config.ncx_nest = true;
    config.title_toc = true;
    config.chapter_section = false;
    config.chapter_h1 = true;
    config.chapter_h2 = true;
    config.chapter_h3 = true;
    config
}

/// Metadata and layout options for one EPUB build.
#[derive(Debug, Clone)]
pub struct EpubBuildOptions {
    pub title: String,
    pub author: String,
    /// Stable source identifier (e.g. the novel TOC URL); hashed into a
    /// deterministic `urn:uuid:` so re-builds keep the same book identity.
    pub source_id: String,
    pub vertical: bool,
    /// narou `-c 0`: promote the first illustration to the cover.
    pub cover_from_first_image: bool,
}

/// One illustration referenced by the text.
#[derive(Debug, Clone)]
pub struct EpubImage {
    /// Path inside the EPUB, e.g. `image/i1186242.png`.
    pub epub_path: String,
    pub media_type: &'static str,
}

/// Unique illustration references in source order, as `image/<name>` EPUB
/// paths. Duplicate references collapse to one entry.
pub fn image_entries(text: &str) -> Vec<EpubImage> {
    let mut seen = std::collections::HashSet::new();
    aozora_epub3_lite::image_references(text)
        .into_iter()
        .filter_map(|reference| {
            let name = reference.rsplit(['/', '\\']).next()?.trim().to_string();
            if name.is_empty() || !seen.insert(name.clone()) {
                return None;
            }
            let media_type = media_type_for(&name)?;
            Some(EpubImage {
                epub_path: format!("image/{name}"),
                media_type,
            })
        })
        .collect()
}

/// Media type for an illustration file name; `None` for unsupported
/// extensions so broken references never enter the manifest.
pub fn media_type_for(file_name: &str) -> Option<&'static str> {
    let lower = file_name.rsplit('.').next()?.to_ascii_lowercase();
    match lower.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Deterministic UUID-formatted identifier derived from the source id.
fn stable_identifier(source_id: &str) -> String {
    let digest = Sha256::digest(source_id.as_bytes());
    let hex = hex::encode(&digest[..16]);
    format!(
        "{:8}-{:4}-{:4}-{:4}-{:12}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Convert converted-novel text into an [`EpubBook`], mirroring the Lite CLI
/// pipeline: metadata strip, section split, chapter records → nav/ncx.
pub fn build_book(text: &str, options: &EpubBuildOptions, images: &[EpubImage]) -> Result<EpubBook> {
    let config = embedded_config();
    // narou convert output is title-then-author, like the Lite CLI default.
    let detected = detect_meta_with_gaiji(text, TitleType::TitleAuthor, false, &config.gaiji);
    let body = remove_metadata_lines(text, &detected);
    // The reference pre-read consumes the first-chapter slot at the title
    // line; the body scan starts without a pending chapter then.
    let (sections, records) =
        aozora_text_to_xhtml_sections_with_chapters(&body, &config, detected.title_line.is_none())
            .map_err(|error| NarouError::Conversion(format!("EPUB変換に失敗しました: {error}")))?;
    let chapters = records
        .into_iter()
        .map(|record| {
            let mut chapter = NavChapter::new(
                record.label,
                format!("xhtml/{:04}.xhtml", record.section_index + 1),
            )
            .with_level(record.level);
            if let Some(anchor) = record.anchor {
                chapter = chapter.with_anchor(anchor);
            }
            chapter
        })
        .collect::<Vec<_>>();

    let metadata = EpubMetadata::new(options.title.clone(), stable_identifier(&options.source_id));
    let mut book = EpubBook::from_sections(metadata, sections)
        .with_vertical(options.vertical)
        .with_toc_page(config.toc_page)
        .with_toc_nest(config.nav_nest, config.ncx_nest)
        .with_title_toc(config.title_toc)
        .with_chapters(chapters)
        .with_assets(
            images
                .iter()
                .map(|image| EpubAsset::lazy(image.epub_path.clone(), image.media_type))
                .collect::<Vec<_>>(),
        );
    if options.cover_from_first_image
        && let Some(first) = images.first()
    {
        book = book.with_cover_asset(first.epub_path.clone());
    }
    Ok(book)
}

/// Stream the book to `writer` as a seek-less ZIP (data descriptors), safe
/// for HTTP response bodies. `resolve` receives the EPUB path of each lazy
/// asset (`image/…`) exactly when it is about to be written.
pub fn stream_epub<W: Write>(
    book: &EpubBook,
    writer: W,
    resolve: impl Fn(&str) -> Option<Vec<u8>>,
) -> Result<()> {
    book.write_to_stream_with(writer, resolve)
        .map(|_| ())
        .map_err(|error| NarouError::Conversion(format!("EPUB書き出しに失敗しました: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const SAMPLE: &str = "テスト小説\nテスト著者\n\n　これはテストです。ルビ《てすと》の確認。\n";

    fn options() -> EpubBuildOptions {
        EpubBuildOptions {
            title: "テスト小説".to_string(),
            author: "テスト著者".to_string(),
            source_id: "https://example.com/n1234ab/".to_string(),
            vertical: true,
            cover_from_first_image: false,
        }
    }

    #[test]
    fn embedded_config_loads_chuki_tables() {
        // The vendored tables are non-empty; parsing them must not panic.
        // load_tag_text registers note rules, so re-loading must be safe and
        // the API surface used by build_book must stay infallible.
        let _first = embedded_config();
        let mut second = embedded_config();
        second.load_tag_text(embedded::CHUKI_TAG);
        assert!(!embedded::CHUKI_TAG.is_empty());
    }

    #[test]
    fn streams_real_converted_text_when_available() {
        // Opt-in smoke: NAROU_EPUB_SMOKE_TXT points at a real converted
        // 青空文庫 text. Skipped silently when the variable is absent so the
        // suite stays hermetic.
        let Ok(path) = std::env::var("NAROU_EPUB_SMOKE_TXT") else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let mut lines = text.lines();
        let title = lines.next().unwrap_or("untitled").to_string();
        let author = lines.next().unwrap_or("unknown").to_string();
        let options = EpubBuildOptions {
            title,
            author,
            source_id: path.clone(),
            vertical: true,
            cover_from_first_image: true,
        };
        let images = image_entries(&text);
        let book = build_book(&text, &options, &images).unwrap();
        let mut bytes = Vec::new();
        stream_epub(&book, &mut bytes, |_| None).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"));
        assert!(bytes.len() > 4096, "epub suspiciously small: {}", bytes.len());
    }

    #[test]
    fn builds_streamable_epub_without_images() {
        let book = build_book(SAMPLE, &options(), &[]).unwrap();
        let mut bytes = Vec::new();
        stream_epub(&book, &mut bytes, |_| None).unwrap();

        assert!(bytes.starts_with(b"PK\x03\x04"));
        // mimetype is required to be the first, uncompressed entry.
        assert!(bytes.windows(8).any(|w| w == b"mimetype"));
        assert!(bytes.windows(20).any(|w| w == b"application/epub+zip"));
    }

    #[test]
    fn streams_lazy_images_through_resolver_once() {
        let text = format!("{SAMPLE}\n　挿絵。［＃挿絵（i1.png）入る］\n");
        let images = image_entries(&text);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].epub_path, "image/i1.png");

        let calls = Arc::new(AtomicUsize::new(0));
        let resolver_calls = calls.clone();
        let png: &[u8] = b"\x89PNG\r\n\x1a\nstub";
        let book = build_book(&text, &options(), &images).unwrap();
        let mut bytes = Vec::new();
        stream_epub(&book, &mut bytes, move |path| {
            if path == "image/i1.png" {
                resolver_calls.fetch_add(1, Ordering::SeqCst);
                return Some(png.to_vec());
            }
            None
        })
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(bytes.starts_with(b"PK\x03\x04"));
    }

    #[test]
    fn image_entries_deduplicate_and_skip_unknown_extensions() {
        let text = "t\na\n［＃挿絵（a.png）入る］\n［＃挿絵（a.png）入る］\n［＃挿絵（b.svg）入る］\n";
        let entries = image_entries(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].media_type, "image/png");
    }

    #[test]
    fn identifier_is_deterministic_uuid_form() {
        let a = stable_identifier("https://ncode.syosetu.com/n1234ab/");
        let b = stable_identifier("https://ncode.syosetu.com/n1234ab/");
        let c = stable_identifier("other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("urn:uuid:"));
    }
}
