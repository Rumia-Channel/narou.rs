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
use std::path::{Path, PathBuf};

use aozora_epub3_lite::{
    AozoraConfig, EpubAsset, EpubBook, EpubMetadata, NavChapter, StyleSettings, TitleType,
    aozora_text_to_xhtml_sections_with_chapters, collect_image_alts, detect_meta_with_gaiji,
    escape_html, inline_to_xhtml, remove_metadata_lines, tcy_label,
};
use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};

/// chuki tables vendored from the Lite crate's bundled assets so that wasm
/// builds (no filesystem) see the same note definitions as CLI runs.
mod embedded {
    /// narou.rb のカスタム注記 (`ここから柱` / 前書き / 後書き / …)。
    /// `narou init` が同じ行をインストール先 `chuki_tag.txt` へ注入するため、
    /// ここで重ねておけば Java 実行と同じ注記が常に使える。
    pub const CUSTOM_CHUKI_TAG: &str = include_str!("../preset/custom_chuki_tag.txt");
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
    config.load_tag_text(embedded::CUSTOM_CHUKI_TAG);
    apply_narou_preset_flags(&mut config);
    config
}

/// narou.rb `preset/AozoraEpub3.ini`: TocPage/NavNest/NcxNest/TitleToc=1,
/// ChapterSection empty (off), ChapterH1/H2/H3=1。`narou init` はこの INI を
/// インストール先へコピーするので、INI がある場合はそちらの値が優先される。
fn apply_narou_preset_flags(config: &mut AozoraConfig) {
    config.toc_page = true;
    config.nav_nest = true;
    config.ncx_nest = true;
    config.title_toc = true;
    config.chapter_section = false;
    config.chapter_h1 = true;
    config.chapter_h2 = true;
    config.chapter_h3 = true;
}

/// 外部 AozoraEpub3 (Java) 実行時に読まれるものと同じ設定を組み立てる。
///
/// Java 版は `aozoraepub3dir` をカレントディレクトリとして起動し、そこにある
/// `chuki_*.txt` / `gaiji/*.ttf` / `AozoraEpub3.ini` を読む。組み込みエンジンでも
/// 同じディレクトリを渡すことで、同じ入力から同じ EPUB 構造になる。
/// ディレクトリが無い場合 (wasm / 未設定) は同梱資産だけで動く。
pub fn config_for(assets_dir: Option<&Path>) -> AozoraConfig {
    let mut config = match assets_dir.filter(|dir| dir.is_dir()) {
        Some(dir) => {
            let ini = dir.join("AozoraEpub3.ini");
            let preset = ini.is_file().then_some(ini.as_path());
            match AozoraConfig::load_from_dirs(&[dir], preset) {
                Ok(loaded) => {
                    let mut loaded = loaded;
                    if preset.is_none() {
                        apply_narou_preset_flags(&mut loaded);
                    }
                    loaded
                }
                Err(_) => embedded_config(),
            }
        }
        None => embedded_config(),
    };
    // インストール先の `chuki_tag.txt` に注入済みの行と同じものが入る。
    config.load_tag_text(embedded::CUSTOM_CHUKI_TAG);
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
    /// AozoraEpub3 インストール先 (`aozoraepub3dir`)。注記表・外字フォント・
    /// `AozoraEpub3.ini` をここから読む。`None` なら同梱資産のみで変換する。
    pub assets_dir: Option<PathBuf>,
    /// Java `-device kindle` 相当。
    pub kindle: bool,
    /// Cover image pixel size for the fixed-layout `cover.xhtml` viewport.
    pub cover_dimensions: Option<(u32, u32)>,
}

/// One illustration referenced by the text.
#[derive(Debug, Clone)]
pub struct EpubImage {
    /// Path inside the EPUB, e.g. `image/0001.png`.
    pub epub_path: String,
    pub media_type: &'static str,
    /// 挿絵ディレクトリ内の実ファイル名 (注記に書かれていた名前)。
    pub file_name: String,
}

/// 挿絵注記 (`［＃挿絵（挿絵/i1.png）入る］`) の本文パス。
static RE_ILLUST_TAG: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"［＃挿絵（(.+?)）入る］").unwrap());

/// 挿絵の注記を Java / Lite CLI と同じ連番 (`0001.png`) に書き換え、EPUB に
/// 格納する挿絵の一覧を返す。
///
/// 組み込みエンジンは注記のパスをそのまま `src` に出すため、注記側を書き換え
/// ないと本文と表紙で参照先が食い違う (表紙は常にファイル名で参照する)。
/// 連番は参照順、同じファイルは同じ番号になる。
pub fn prepare_images(text: &str) -> (String, Vec<EpubImage>) {
    let mut images: Vec<EpubImage> = Vec::new();
    let mut numbers: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let rewritten = RE_ILLUST_TAG
        .replace_all(text, |caps: &regex::Captures| {
            let source = caps[1].trim();
            let Some(file_name) = source.rsplit(['/', '\\']).next().map(str::trim) else {
                return caps[0].to_string();
            };
            let Some(media_type) = media_type_for(file_name) else {
                return caps[0].to_string();
            };
            let index = *numbers.entry(source.to_string()).or_insert_with(|| {
                images.push(EpubImage {
                    epub_path: format!("image/{:04}.{}", images.len() + 1, image_extension(file_name)),
                    media_type,
                    file_name: file_name.to_string(),
                });
                images.len()
            });
            format!(
                "［＃挿絵（{:04}.{}）入る］",
                index,
                image_extension(file_name)
            )
        })
        .into_owned();
    (rewritten, images)
}

/// Java は `.jpeg` を `.jpg` として格納する。
fn image_extension(file_name: &str) -> String {
    match file_name.rsplit_once('.') {
        Some((_, extension)) => {
            let extension = extension.to_ascii_lowercase();
            if extension == "jpeg" {
                "jpg".to_string()
            } else {
                extension
            }
        }
        None => String::new(),
    }
}

/// `stream_epub` の resolver 用に、EPUB パス → 実ファイル名の対応表を作る。
pub fn image_names(images: &[EpubImage]) -> std::collections::HashMap<String, String> {
    images
        .iter()
        .map(|image| (image.epub_path.clone(), image.file_name.clone()))
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

/// 表紙画像の実寸。`cover.xhtml` の viewport に使う (Java `cover.vm`)。
pub fn cover_dimensions(images_dir: &Path, image: &EpubImage) -> Option<(u32, u32)> {
    let data = std::fs::read(images_dir.join(&image.file_name)).ok()?;
    aozora_epub3_lite::image::dimensions(&data, image.media_type)
}

/// Deterministic UUID-formatted identifier derived from the source id.
fn stable_identifier(source_id: &str) -> String {
    let digest = Sha256::digest(source_id.as_bytes());
    let hex = hex::encode(&digest[..16]);
    format!(
        "urn:uuid:{:8}-{:4}-{:4}-{:4}-{:12}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Convert converted-novel text into an [`EpubBook`], mirroring the Lite CLI
/// pipeline: metadata strip, section split, chapter records → nav/ncx, and the
/// note tables, gaiji fonts and INI the Java engine would read from the
/// AozoraEpub3 install directory.
pub fn build_book(text: &str, options: &EpubBuildOptions, images: &[EpubImage]) -> Result<EpubBook> {
    let mut config = config_for(options.assets_dir.as_deref());
    // narou convert output is title-then-author, like the Lite CLI default.
    let detected = detect_meta_with_gaiji(text, TitleType::TitleAuthor, false, &config.gaiji);
    let body = remove_metadata_lines(text, &detected);
    // Java は入力ごとに imageAltMap を初期化する。
    config.image_alt_map.clear();
    collect_image_alts(&body, &mut config);
    // The reference pre-read consumes the first-chapter slot at the title
    // line; the body scan starts without a pending chapter then.
    let (sections, records) =
        aozora_text_to_xhtml_sections_with_chapters(&body, &config, detected.title_line.is_none())
            .map_err(|error| NarouError::Conversion(format!("EPUB変換に失敗しました: {error}")))?;
    let chapters = records
        .into_iter()
        .map(|record| {
            // Java Epub3Writer: TocVertical のときだけ章名をエスケープ後に
            // 縦中横変換へ通す。
            let (label, markup) = if config.toc_vertical {
                (tcy_label(&escape_html(&record.label), &config), true)
            } else {
                (record.label, false)
            };
            let mut chapter = NavChapter::new(
                label,
                format!("xhtml/{:04}.xhtml", record.section_index + 1),
            )
            .with_level(record.level)
            .with_markup(markup);
            if let Some(anchor) = record.anchor {
                chapter = chapter.with_anchor(anchor);
            }
            chapter
        })
        .collect::<Vec<_>>();

    let title = if options.title.is_empty() {
        detected.title.clone().unwrap_or_default()
    } else {
        options.title.clone()
    };
    let creator = if options.author.is_empty() {
        detected.creator.clone()
    } else {
        Some(options.author.clone())
    };
    let mut metadata = EpubMetadata::new(title.clone(), stable_identifier(&options.source_id));
    if let Some(creator) = creator.clone() {
        metadata = metadata.with_creator(creator);
    }
    // 表題・著者は入力テキスト側の生の行を変換する (Java と同じ)。
    // `TITLE_HORIZONTAL` の表題ページは縦書き変換を通さない。
    let title_line_config = if config.title_page_type == 2 {
        let mut horizontal = config.clone();
        horizontal.vertical = false;
        horizontal
    } else {
        config.clone()
    };
    let title_markup = inline_to_xhtml(
        metadata_line(text, detected.title_line).unwrap_or(title.as_str()),
        &title_line_config,
    );
    let creator_markup = creator.as_ref().map(|value| {
        inline_to_xhtml(
            metadata_line(text, detected.creator_line).unwrap_or(value.as_str()),
            &title_line_config,
        )
    });

    let mut assets = images
        .iter()
        .map(|image| EpubAsset::lazy(image.epub_path.clone(), image.media_type))
        .collect::<Vec<_>>();
    assets.extend(gaiji_font_assets(
        &config,
        &sections,
        &title_markup,
        creator_markup.as_deref(),
    )?);

    let title_page_selected = config.title_page_write && matches!(config.title_page_type, 1 | 2);
    let mut book = EpubBook::from_sections(metadata, sections)
        .with_vertical(options.vertical)
        .with_style(StyleSettings::from_ini(&config.ini))
        .with_toc_page(config.toc_page)
        .with_toc_vertical(config.toc_vertical)
        .with_toc_nest(config.nav_nest, config.ncx_nest)
        .with_title_toc(config.title_toc)
        .with_cover_page(config.cover_page, config.cover_page_toc)
        .with_kindle(options.kindle)
        .with_chapters(chapters)
        .with_metadata_markup(title_markup, creator_markup)
        .with_title_page_if(title_page_selected)
        .with_title_page_type(config.title_page_type)
        .with_assets(assets);
    if options.cover_from_first_image
        && let Some(first) = images.first()
    {
        book = book
            .with_cover_asset(first.epub_path.clone())
            .with_cover_dimensions(options.cover_dimensions);
    }
    Ok(book)
}

/// 入力テキストの指定行 (表題・著者) をそのまま取り出す。
fn metadata_line(text: &str, line: Option<usize>) -> Option<&str> {
    let line = text.lines().nth(line?)?.trim();
    (!line.is_empty()).then_some(line)
}

/// 本文・表題で参照された外字フォントだけを EPUB に格納する。Java も
/// タグから参照されたフォントしか `gaiji_N` として出力しない。
fn gaiji_font_assets(
    config: &AozoraConfig,
    sections: &[String],
    title_markup: &str,
    creator_markup: Option<&str>,
) -> Result<Vec<EpubAsset>> {
    let marker = |class_name: &str| format!("class=\"glyph {class_name}\"");
    let mut fonts = config
        .gaiji_fonts
        .iter()
        .filter(|(class_name, _)| {
            let marker = marker(class_name);
            sections.iter().any(|section| section.contains(&marker))
                || title_markup.contains(&marker)
                || creator_markup.is_some_and(|markup| markup.contains(&marker))
        })
        .map(|(class_name, path)| (class_name.clone(), path.clone()))
        .collect::<Vec<_>>();
    // 出現順 (manifest の並びを Java に合わせる)。
    fonts.sort_by_key(|(class_name, _)| {
        let marker = marker(class_name);
        sections
            .iter()
            .position(|section| section.contains(&marker))
            .unwrap_or(usize::MAX)
    });
    let mut assets = Vec::new();
    for (_, path) in fonts {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let epub_path = format!("gaiji/{name}");
        if assets.iter().any(|asset: &EpubAsset| asset.path == epub_path) {
            continue;
        }
        let data = std::fs::read(&path).map_err(|error| {
            NarouError::Conversion(format!(
                "外字フォントを読み込めません ({}): {}",
                path.display(),
                error
            ))
        })?;
        assets.push(EpubAsset::new(
            epub_path,
            "application/font-sfnt",
            data,
        ));
    }
    Ok(assets)
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
            assets_dir: None,
            kindle: false,
            cover_dimensions: None,
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
            assets_dir: None,
            kindle: false,
            cover_dimensions: None,
        };
        let (text, images) = prepare_images(&text);
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
        let (text, images) = prepare_images(&text);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].epub_path, "image/0001.png");

        let calls = Arc::new(AtomicUsize::new(0));
        let resolver_calls = calls.clone();
        let png: &[u8] = b"\x89PNG\r\n\x1a\nstub";
        let book = build_book(&text, &options(), &images).unwrap();
        let mut bytes = Vec::new();
        stream_epub(&book, &mut bytes, move |path| {
            if path == "image/0001.png" {
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
    fn prepare_images_numbers_references_and_skips_unknown_extensions() {
        let text = "t\na\n［＃挿絵（挿絵/a.png）入る］\n［＃挿絵（挿絵/a.png）入る］\n［＃挿絵（b.jpeg）入る］\n［＃挿絵（c.svg）入る］\n";
        let (rewritten, images) = prepare_images(text);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].epub_path, "image/0001.png");
        assert_eq!(images[0].file_name, "a.png");
        // Java は `.jpeg` を `.jpg` として格納する。
        assert_eq!(images[1].epub_path, "image/0002.jpg");
        assert_eq!(images[1].file_name, "b.jpeg");
        // 未対応拡張子は注記も資産もそのまま (EPUB に入れない)。
        assert!(rewritten.contains("［＃挿絵（c.svg）入る］"));
        assert_eq!(rewritten.matches("［＃挿絵（0001.png）入る］").count(), 2);
        assert!(rewritten.contains("［＃挿絵（0002.jpg）入る］"));
    }

    #[test]
    fn config_for_reads_install_directory_assets() {
        let dir = std::env::temp_dir().join(format!("narou-lite-assets-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("gaiji")).unwrap();
        std::fs::write(
            dir.join("AozoraEpub3.ini"),
            "SpaceHyphenation=1\nTitlePage=2\nLineHeight=2.1\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("chuki_tag.txt"),
            "テスト見出し\t<div class=\"test\">\t</div>\t1\n",
        )
        .unwrap();

        let config = config_for(Some(&dir));
        // Java が同じディレクトリから読む INI がそのまま効く。
        assert_eq!(config.ini.get("SpaceHyphenation"), Some("1"));
        assert_eq!(config.space_hyphenation, 1);
        assert_eq!(config.title_page_type, 2);
        let style = StyleSettings::from_ini(&config.ini);
        assert_eq!(style.line_height, "2.1");
        // インストール先の注記表と、narou が注入するカスタム注記の両方が載る。
        assert!(config.block_inline_tags.contains_key("テスト見出し"));
        assert!(config.block_inline_tags.contains_key("一字下げ"));

        std::fs::remove_dir_all(&dir).ok();

        // 資産が無い場合 (wasm / 未設定) は同梱資産だけで動く。
        let embedded = config_for(None);
        assert!(embedded.inline_notes.contains_key("傍点"));
        assert!(embedded.block_inline_tags.contains_key("一字下げ"));
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
