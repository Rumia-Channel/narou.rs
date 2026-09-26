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
    AozoraConfig, EpubAsset, EpubBook, Input, NavChapter, StyleSettings, TitleType,
    aozora_text_to_xhtml_sections_with_chapters, build_metadata, build_title_page_markup,
    collect_assets, collect_image_alts, decode_text, decorate_image_tags, detect_meta_with_gaiji,
    escape_html, image_references, inline_to_xhtml, is_auto_cover, remove_image_sources,
    remove_metadata_lines, remove_missing_image_sources, reflow_image_sections,
    rewrite_image_source, tcy_label,
};
use aozora_epub3_lite::pipeline::append_gaiji_assets;

use crate::error::{NarouError, Result};

/// ライブラリ利用側 (Worker など) が入力源を実装するために使う。
pub use aozora_epub3_lite::{FileSource, InputError};
/// 1 エントリずつ書き出すための Lite の型 (Worker のストリーミング応答で使う)。
pub use aozora_epub3_lite::{EpubEntryInfo, EpubStreamWriter};

/// chuki tables vendored from the Lite crate's bundled assets so that wasm
/// builds (no filesystem) see the same note definitions as CLI runs.
mod embedded {
    /// narou.rb のカスタム注記 (`ここから柱` / 前書き / 後書き / …)。
    /// `narou init` が同じ行をインストール先 `chuki_tag.txt` へ注入するため、
    /// ここで重ねておけば Java 実行と同じ注記が常に使える。
    pub const CUSTOM_CHUKI_TAG: &str = include_str!("../preset/custom_chuki_tag.txt");
    /// `narou init` がインストール先へコピーするプリセット INI。外部 AozoraEpub3 が
    /// 無い環境でも Java 版と同じ変換フラグ (TitlePage / CoverPage /
    /// SpaceHyphenation / DakutenType など) で動かすために読み込む。
    pub const AOZORA_INI: &str = include_str!("../preset/AozoraEpub3.ini");
    pub const CHUKI_TAG: &str = include_str!("../assets/aozora_lite/chuki_tag.txt");
    pub const CHUKI_TAG_SUF: &str = include_str!("../assets/aozora_lite/chuki_tag_suf.txt");
    pub const CHUKI_UTF: &str = include_str!("../assets/aozora_lite/chuki_utf.txt");
    pub const CHUKI_IVS: &str = include_str!("../assets/aozora_lite/chuki_ivs.txt");
    pub const CHUKI_ALT: &str = include_str!("../assets/aozora_lite/chuki_alt.txt");
    pub const CHUKI_LATIN: &str = include_str!("../assets/aozora_lite/chuki_latin.txt");
}

/// `AozoraConfig` equivalent to running the Lite CLI with its bundled assets
/// plus the narou `AozoraEpub3.ini` chapter/TOC flags (`preset/AozoraEpub3.ini`).
///
/// Built from strings on every call; cheap relative to text conversion and
/// keeps the config free of shared mutable state.
pub fn embedded_config() -> AozoraConfig {
    // narou.rb がインストール先へコピーするプリセット INI を既定にする。
    // Java 版は常にこの INI を読むため、外部 AozoraEpub3 が無い環境でも
    // 変換フラグが一致する。
    let ini = aozora_epub3_lite::IniSettings::parse(embedded::AOZORA_INI)
        .unwrap_or_default();
    let mut config = AozoraConfig::from_ini(ini);
    config.load_tag_text(embedded::CHUKI_TAG);
    config.load_suffix_text(embedded::CHUKI_TAG_SUF);
    config.load_utf_text(embedded::CHUKI_UTF);
    config.load_ivs_text(embedded::CHUKI_IVS);
    config.load_alt_text(embedded::CHUKI_ALT);
    config.load_latin_text(embedded::CHUKI_LATIN);
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
    pub vertical: bool,
    /// narou `-c 0`: promote the first illustration to the cover.
    pub cover_from_first_image: bool,
    /// AozoraEpub3 インストール先 (`aozoraepub3dir`)。注記表・外字フォント・
    /// `AozoraEpub3.ini` をここから読む。`None` なら同梱資産のみで変換する。
    pub assets_dir: Option<PathBuf>,
    /// Java `-device kindle` 相当。
    pub kindle: bool,
    /// 呼び出し側が足す EPUB 内アセット。
    ///
    /// 組み込みエンジン (Lite) には外部ツールのような `aozoraepub3dir` への
    /// ファイル流し込みが無いので、濁点フォント (`style/vertical_font.css` +
    /// `fonts/DMincho.ttf`) はここで渡す。
    pub extra_assets: Vec<EpubAsset>,
}

/// 書き出し時に読み出す挿絵。
#[derive(Debug, Clone)]
pub struct EpubImageSource {
    /// Path inside the EPUB, e.g. `image/0001.png`.
    pub epub_path: String,
    /// 読み出し名 (TXT 入力なら入力テキストの階層からの相対パス、ObjectStore
    /// 入力なら論理キー)。
    pub source: String,
    pub media_type: String,
    /// 表紙として書き出すか (Java は表紙だけ余白処理の条件が違う)。
    pub is_cover: bool,
    /// Java `imageInfo.rotateAngle`。
    pub rotate: i32,
}

/// [`build_book`] の結果。`book` を書き出す際は [`EpubBuild::resolve`] を
/// [`stream_epub`] の resolver に渡す。
pub struct EpubBuild {
    pub book: EpubBook,
    pub images: Vec<EpubImageSource>,
    config: AozoraConfig,
    input: Input,
}

impl EpubBuild {
    /// 挿絵 1 枚を読み出して Java と同じ前処理 (余白除去・リサイズ・回転) を
    /// かける。`stream_epub` は書き出し直前にこれを呼ぶので、画像は常に 1 枚
    /// ずつしかメモリに載らない。
    pub fn resolve(&self, epub_path: &str) -> Option<Vec<u8>> {
        let image = self
            .images
            .iter()
            .find(|image| image.epub_path == epub_path)?;
        let data = self
            .input
            .read_image(&image.source)
            .ok()
            .flatten()
            .or_else(|| {
                // TXT 入力はファイルシステムから読む (Lite CLI と同じ)。
                let base = self.input.path().parent()?;
                std::fs::read(base.join(image.source.replace('\\', "/"))).ok()
            })?;
        aozora_epub3_lite::image::process(
            &data,
            &image.media_type,
            &self.config.ini,
            image.is_cover,
            image.rotate,
        )
        .ok()
    }
}

/// 挿絵として格納できる拡張子か。ObjectStore の一覧から拾うときに使う。
pub fn is_supported_image(file_name: &str) -> bool {
    file_name
        .rsplit_once('.')
        .and_then(|(_, extension)| {
            aozora_epub3_lite::pipeline::media_type_for_extension(extension)
        })
        .is_some()
}
/// 変換済みテキストから EPUB を組み立てる。手順は Lite CLI
/// (`main.rs::convert_input`) と同じで、注記表・外字フォント・INI は
/// `aozoraepub3dir` から Java 版と同じものを読む。
///
/// 画像は寸法だけをここで読み、バイト列は書き出し時に [`EpubBuild::resolve`]
/// が 1 枚ずつ読み出して前処理する。
pub fn build_book(input_txt: &Path, options: &EpubBuildOptions) -> Result<EpubBuild> {
    let input = Input::open(input_txt).map_err(|error| {
        NarouError::Conversion(format!(
            "変換済みテキストを開けません ({}): {}",
            input_txt.display(),
            error
        ))
    })?;
    build_from_input(input, options)
}

/// ファイルシステムの無いランタイム (wasm / ObjectStore) 向け。入力テキストと
/// 挿絵を [`FileSource`] から供給して EPUB を組み立てる。
///
/// 注意: Lite の画像パイプライン (`collect_assets`) は現状 `FileSource` 入力の
/// 挿絵を解決できない (内部で `Input::is_archive()` しか見ていない) ため、この経路
/// では単ページ画像化・連番・表紙処理を行わず、注記のパスをそのまま格納先に
/// して参照が解決するようにする。ネイティブ経路 (`build_book`) は Java と同じ
/// パイプラインを通る。
///
/// [FileSource]: aozora_epub3_lite::FileSource
pub fn build_book_from_source(
    source: std::sync::Arc<dyn aozora_epub3_lite::FileSource>,
    options: &EpubBuildOptions,
) -> Result<EpubBuild> {
    let input = Input::from_source(source).map_err(|error| {
        NarouError::Conversion(format!("入力テキストを開けません: {error}"))
    })?;
    let Some(entry) = input.text_entries().first() else {
        return Err(NarouError::Conversion(
            "変換済みテキストが空です".to_string(),
        ));
    };
    let bytes = input.read_text(entry).map_err(|error| {
        NarouError::Conversion(format!("変換済みテキストを読めません: {error}"))
    })?;
    let text = decode_text(&bytes, Some("UTF-8")).map_err(|error| {
        NarouError::Conversion(format!("変換済みテキストを復号できません: {error}"))
    })?;

    let mut config = config_for(options.assets_dir.as_deref());
    let detected = detect_meta_with_gaiji(&text, TitleType::TitleAuthor, false, &config.gaiji);
    let body = remove_metadata_lines(&text, &detected);
    config.image_alt_map.clear();
    collect_image_alts(&body, &mut config);
    let (sections, records) =
        aozora_text_to_xhtml_sections_with_chapters(&body, &config, detected.title_line.is_none())
            .map_err(|error| NarouError::Conversion(format!("EPUB変換に失敗しました: {error}")))?;

    // 注記のパスをそのまま EPUB の格納先にする (レンダラが同じパスを参照する)。
    let mut images = Vec::new();
    let mut epub_assets = Vec::new();
    for reference in image_references(&text) {
        let Some(source_path) = input.resolve_image_path(entry, &reference) else {
            continue;
        };
        let Some(extension) = source_path.rsplit_once('.').map(|(_, extension)| extension) else {
            continue;
        };
        let Some(media_type) =
            aozora_epub3_lite::pipeline::media_type_for_extension(extension)
        else {
            continue;
        };
        let epub_path = format!("image/{source_path}");
        epub_assets.push(EpubAsset::lazy(epub_path.clone(), media_type));
        images.push(EpubImageSource {
            epub_path,
            source: source_path,
            media_type: media_type.to_string(),
            is_cover: false,
            rotate: 0,
        });
    }

    let chapters = nav_chapters(records, &config);
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
    let metadata = build_metadata(&title, creator.as_deref(), None, None);
    let book = EpubBook::from_sections(metadata, sections)
        .with_vertical(options.vertical)
        .with_style(StyleSettings::from_ini(&config.ini))
        .with_toc_page(config.toc_page)
        .with_toc_vertical(config.toc_vertical)
        .with_toc_nest(config.nav_nest, config.ncx_nest)
        .with_title_toc(config.title_toc)
        .with_kindle(options.kindle)
        .with_chapters(chapters)
        .with_assets(epub_assets);

    Ok(EpubBuild {
        book,
        images,
        config,
        input,
    })
}

/// 章レコードを nav/NCX 用の並びへ変換する (Java `Epub3Writer` と同じ規則)。
fn nav_chapters(records: Vec<aozora_epub3_lite::ChapterRecord>, config: &AozoraConfig) -> Vec<NavChapter> {
    records
        .into_iter()
        .map(|record| {
            // Java Epub3Writer: TocVertical のときだけ章名をエスケープ後に
            // 縦中横変換へ通す。
            let (label, markup) = if config.toc_vertical {
                (tcy_label(&escape_html(&record.label), config), true)
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
        .collect()
}

fn build_from_input(input: Input, options: &EpubBuildOptions) -> Result<EpubBuild> {
    let mut config = config_for(options.assets_dir.as_deref());
    let Some(entry) = input.text_entries().first() else {
        return Err(NarouError::Conversion(
            "変換済みテキストが空です".to_string(),
        ));
    };
    let bytes = input.read_text(entry).map_err(|error| {
        NarouError::Conversion(format!("変換済みテキストを読めません: {error}"))
    })?;
    let text = decode_text(&bytes, Some("UTF-8")).map_err(|error| {
        NarouError::Conversion(format!("変換済みテキストを復号できません: {error}"))
    })?;
    // narou convert output is title-then-author, like the Lite CLI default.
    let detected = detect_meta_with_gaiji(&text, TitleType::TitleAuthor, false, &config.gaiji);
    let body = remove_metadata_lines(&text, &detected);
    // Java は入力ごとに imageAltMap を初期化する。
    config.image_alt_map.clear();
    collect_image_alts(&body, &mut config);
    // The reference pre-read consumes the first-chapter slot at the title
    // line; the body scan starts without a pending chapter then.
    let (mut sections, mut records) =
        aozora_text_to_xhtml_sections_with_chapters(&body, &config, detected.title_line.is_none())
            .map_err(|error| NarouError::Conversion(format!("EPUB変換に失敗しました: {error}")))?;

    let cover_setting = options.cover_from_first_image.then_some("0");
    // NoIllust では本文から消えた挿絵を EPUB に格納しない。
    let body_filter = config.no_illust.then(|| sections.concat());
    let (mut assets, cover) = collect_assets(
        &input,
        entry,
        &text,
        cover_setting,
        body_filter.as_deref(),
    )
    .map_err(|error| NarouError::Conversion(format!("挿絵を収集できません: {error}")))?;
    // 装飾は書き換え前に実行する (元の参照名で解決できたかで配置クラスが変わる)。
    decorate_image_tags(&mut sections, &mut assets, &config, input.is_archive());
    for collected in &assets {
        for reference in &collected.references {
            if collected.resolved != *reference {
                rewrite_image_source(&mut sections, reference, &collected.resolved);
            }
        }
    }
    let resolved_references = assets
        .iter()
        .flat_map(|asset| asset.references.iter().cloned())
        .collect::<Vec<_>>();
    remove_missing_image_sources(&mut sections, &image_references(&text), &resolved_references);
    // Java Epub3Writer.getImageFilePath: 表紙ページへ移動した挿絵は本文から除く。
    if config.cover_page
        && is_auto_cover(cover_setting)
        && let Some(cover) = cover.as_deref()
    {
        remove_image_sources(&mut sections, &[cover.to_owned()]);
    }
    reflow_image_sections(&mut sections, &mut records, &assets, &config);

    let chapters = nav_chapters(records, &config);

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
    // 識別子は Java と同じ `urn:uuid:` (MD5 of `title-creator`)。
    let metadata = build_metadata(&title, creator.as_deref(), None, None);
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
        metadata_line(&text, detected.title_line).unwrap_or(title.as_str()),
        &title_line_config,
    );
    let creator_markup = creator.as_ref().map(|value| {
        inline_to_xhtml(
            metadata_line(&text, detected.creator_line).unwrap_or(value.as_str()),
            &title_line_config,
        )
    });
    let title_page_markup = build_title_page_markup(&text, &detected, &config, options.vertical);

    let mut epub_assets = assets
        .iter()
        .map(|collected| collected.asset.clone())
        .collect::<Vec<_>>();
    append_gaiji_assets(
        &mut epub_assets,
        &config,
        &sections,
        &title_markup,
        creator_markup.as_deref(),
        title_page_markup.as_deref(),
    )
    .map_err(|error| NarouError::Conversion(format!("外字フォントを収集できません: {error}")))?;
    for asset in &options.extra_assets {
        if !epub_assets.iter().any(|existing| existing.path == asset.path) {
            epub_assets.push(asset.clone());
        }
    }

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
        .with_title_page_type(config.title_page_type)
        .with_title_page_if(title_page_selected)
        .with_assets(epub_assets);
    if let Some(title_page_markup) = title_page_markup {
        book = book.with_title_page_markup(title_page_markup);
    }
    if let Some(cover) = cover.as_deref() {
        // Java cover.vm の viewport / viewBox は表紙画像の元寸法を使う。
        let dimensions = assets
            .iter()
            .find(|collected| collected.asset.path == cover)
            .and_then(|collected| collected.dimensions)
            .map(|dimensions| (dimensions.width, dimensions.height));
        book = book
            .with_cover_asset(cover.to_owned())
            .with_cover_dimensions(dimensions);
    }

    let images = assets
        .iter()
        .map(|collected| EpubImageSource {
            epub_path: collected.asset.path.clone(),
            source: collected.source.clone(),
            media_type: collected.asset.media_type.clone(),
            is_cover: collected.is_cover,
            rotate: collected.rotate,
        })
        .collect();

    Ok(EpubBuild {
        book,
        images,
        config,
        input,
    })
}

/// 入力テキストの指定行 (表題・著者) をそのまま取り出す。
fn metadata_line(text: &str, line: Option<usize>) -> Option<&str> {
    let line = text.lines().nth(line?)?.trim();
    (!line.is_empty()).then_some(line)
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

/// EPUB 書き出しのシンク。1 エントリ分のバイト列を溜め、書き出し側が
/// エントリを書き終えるたびに [`ChunkSink::take`] で引き取る。
///
/// `Clone` しても同じバッファを共有するので、`EpubStreamWriter` に渡した
/// clone が書いた内容を呼び出し側のハンドルから読める (Worker の応答のように
/// チャンクを 1 つずつ外へ流す用途を想定)。
#[derive(Clone, Default)]
pub struct ChunkSink {
    buffer: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
}

impl ChunkSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// 溜まっているバイト列を取り出してバッファを空にする。
    pub fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.buffer.borrow_mut())
    }
}

impl Write for ChunkSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 挿絵のバイト列を書き出し直前に注入できる入力源。
///
/// 構築 ([`build_book_from_source`]) は挿絵のパス一覧しか見ないので、バイト列は
/// 一覧に載せた名前へ書き出し直前に [`LazyImageSource::insert_image`] で入れる。
/// これで挿絵を全部はメモリに載せずに EPUB を組める (Worker は ObjectStore から
/// 書き出し直前の 1 枚だけ読む)。
#[derive(Debug)]
pub struct LazyImageSource {
    text: String,
    names: Vec<String>,
    images: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl LazyImageSource {
    /// `image_names` は本文の参照と同じ相対パス (`挿絵/foo.jpg`) で渡す。
    pub fn new(text: String, image_names: Vec<String>) -> Self {
        let mut names = vec!["novel.txt".to_string()];
        names.extend(image_names);
        Self {
            text,
            names,
            images: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 書き出し直前の挿絵 1 枚を差し込む。
    pub fn insert_image(&self, name: &str, bytes: Vec<u8>) {
        if let Ok(mut images) = self.images.lock() {
            images.insert(name.to_string(), bytes);
        }
    }

    /// 書き出しが済んだ挿絵を解放し、保持していればそのバイト列を返す。
    /// 1 エントリ書き終えるごとに呼ぶことで、保持するのは常に 1 枚分になる。
    pub fn remove_image(&self, name: &str) -> Option<Vec<u8>> {
        self.images
            .lock()
            .ok()
            .and_then(|mut images| images.remove(name))
    }
}

impl FileSource for LazyImageSource {
    fn list(&self) -> &[String] {
        &self.names
    }

    fn open(
        &self,
        name: &str,
    ) -> std::result::Result<Option<Box<dyn std::io::Read + Send>>, InputError> {
        if name == "novel.txt" {
            return Ok(Some(Box::new(std::io::Cursor::new(
                self.text.clone().into_bytes(),
            ))));
        }
        let bytes = self
            .images
            .lock()
            .ok()
            .and_then(|images| images.get(name).cloned());
        Ok(bytes
            .map(|bytes| Box::new(std::io::Cursor::new(bytes)) as Box<dyn std::io::Read + Send>))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const SAMPLE: &str = "テスト小説\nテスト著者\n\n　これはテストです。ルビ《てすと》の確認。\n";

    fn options() -> EpubBuildOptions {
        EpubBuildOptions {
            title: "テスト小説".to_string(),
            author: "テスト著者".to_string(),
            vertical: true,
            cover_from_first_image: false,
            assets_dir: None,
            kindle: false,
            extra_assets: Vec::new(),
        }
    }

    /// 一時ディレクトリに `novel.txt` (と任意の挿絵) を置く。
    fn text_file(name: &str, text: &str, illustration: Option<&[u8]>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("narou-lite-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("挿絵")).unwrap();
        let path = dir.join("novel.txt");
        std::fs::write(&path, text).unwrap();
        if let Some(bytes) = illustration {
            std::fs::write(dir.join("挿絵/i1.png"), bytes).unwrap();
        }
        path
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
        let options = EpubBuildOptions {
            title: lines.next().unwrap_or("untitled").to_string(),
            author: lines.next().unwrap_or("unknown").to_string(),
            cover_from_first_image: true,
            ..options()
        };
        let build = build_book(Path::new(&path), &options).unwrap();
        let mut bytes = Vec::new();
        stream_epub(&build.book, &mut bytes, |path| build.resolve(path)).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"));
        assert!(bytes.len() > 4096, "epub suspiciously small: {}", bytes.len());
    }

    #[test]
    fn builds_streamable_epub_from_a_text_file() {
        let path = text_file("plain", SAMPLE, None);
        let build = build_book(&path, &options()).unwrap();
        let mut bytes = Vec::new();
        stream_epub(&build.book, &mut bytes, |path| build.resolve(path)).unwrap();

        assert!(bytes.starts_with(b"PK\x03\x04"));
        // mimetype is required to be the first, uncompressed entry.
        assert!(bytes.windows(8).any(|w| w == b"mimetype"));
        assert!(bytes.windows(20).any(|w| w == b"application/epub+zip"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn collects_and_resolves_illustrations() {
        let png: &[u8] = b"\x89PNG\r\n\x1a\nstub";
        let text = format!("{SAMPLE}\n　挿絵。［＃挿絵（挿絵/i1.png）入る］\n");
        let path = text_file("illust", &text, Some(png));
        let build = build_book(&path, &options()).unwrap();

        // 実ファイルは挿絵ディレクトリから、EPUB パスは Java と同じ連番で。
        assert_eq!(build.images.len(), 1);
        assert_eq!(build.images[0].epub_path, "image/0001.png");
        assert_eq!(build.resolve("image/0001.png").as_deref(), Some(png));
        assert!(build.resolve("image/9999.png").is_none());

        let mut bytes = Vec::new();
        stream_epub(&build.book, &mut bytes, |path| build.resolve(path)).unwrap();
        let needle = b"item/image/0001.png";
        assert!(bytes.windows(needle.len()).any(|window| window == needle));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn builds_from_an_in_memory_source() {
        // Worker (wasm) と同じ経路: ファイルシステム無しで入力と挿絵を渡す。
        #[derive(Debug)]
        struct Memory {
            files: Vec<(String, Vec<u8>)>,
            names: Vec<String>,
        }
        impl FileSource for Memory {
            fn list(&self) -> &[String] {
                &self.names
            }
            fn open(
                &self,
                name: &str,
            ) -> std::result::Result<Option<Box<dyn std::io::Read + Send>>, InputError> {
                Ok(self
                    .files
                    .iter()
                    .find(|(candidate, _)| candidate == name)
                    .map(|(_, bytes)| {
                        Box::new(std::io::Cursor::new(bytes.clone())) as Box<dyn std::io::Read + Send>
                    }))
            }
        }

        let png: &[u8] = b"\x89PNG\r\n\x1a\nstub";
        let text = format!("{SAMPLE}\n　挿絵。［＃挿絵（挿絵/i1.png）入る］\n");
        let files = vec![
            ("novel.txt".to_string(), text.into_bytes()),
            ("挿絵/i1.png".to_string(), png.to_vec()),
        ];
        let names = files.iter().map(|(name, _)| name.clone()).collect();
        let build = build_book_from_source(Arc::new(Memory { files, names }), &options()).unwrap();

        // この経路は番号化しないので、注記のパスがそのまま格納先になる。
        assert_eq!(build.images.len(), 1);
        assert_eq!(build.images[0].epub_path, "image/挿絵/i1.png");
        assert_eq!(build.resolve("image/挿絵/i1.png").as_deref(), Some(png));

        // 本文の参照 (<img src="../image/挿絵/i1.png">) と格納先が一致すること。
        let mut bytes = Vec::new();
        stream_epub(&build.book, &mut bytes, |path| build.resolve(path)).unwrap();
        let needle = "item/image/挿絵/i1.png".as_bytes();
        assert!(bytes.windows(needle.len()).any(|window| window == needle));
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

    /// 1 エントリずつ書き出す経路 (Worker の `download.epub` と同じ手順) が
    /// 一括書き出しと同一バイトを返すこと。エントリごとにチャンクが取れることと、
    /// 最後のチャンク (ZIP の集中ディレクトリ) を取りこぼさないことも固定する。
    #[test]
    fn entry_by_entry_writer_matches_batch_output() {
        let png: &[u8] = b"\x89PNG\r\n\x1a\nstub";
        let text = format!("{SAMPLE}\n　挿絵。［＃挿絵（挿絵/i1.png）入る］\n");
        let path = text_file("stream", &text, Some(png));
        let build = build_book(&path, &options()).unwrap();

        let mut batch = Vec::new();
        stream_epub(&build.book, &mut batch, |path| build.resolve(path)).unwrap();

        let sink = ChunkSink::new();
        let mut writer = build.book.stream_writer(sink.clone()).unwrap();
        let mut streamed = Vec::new();
        let mut chunks = 0;
        while let Some(info) = writer.next_entry() {
            let bytes = info
                .asset_path
                .as_deref()
                .and_then(|path| build.resolve(path));
            writer.write_current(bytes.as_deref()).unwrap();
            let chunk = sink.take();
            if !chunk.is_empty() {
                chunks += 1;
                streamed.extend_from_slice(&chunk);
            }
        }
        writer.finish().unwrap();
        let tail = sink.take();
        if !tail.is_empty() {
            chunks += 1;
            streamed.extend_from_slice(&tail);
        }

        assert_eq!(streamed, batch);
        assert!(chunks > 3, "1 エントリずつ流れていない: {chunks} chunks");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// 挿絵のバイト列を書き出し直前に注入する経路 (Worker と同じ) でも EPUB が
    /// 組めること。構築はパス一覧しか見ないので注入前は解決できず、注入した
    /// 1 枚だけが格納され、書き出し後は解放される (先読みと同じメモリに戻らない)。
    #[test]
    fn lazy_source_takes_illustrations_at_write_time() {
        let png: &[u8] = b"\x89PNG\r\n\x1a\nstub";
        let text = format!(
            "{SAMPLE}\n　挿絵。［＃挿絵（挿絵/i1.png）入る］\n［＃挿絵（挿絵/i2.png）入る］\n"
        );
        let names = vec!["挿絵/i1.png".to_string(), "挿絵/i2.png".to_string()];
        let source = Arc::new(LazyImageSource::new(text, names.clone()));
        let build = build_book_from_source(source.clone(), &options()).unwrap();

        assert_eq!(build.images.len(), 2);
        let epub_path = build.images[0].epub_path.clone();
        // 構築時にバイト列は要求されない (注入前は解決できない)。
        assert!(build.resolve(&epub_path).is_none());

        let sink = ChunkSink::new();
        let mut writer = build.book.stream_writer(sink.clone()).unwrap();
        let mut streamed = Vec::new();
        while let Some(info) = writer.next_entry() {
            let bytes = info.asset_path.as_deref().and_then(|path| {
                let name = path.strip_prefix("image/")?;
                source.insert_image(name, png.to_vec());
                let bytes = build.resolve(path);
                // 書き出したら解放する (保持するのは常に 1 枚分)。
                assert!(source.remove_image(name).is_some());
                bytes
            });
            writer.write_current(bytes.as_deref()).unwrap();
            streamed.extend(sink.take());
        }
        writer.finish().unwrap();
        streamed.extend(sink.take());

        assert!(streamed.starts_with(b"PK\x03\x04"));
        for name in &names {
            let needle = format!("item/image/{name}");
            assert!(streamed.windows(needle.len()).any(|window| window == needle.as_bytes()));
            assert!(source.remove_image(name).is_none(), "{name} が解放されていない");
        }
    }
}

