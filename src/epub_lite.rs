//! In-process EPUB generation backed by [AozoraEpub3_Lite].
//!
//! Compiled only with the `lite` cargo feature. The Worker runtime enables it
//! implicitly (`worker-runtime = ["lite"]`); native builds opt in explicitly.
//! Binaries linked against AozoraEpub3_Lite are GPL-3.0-only — see
//! `assets/aozora_lite/LICENSE.md`.
//!
//! A converted 青空文庫 text plus optional illustrations go through one layout
//! pipeline, producing a streamed EPUB 3 container. Native inputs supply image
//! dimensions and pixels through filesystem/archive reads; asynchronous callers
//! provide dimensions to the upstream asset plan and pixels at write time.
//!
//! [AozoraEpub3_Lite]: https://github.com/Rumia-Channel/AozoraEpub3_Lite

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aozora_epub3_lite::{
    AozoraConfig, BookMeta, ChapterRecord, EpubAsset, EpubBook, IniSettings, Input, NavChapter,
    StyleSettings, TitleType, aozora_text_to_xhtml_sections_with_chapters,
    build_metadata, build_title_page_markup, collect_assets, collect_image_alts, decode_text,
    decorate_image_tags, detect_meta_with_gaiji, escape_html, image_references, inline_to_xhtml,
    remove_image_sources, remove_metadata_lines, remove_missing_image_sources,
    reflow_image_sections, rewrite_image_source, tcy_label,
};
use aozora_epub3_lite::pipeline::{append_gaiji_assets, plan_assets};

use crate::converter::settings::{ImageRotation, NovelSettings};
use crate::error::{NarouError, Result};

/// ライブラリ利用側 (Worker など) が入力源を実装するために使う。
pub use aozora_epub3_lite::{FileSource, InputError};
/// 1 エントリずつ書き出すための Lite の型 (Worker のストリーミング応答で使う)。
pub use aozora_epub3_lite::{EpubEntryInfo, EpubStreamWriter};
/// 画像を 1 枚ずつ非同期に調べるための上流の計画・寸法型。
pub use aozora_epub3_lite::pipeline::{
    AssetMetadataRequest, AssetPlan, CollectedAsset, ImageDimensions, image_dimensions,
};

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
    /// 濁点フォント本体 (`preset/DMincho.ttf`)。native は `preset/` の実体を
    /// 読む (`converter::dakuten_font`) ので、ここは同じ内容の埋め込みコピー —
    /// ファイルシステムの無い wasm (Worker) 向け。
    pub const DAKUTEN_FONT_TTF: &[u8] = include_bytes!("../preset/DMincho.ttf");
    /// 濁点フォント用スタイルシート (`preset/vertical_font_with_dakuten.css`)。
    /// 行高は `<%= line_height %>` を展開して使う。
    pub const DAKUTEN_CSS_TEMPLATE: &str = include_str!("../preset/vertical_font_with_dakuten.css");
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

/// INI 1 項目だけ差し替えた `IniSettings` を返す。`RotateImage` は回転角の
/// 決定時 (`pipeline::image_rotation` / `rotate_for_image`) に都度読まれる
/// ので、`AozoraConfig::from_ini` の派生フィールドには影響しない。
///
/// `IniSettings` にセッターが無いため、全項目を直列化して該当キーだけを
/// 差し替えて再パースする。`iter()` はセクションを `Section.Key` の平坦な
/// キーとして返し、`parse` はそのキーをそのまま保持するので往復で一致する。
/// (考えられる限りのエッジケース: デコード済み値がリテラルな `\uXXXX` を含む
/// と再パース時に 1 回展開される。実際の AozoraEpub3.ini には無い。)
fn ini_with_rotate_image(ini: &IniSettings, value: &str) -> IniSettings {
    let mut text = String::new();
    let mut replaced = false;
    for (key, item) in ini.iter() {
        if key == "RotateImage" {
            text.push_str("RotateImage=");
            text.push_str(value);
            text.push('\n');
            replaced = true;
        } else {
            text.push_str(key);
            text.push('=');
            text.push_str(item);
            text.push('\n');
        }
    }
    if !replaced {
        text.push_str("RotateImage=");
        text.push_str(value);
        text.push('\n');
    }
    IniSettings::parse(&text).unwrap_or_else(|_| ini.clone())
}

/// `convert.rotate-image` (未指定 = INI に従う) を `config.ini` の
/// `RotateImage` へ反映する。
fn apply_rotate_image(config: &mut AozoraConfig, rotation: Option<ImageRotation>) {
    if let Some(rotation) = rotation {
        config.ini = ini_with_rotate_image(&config.ini, rotation.ini_value());
    }
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
    /// `fonts/DMincho.ttf`) はここで渡す。組立規則は [`dakuten_font_assets`]
    /// (wasm: 埋め込み) / native `dakuten_font::lite_font_assets` (preset の実体)。
    pub extra_assets: Vec<EpubAsset>,
    /// `convert.rotate-image` の解決値。`None` (未指定) は INI の
    /// `RotateImage` に従う (AozoraEpub3 と同じ挙動)。
    ///
    /// 角度自体ではなく本家の「回転する/しない・方向」の差し替え。回る条件
    /// (向き・大きさ・ページ種別) は AozoraEpub3 の規則のまま。
    pub rotate_image: Option<ImageRotation>,
}

/// 濁点注記のマーカー (変換側 `converter_base::convert_dakuten_char_to_font` が
/// 本文へ入れる)。`convert.epub-font=auto` の判定に使う。
pub const DAKUTEN_CHUKI_MARKER: &str = "［＃濁点］";

/// 変換済みテキストに濁点注記があるか (`convert.epub-font=auto` の判定)。
/// native CLI の `file_contains_dakuten_chuki` と Worker の走査に同じ。
pub fn text_has_dakuten_marker(text: &str) -> bool {
    text.contains(DAKUTEN_CHUKI_MARKER)
}

/// 設定 (per-novel `setting.ini` + 設定マップ + 環境設定) から
/// [`EpubBuildOptions`] を組む入力。
///
/// `NovelSettings` の組み立て (`NovelSettings::load_for_novel` / Worker の
/// ObjectStore + D1) は呼び出し側の仕事。ここは「保存済み設定 → 組立
/// オプション」の規則だけを持つ。
pub struct EpubSettingsInput<'a> {
    pub title: &'a str,
    pub author: &'a str,
    /// per-novel 設定 (`enable_yokogaki` = 横書き、`enable_illust` = 挿絵)。
    pub settings: &'a NovelSettings,
    /// 表紙に使える挿絵 (表紙候補) があるか。
    ///
    /// 判定の規則は保存構成ごとに違う: native Web は `cover.{jpg,png,jpeg}`
    /// の実在 (`converter::render::cover_image_path`)、Worker は挿絵の実体を
    /// 一覧から拾った結果。[`EpubBuildOptions::from_settings`] はこの値に
    /// `enable_illust` を掛けたものを表紙条件にする。native CLI は Java の
    /// `-c 0` と同じく `cover.{jpg,png,jpeg}` の実在だけで決め `enable_illust`
    /// を見ないため、この組み立てを使わない。
    pub has_images: bool,
    /// 呼び出し側が組んだ EPUB 資産 ([`EpubBuildOptions::extra_assets`])。
    ///
    /// 濁点フォントは [`dakuten_font_assets`] (wasm: 埋め込みコピー) か
    /// native `dakuten_font::lite_font_assets` (preset の実体) で組む —
    /// バイト源が実行環境の差なので、ここでは決めない。
    pub extra_assets: Vec<EpubAsset>,
    /// AozoraEpub3 の資産ディレクトリ。組み込みエンジンだけなら `None`。
    pub assets_dir: Option<PathBuf>,
    /// Java `-device kindle` 相当。
    pub kindle: bool,
    /// `convert.rotate-image`。
    pub rotate_image: Option<ImageRotation>,
}

impl EpubBuildOptions {
    /// 保存済み設定から組立オプションを作る (native Web / Worker 共通の規則)。
    ///
    /// - 書字方向は per-novel `enable_yokogaki` (横書き指定) の否定。縦書きの
    ///   小説を勝手に横組みにしない / 横書き指定を縦組みにしない。
    /// - 表紙は `enable_illust` かつ表紙候補があるときだけ
    ///   ([`EpubSettingsInput::has_images`] の判定は構成ごとに違う)。
    /// - 濁点フォントは [`EpubSettingsInput::extra_assets`] のとおり。
    pub fn from_settings(input: EpubSettingsInput<'_>) -> Self {
        Self {
            title: input.title.to_string(),
            author: input.author.to_string(),
            vertical: !input.settings.enable_yokogaki,
            cover_from_first_image: input.settings.enable_illust && input.has_images,
            assets_dir: input.assets_dir,
            kindle: input.kindle,
            extra_assets: input.extra_assets,
            rotate_image: input.rotate_image,
        }
    }
}

/// `convert.epub-font` の保存値が `always` か (native / Worker 共通の判定)。
/// `auto` (未設定含む) は本文の濁点注記でのみ濁点フォントを使う。
pub fn epub_font_always(setting: Option<&str>) -> bool {
    setting.is_some_and(|value| value.trim().eq_ignore_ascii_case("always"))
}

/// 濁点フォントを EPUB に埋め込む条件: 本文に濁点注記がある (`auto`) か
/// [`epub_font_always`]。
pub fn needs_dakuten_font(use_dakuten_font: bool, always: bool) -> bool {
    use_dakuten_font || always
}

/// 濁点フォント資産一式の組立規則 (パス `style/vertical_font.css` +
/// `fonts/DMincho.ttf`、media type、`always` の付加 CSS) はここが唯一の持ち主。
///
/// バイト源 (`css_template` / `font`) だけが実行環境の差で、native は preset の
/// 実体 (`dakuten_font::lite_font_assets`)、wasm (Worker) は同梱の埋め込み
/// コピー ([`dakuten_font_assets`]) を渡す。
pub fn dakuten_font_assets_from(
    css_template: &str,
    font: &[u8],
    always: bool,
    line_height: f64,
) -> Vec<EpubAsset> {
    let mut css = css_template.replace(
        "<%= line_height %>",
        &crate::converter::settings::format_line_height(line_height),
    );
    if always {
        css.push_str(ALWAYS_BODY_FONT_CSS);
    }
    vec![
        EpubAsset::new("style/vertical_font.css", "text/css", css.into_bytes()),
        EpubAsset::new("fonts/DMincho.ttf", "application/font-sfnt", font.to_vec()),
    ]
}

/// `convert.epub-font=always` のとき本文全体を DMincho で組む追記 CSS。
const ALWAYS_BODY_FONT_CSS: &str =
    "\n/* narou: convert.epub-font=always — 本文全体を DMincho で組む */\n\nbody, p {\n\tfont-family: \"DakutenAokinMincho\", serif;\n}\n";

/// 埋め込みコピーで濁点フォント資産を組む (ファイルシステムの無い wasm / 未設定
/// の native 向け)。
///
/// - `use_dakuten_font`: 本文に濁点注記がある (auto の対象)。
/// - `always`: `convert.epub-font=always`。
/// - `line_height`: CSS の `<%= line_height %>` へ展開する値。
///
/// どちらも偽なら空を返す ([`needs_dakuten_font`])。
pub fn dakuten_font_assets(
    use_dakuten_font: bool,
    always: bool,
    line_height: f64,
) -> Vec<EpubAsset> {
    if !needs_dakuten_font(use_dakuten_font, always) {
        return Vec::new();
    }
    dakuten_font_assets_from(
        embedded::DAKUTEN_CSS_TEMPLATE,
        embedded::DAKUTEN_FONT_TTF,
        always,
        line_height,
    )
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
    /// Sorted by `epub_path` for [`Self::image`] lookup.
    pub images: Vec<EpubImageSource>,
    config: AozoraConfig,
    input: Input,
}

impl EpubBuild {
    /// EPUB の連番パスに対応する、保存元と計画済み画像処理を引く。
    pub fn image(&self, epub_path: &str) -> Option<&EpubImageSource> {
        let index = self
            .images
            .binary_search_by(|image| image.epub_path.as_str().cmp(epub_path))
            .ok()?;
        Some(&self.images[index])
    }

    /// 呼び出し側が読み出した 1 枚を、native と同じ計画済み条件で処理する。
    pub fn process_image(&self, epub_path: &str, data: Vec<u8>) -> Result<Vec<u8>> {
        let image = self.image(epub_path).ok_or_else(|| {
            NarouError::Conversion(format!("EPUB画像が計画にありません: {epub_path}"))
        })?;
        aozora_epub3_lite::image::process(
            &data,
            &image.media_type,
            &self.config.ini,
            image.is_cover,
            image.rotate,
        )
        .map_err(|error| {
            NarouError::Conversion(format!("挿絵を処理できません ({}): {error}", image.source))
        })
    }

    /// 挿絵 1 枚を読み出して Java と同じ前処理 (余白除去・リサイズ・回転) を
    /// かける。`stream_epub` は書き出し直前にこれを呼ぶので、画像は常に 1 枚
    /// ずつしかメモリに載らない。
    pub fn resolve(&self, epub_path: &str) -> Option<Vec<u8>> {
        let image = self.image(epub_path)?;
        let data = if self.input.is_archive() || self.input.has_source() {
            self.input.read_image(&image.source).ok()??
        } else {
            let base = self.input.path().parent()?;
            std::fs::read(base.join(image.source.replace('\\', "/"))).ok()?
        };
        self.process_image(epub_path, data).ok()
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


/// 呼び出し側が渡した EPUB 資産 ([`EpubBuildOptions::extra_assets`]) を、外字・
/// 挿絵と同じ規則で本文アセットへ積む (同じパスは先着優先)。
fn append_extra_assets(assets: &mut Vec<EpubAsset>, extra: &[EpubAsset]) {
    for asset in extra {
        if !assets.iter().any(|existing| existing.path == asset.path) {
            assets.push(asset.clone());
        }
    }
}

/// 入力・本文変換と画像メタデータ取得の間で所有する、共通の EPUB 組立状態。
///
/// [`Self::asset_plan`] の寸法要求だけを入力環境に合わせて満たし、完了した資産を
/// [`Self::finish`] に渡す。画像の連番化・表紙・装飾・再フローは上流と共有する。
pub struct EpubPreparation {
    input: Input,
    config: AozoraConfig,
    detected: BookMeta,
    text: String,
    sections: Vec<String>,
    records: Vec<ChapterRecord>,
    cover_setting: Option<&'static str>,
    body_filter: Option<String>,
}

impl EpubPreparation {
    fn from_input(input: Input, options: &EpubBuildOptions) -> Result<Self> {
        let mut config = config_for(options.assets_dir.as_deref());
        apply_rotate_image(&mut config, options.rotate_image);
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
        // narou の変換出力は表題→著者の順 (Lite CLI の既定と同じ)。
        let detected = detect_meta_with_gaiji(&text, TitleType::TitleAuthor, false, &config.gaiji);
        let body = remove_metadata_lines(&text, &detected);
        // Java は入力ごとに imageAltMap を初期化する。
        config.image_alt_map.clear();
        collect_image_alts(&body, &mut config);
        // 挿絵注記の先読みが表題行の第 1 章スロットを消費するため、本文走査は
        // 保留章なしで始める。
        let (sections, records) =
            aozora_text_to_xhtml_sections_with_chapters(&body, &config, detected.title_line.is_none())
                .map_err(|error| NarouError::Conversion(format!("EPUB変換に失敗しました: {error}")))?;
        // NoIllust では本文から消えた挿絵を EPUB に格納しない。
        let body_filter = config.no_illust.then(|| sections.concat());
        Ok(Self {
            input,
            config,
            detected,
            text,
            sections,
            records,
            cover_setting: options.cover_from_first_image.then_some("0"),
            body_filter,
        })
    }

    /// native の `collect_assets` と同じ入力で、画像寸法の逐次取得を始める。
    pub fn asset_plan(&self) -> Result<AssetPlan<'_>> {
        plan_assets(
            &self.input,
            &self.input.text_entries()[0],
            &self.text,
            self.cover_setting,
            self.body_filter.as_deref(),
        )
        .map_err(|error| NarouError::Conversion(format!("挿絵を収集できません: {error}")))
    }

    /// 画像計画の結果を使って、すべての入力源に共通のレイアウトを仕上げる。
    pub fn finish(
        self,
        mut assets: Vec<CollectedAsset>,
        cover: Option<String>,
        options: &EpubBuildOptions,
    ) -> Result<EpubBuild> {
        let Self {
            input,
            config,
            detected,
            text,
            mut sections,
            mut records,
            ..
        } = self;
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
        let references = image_references(&text);
        if input.has_source() && !config.no_illust {
            let unresolved = illustration_references(&text)
                .into_iter()
                .filter(|reference| !resolved_references.contains(reference))
                .collect::<Vec<_>>();
            report_illustration_resolution(&unresolved, assets.len());
        }
        remove_missing_image_sources(&mut sections, &references, &resolved_references);
        remove_promoted_cover(&mut sections, &config, cover.as_deref());
        reflow_image_sections(&mut sections, &mut records, &assets, &config);

        let (title, creator) = resolved_title_creator(options, &detected);
        let title_markup = build_title_markup(
            &text,
            &detected,
            &config,
            title,
            creator,
            options.vertical,
        );
        let cover = cover.map(|cover| {
            // Java cover.vm の viewport / viewBox は表紙画像の元寸法を使う。
            let dimensions = assets
                .iter()
                .find(|collected| collected.asset.path == cover)
                .and_then(|collected| collected.dimensions)
                .map(|dimensions| (dimensions.width, dimensions.height));
            (cover, dimensions)
        });
        let mut epub_assets = Vec::with_capacity(assets.len());
        let mut images = Vec::with_capacity(assets.len());
        for collected in assets {
            images.push(EpubImageSource {
                epub_path: collected.asset.path.clone(),
                source: collected.source,
                media_type: collected.asset.media_type.clone(),
                is_cover: collected.is_cover,
                rotate: collected.rotate,
            });
            epub_assets.push(collected.asset);
        }
        append_gaiji_assets(
            &mut epub_assets,
            &config,
            &sections,
            &title_markup.title,
            title_markup.creator.as_deref(),
            title_markup.page.as_deref(),
        )
        .map_err(|error| NarouError::Conversion(format!("外字フォントを収集できません: {error}")))?;
        append_extra_assets(&mut epub_assets, &options.extra_assets);
        let book = finish_book(
            &config,
            options,
            title,
            creator,
            sections,
            records,
            epub_assets,
            cover,
            Some(title_markup),
        );
        images.sort_unstable_by(|left, right| left.epub_path.cmp(&right.epub_path));
        Ok(EpubBuild {
            book,
            images,
            config,
            input,
        })
    }
}

/// 保存済み設定の表題・著者 (空なら本文の検出値) を採る規則 (両経路共通)。
fn resolved_title_creator<'a>(
    options: &'a EpubBuildOptions,
    detected: &'a BookMeta,
) -> (&'a str, Option<&'a str>) {
    let title = if options.title.is_empty() {
        detected.title.as_deref().unwrap_or_default()
    } else {
        options.title.as_str()
    };
    let creator = if options.author.is_empty() {
        detected.creator.as_deref()
    } else {
        Some(options.author.as_str())
    };
    (title, creator)
}

/// 扉ページ・書誌の markup (すべての入力源で共通)。
struct TitleMarkup {
    /// `<metadata>` の表題 (入力テキストの生の行を変換したもの)。
    title: String,
    creator: Option<String>,
    /// `TitlePage=1|2` の扉ページ本文 (`TitlePageWrite=0` のときは未使用)。
    page: Option<String>,
}

/// 外部ツール (Java / Lite CLI) と同じ扉ページ・書誌 markup を作る。
///
/// 表題・著者は入力テキスト側の生の行を変換する (`metadata_line`)。
/// `TITLE_HORIZONTAL` の扉ページは縦書き変換を通さない。
fn build_title_markup(
    text: &str,
    detected: &BookMeta,
    config: &AozoraConfig,
    title: &str,
    creator: Option<&str>,
    vertical: bool,
) -> TitleMarkup {
    let title_line_config = if config.title_page_type == 2 {
        let mut horizontal = config.clone();
        horizontal.vertical = false;
        horizontal
    } else {
        config.clone()
    };
    let title_markup = inline_to_xhtml(
        metadata_line(text, detected.title_line).unwrap_or(title),
        &title_line_config,
    );
    let creator_markup = creator.map(|value| {
        inline_to_xhtml(
            metadata_line(text, detected.creator_line).unwrap_or(value),
            &title_line_config,
        )
    });
    let page = build_title_page_markup(text, detected, config, vertical);
    TitleMarkup {
        title: title_markup,
        creator: creator_markup,
        page,
    }
}

/// 組立の仕上げ段 (環境非依存): nav/NCX → metadata → [`EpubBook`] → 表紙。
///
/// 画像の寸法取得だけが入力環境の差で、その結果 (資産と表紙) をここへ渡す。
/// 扉ページと書誌 markup も同じ設定・生成処理を使う。
fn finish_book(
    config: &AozoraConfig,
    options: &EpubBuildOptions,
    title: &str,
    creator: Option<&str>,
    sections: Vec<String>,
    records: Vec<ChapterRecord>,
    epub_assets: Vec<EpubAsset>,
    cover: Option<(String, Option<(u32, u32)>)>,
    title_markup: Option<TitleMarkup>,
) -> EpubBook {
    let chapters = nav_chapters(records, config);
    // 識別子は Java と同じ `urn:uuid:` (MD5 of `title-creator`)。
    let metadata = build_metadata(title, creator, None, None);
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
        .with_assets(epub_assets);
    if let Some(title_markup) = title_markup {
        // 扉ページは `TitlePageWrite=1` かつ `TitlePage=1|2` のときだけ挿入する
        // (Java `bookInfo.insertTitlePage` と同じ)。
        let title_page_selected =
            config.title_page_write && matches!(config.title_page_type, 1 | 2);
        book = book
            .with_metadata_markup(title_markup.title, title_markup.creator)
            .with_title_page_type(config.title_page_type)
            .with_title_page_if(title_page_selected);
        if let Some(markup) = title_markup.page {
            book = book.with_title_page_markup(markup);
        }
    }
    if let Some((cover_path, dimensions)) = cover {
        // 表紙ページの viewport / viewBox は表紙画像の元寸法を使う
        // (Java `cover.vm` と同じ)。
        book = book
            .with_cover_asset(cover_path)
            .with_cover_dimensions(dimensions);
    }
    book
}


/// Java Epub3Writer.getImageFilePath: 表紙ページへ移した挿絵は本文から取り除く
/// (`-c 0` 相当で表紙へ昇格した 1 枚だけ)。
///
/// 昇格済みの `cover` は `options.cover_from_first_image` のときだけ Some に
/// なるため、ここでの条件は `CoverPage` INI だけ。
fn remove_promoted_cover(sections: &mut [String], config: &AozoraConfig, cover: Option<&str>) {
    if config.cover_page && let Some(cover) = cover {
        remove_image_sources(sections, &[cover.to_owned()]);
    }
}

/// 入力テキストの指定行 (表題・著者) をそのまま取り出す。
fn metadata_line(text: &str, line: Option<usize>) -> Option<&str> {
    let line = text.lines().nth(line?)?.trim();
    (!line.is_empty()).then_some(line)
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

/// 挿絵参照の解決結果を報告する。
///
/// 本文が参照している挿絵が入力に無い場合、EPUB からその挿絵が落ちる —
/// 見た目は「挿絵が入っていない EPUB」になるだけで、既定では何も出ない。
/// 原因を追えるよう、未解決は常に警告し、判断の内訳は `webui.debug-mode` の
/// ときだけ詳細ログへ流す。
fn report_illustration_resolution(unresolved: &[String], resolved: usize) {
    use crate::application::debug;
    use crate::application::messages;

    for reference in unresolved {
        messages::emit_default(
            messages::Stream::Stderr,
            &messages::download::warn_illustration_missing(reference),
        );
    }
    if !debug::enabled() {
        return;
    }
    debug::emit(format!(
        "挿絵: 解決 {} 件 / 未解決 {} 件",
        resolved,
        unresolved.len()
    ));
    for reference in unresolved {
        debug::emit(format!("挿絵: 本文の参照が入力に見つかりません: {reference}"));
    }
}

/// 本文が参照している挿絵の一覧 (重複なし)。詳細ログ・診断用。
///
/// Lite の `image_references` はパス区切りを正規化するため `https://…` の
/// ような URL を参照として認識しない (空セグメントを弾く)。Worker の本文は
/// 取得元 URL のままなので、URL も参照として拾う。
pub fn illustration_references(text: &str) -> Vec<String> {
    let mut references = Vec::new();
    for caps in note_pattern().captures_iter(text) {
        push_reference(&mut references, &caps[1]);
    }
    for caps in image_pattern().captures_iter(text) {
        push_reference(&mut references, &caps[2]);
    }
    references
}

/// 挿絵注記 (`［＃挿絵（PATH、説明）入る］`) の PATH を捕まえる。
fn note_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        regex::Regex::new(r"［＃挿絵（([^、）]+)(?:、[^）]*)?）入る］")
            .expect("illustration note pattern is valid")
    })
}

/// 生の `<img src="…">` を捕まえる (1: src= まで, 2: 値, 3: 閉じ引用符)。
fn image_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        regex::Regex::new(r#"(?i)(<img[^>]+src=["'])([^"']+)(["'])"#)
            .expect("illustration img pattern is valid")
    })
}

fn push_reference(references: &mut Vec<String>, reference: &str) {
    let reference = reference.trim();
    if !reference.is_empty() && !references.iter().any(|known| known == reference) {
        references.push(reference.to_string());
    }
}

/// 本文の挿絵参照を、保存済みの名前 (`挿絵/<file>`) へ寄せる。
///
/// Worker の変換は native と違い挿絵をローカライズしないため、本文には
/// サイト上の URL がそのまま残る。ダウンロード時に保存した実体は
/// `.illustration_cache.yaml` (取得元 → ファイル名) が知っているので、EPUB を
/// 組む前にここで参照を差し替える。`resolve` が `None` を返した参照は元の
/// まま残し、戻り値の 2 番目に (重複なしで) 集める。
///
/// 置換は注記 (`［＃挿絵（X、説明）入る］`) と生の `src="X"` の両方の書き方を
/// 見る。注記では説明文をそのまま残す。
pub fn rewrite_illustration_references(
    text: &str,
    resolve: impl Fn(&str) -> Option<String>,
) -> (String, Vec<String>) {
    let mut unresolved: Vec<String> = Vec::new();
    let mut rewritten = String::with_capacity(text.len());
    let mut last = 0;
    for caps in note_pattern().captures_iter(text) {
        let whole = caps.get(0).expect("whole match");
        let path_match = caps.get(1).expect("path group");
        let reference = path_match.as_str().trim();
        let Some(localized) = resolve(reference) else {
            push_reference(&mut unresolved, reference);
            continue;
        };
        rewritten.push_str(&text[last..whole.start()]);
        rewritten.push_str(&text[whole.start()..path_match.start()]);
        rewritten.push_str(&localized);
        rewritten.push_str(&text[path_match.end()..whole.end()]);
        last = whole.end();
    }
    rewritten.push_str(&text[last..]);

    let rewritten = image_pattern()
        .replace_all(&rewritten, |caps: &regex::Captures| {
            let reference = caps.get(2).expect("source group").as_str().trim();
            match resolve(reference) {
                Some(localized) => format!(
                    "{}{}{}",
                    caps.get(1).expect("prefix group").as_str(),
                    localized,
                    caps.get(3).expect("quote group").as_str()
                ),
                None => {
                    push_reference(&mut unresolved, reference);
                    caps.get(0).expect("whole match").as_str().to_string()
                }
            }
        })
        .into_owned();
    (rewritten, unresolved)
}

/// 非同期画像 I/O を持つランタイム向けに、本文と共通レイアウトの準備を行う。
///
/// [`EpubPreparation::asset_plan`] に画像寸法を供給してから、その結果を
/// [`EpubPreparation::finish`] に渡す。画像のバイト列は入力源へ注入しない。
pub fn prepare_book_from_source(
    source: Arc<dyn FileSource>,
    options: &EpubBuildOptions,
) -> Result<EpubPreparation> {
    let input = Input::from_source(source).map_err(|error| {
        NarouError::Conversion(format!("入力テキストを開けません: {error}"))
    })?;
    EpubPreparation::from_input(input, options)
}

/// 章レコードを nav/NCX 用の並びへ変換する (Java `Epub3Writer` と同じ規則)。
fn nav_chapters(records: Vec<ChapterRecord>, config: &AozoraConfig) -> Vec<NavChapter> {
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
    let preparation = EpubPreparation::from_input(input, options)?;
    let (assets, cover) = collect_assets(
        &preparation.input,
        &preparation.input.text_entries()[0],
        &preparation.text,
        preparation.cover_setting,
        preparation.body_filter.as_deref(),
    )
    .map_err(|error| NarouError::Conversion(format!("挿絵を収集できません: {error}")))?;
    preparation.finish(assets, cover, options)
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

/// 本文だけを同期に開き、挿絵は名前だけを列挙する入力源。
///
/// 挿絵の寸法とバイト列は呼び出し側の非同期 I/O で取得し、画像計画と
/// [`EpubBuild::process_image`] へ直接渡す。
#[derive(Debug)]
pub struct EpubTextSource {
    text: Arc<[u8]>,
    names: Vec<String>,
}

impl EpubTextSource {
    /// `image_names` は本文の参照と同じ相対パス (`挿絵/foo.jpg`) で渡す。
    pub fn new(text: String, image_names: Vec<String>) -> Self {
        let mut names = Vec::with_capacity(image_names.len() + 1);
        names.push("novel.txt".to_string());
        names.extend(image_names);
        Self {
            text: text.into_bytes().into(),
            names,
        }
    }
}

impl FileSource for EpubTextSource {
    fn list(&self) -> &[String] {
        &self.names
    }

    fn open(
        &self,
        name: &str,
    ) -> std::result::Result<Option<Box<dyn std::io::Read + Send>>, InputError> {
        Ok((name == "novel.txt").then(|| {
            Box::new(std::io::Cursor::new(self.text.clone())) as Box<dyn std::io::Read + Send>
        }))
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
            rotate_image: None,
        }
    }

    /// 一時ディレクトリに `novel.txt` (と任意の挿絵) を置く。
    fn text_file(text: &str, illustration: Option<&[u8]>) -> (tempfile::TempDir, PathBuf) {
        let guard = tempfile::tempdir().unwrap();
        let dir = guard.path();
        std::fs::create_dir_all(dir.join("挿絵")).unwrap();
        let path = dir.join("novel.txt");
        std::fs::write(&path, text).unwrap();
        if let Some(bytes) = illustration {
            std::fs::write(dir.join("挿絵/i1.png"), bytes).unwrap();
        }
        (guard, path)
    }

    #[derive(Debug)]
    struct Memory {
        files: Vec<(String, Arc<[u8]>)>,
        names: Vec<String>,
    }

    impl Memory {
        fn new(files: Vec<(String, Vec<u8>)>) -> Self {
            let names = files.iter().map(|(name, _)| name.clone()).collect();
            let files = files
                .into_iter()
                .map(|(name, bytes)| (name, Arc::<[u8]>::from(bytes)))
                .collect();
            Self { files, names }
        }
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

    fn source_assets(preparation: &EpubPreparation) -> Result<(Vec<CollectedAsset>, Option<String>)> {
        preparation
            .asset_plan()?
            .finish(|request| {
                let bytes = preparation.input.read_image(request.source)?.ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("image entry not found: {}", request.source),
                    )
                })?;
                Ok(image_dimensions(&bytes, request.media_type))
            })
            .map_err(|error| NarouError::Conversion(format!("挿絵を収集できません: {error}")))
    }


    #[test]
    fn builds_streamable_epub_from_a_text_file() {
        let (_directory, path) = text_file(SAMPLE, None);
        let build = build_book(&path, &options()).unwrap();
        let mut bytes = Vec::new();
        stream_epub(&build.book, &mut bytes, |path| build.resolve(path)).unwrap();

        assert!(bytes.starts_with(b"PK\x03\x04"));
        // mimetype is required to be the first, uncompressed entry.
        assert!(bytes.windows(8).any(|w| w == b"mimetype"));
        assert!(bytes.windows(20).any(|w| w == b"application/epub+zip"));
    }

    #[test]
    fn collects_and_resolves_illustrations() {
        let png = test_png(32, 32);
        let text = format!("{SAMPLE}\n　挿絵。［＃挿絵（挿絵/i1.png）入る］\n");
        let (_directory, path) = text_file(&text, Some(&png));
        let build = build_book(&path, &options()).unwrap();

        // 実ファイルは挿絵ディレクトリから、EPUB パスは Java と同じ連番で。
        assert_eq!(build.images.len(), 1);
        assert_eq!(build.images[0].epub_path, "image/0001.png");
        let resolved = build.resolve("image/0001.png").unwrap();
        assert_eq!(
            aozora_epub3_lite::image::dimensions(&resolved, "image/png"),
            Some((32, 32))
        );
        assert_eq!(build.image("image/0001.png").unwrap().source, "挿絵/i1.png");
        assert!(build.resolve("image/9999.png").is_none());

        let mut bytes = Vec::new();
        stream_epub(&build.book, &mut bytes, |path| build.resolve(path)).unwrap();
        let needle = b"item/image/0001.png";
        assert!(bytes.windows(needle.len()).any(|window| window == needle));
    }

    #[test]
    fn rewrites_illustration_references_to_stored_names() {
        // Worker の本文は取得元 URL のままなので、保存済みの名前へ寄せる。
        let text = format!("{SAMPLE}\n　挿絵。［＃挿絵（https://example.invalid/a.png）入る］\n");
        let (rewritten, unresolved) = rewrite_illustration_references(&text, |reference| {
            (reference == "https://example.invalid/a.png").then(|| "挿絵/0001.png".to_string())
        });
        assert!(
            rewritten.contains("［＃挿絵（挿絵/0001.png）入る］"),
            "{rewritten}"
        );
        assert!(!rewritten.contains("example.invalid"), "{rewritten}");
        assert!(unresolved.is_empty());
    }

    #[test]
    fn keeps_unresolved_references_and_reports_each_once() {
        let text = format!(
            "{SAMPLE}\n［＃挿絵（https://example.invalid/a.png）入る］\n\
             ［＃挿絵（https://example.invalid/a.png）入る］\n［＃挿絵（挿絵/local.png）入る］\n"
        );
        let (rewritten, unresolved) = rewrite_illustration_references(&text, |_| None);
        assert_eq!(rewritten, text, "解決できない参照は書き換えない");
        assert_eq!(
            unresolved,
            vec![
                "https://example.invalid/a.png".to_string(),
                "挿絵/local.png".to_string()
            ]
        );
    }

    #[test]
    fn rewrites_raw_img_sources_too() {
        let text = format!("{SAMPLE}\n<img src=\"https://example.invalid/b.jpg\" alt=\"x\">\n");
        let (rewritten, unresolved) =
            rewrite_illustration_references(&text, |_| Some("挿絵/b.jpg".to_string()));
        assert!(rewritten.contains(r#"src="挿絵/b.jpg""#), "{rewritten}");
        assert!(unresolved.is_empty());
    }

    #[test]
    fn builds_from_an_in_memory_source() {
        let text = format!("{SAMPLE}\n　挿絵。［＃挿絵（挿絵/i1.png）入る］\n");
        let source = Arc::new(Memory::new(vec![
            ("novel.txt".to_string(), text.into_bytes()),
            ("挿絵/i1.png".to_string(), test_png(32, 32)),
        ]));
        let options = options();
        let preparation = prepare_book_from_source(source, &options).unwrap();
        let (assets, cover) = source_assets(&preparation).unwrap();
        let build = preparation.finish(assets, cover, &options).unwrap();

        assert_eq!(build.images.len(), 1);
        assert_eq!(build.images[0].epub_path, "image/0001.png");
        assert_eq!(build.image("image/0001.png").unwrap().source, "挿絵/i1.png");
        let resolved = build.resolve("image/0001.png").unwrap();
        assert_eq!(
            aozora_epub3_lite::image::dimensions(&resolved, "image/png"),
            Some((32, 32))
        );
        assert!(build.book.sections.iter().any(|section| {
            section.body_fragment.contains("../image/0001.png")
        }));
        assert!(build.book.sections.iter().all(|section| {
            !section.body_fragment.contains("../image/挿絵/i1.png")
        }));

        let mut bytes = Vec::new();
        stream_epub(&build.book, &mut bytes, |path| build.resolve(path)).unwrap();
        let needle = b"item/image/0001.png";
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
        let png = test_png(32, 32);
        let text = format!("{SAMPLE}\n　挿絵。［＃挿絵（挿絵/i1.png）入る］\n");
        let (_directory, path) = text_file(&text, Some(&png));
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
    }

    /// Worker と同じように、寸法だけを計画へ渡し、画像を直接処理して書き出す。
    #[test]
    fn text_source_streams_images_without_storing_their_bytes() {
        let png = test_png(32, 32);
        let text = format!(
            "{SAMPLE}\n　挿絵。［＃挿絵（挿絵/i1.png）入る］\n［＃挿絵（挿絵/i2.png）入る］\n"
        );
        let names = vec!["挿絵/i1.png".to_string(), "挿絵/i2.png".to_string()];
        let source = Arc::new(EpubTextSource::new(text, names.clone()));
        let options = options();
        let preparation = prepare_book_from_source(source.clone(), &options).unwrap();
        let mut plan = preparation.asset_plan().unwrap();
        let mut requested = 0;
        while let Some(request) = plan.next_request().unwrap() {
            assert!(names.iter().any(|name| name == request.source));
            assert!(request.filesystem_path.is_none());
            let dimensions = image_dimensions(&png, request.media_type);
            plan.provide_dimensions(dimensions).unwrap();
            requested += 1;
        }
        assert_eq!(requested, names.len());
        let (assets, cover) = plan.complete().unwrap();
        let build = preparation.finish(assets, cover, &options).unwrap();
        assert_eq!(build.images.len(), 2);
        for name in &names {
            assert!(source.open(name).unwrap().is_none());
        }
        assert!(build.resolve("image/0001.png").is_none());

        let sink = ChunkSink::new();
        let mut writer = build.book.stream_writer(sink.clone()).unwrap();
        let mut streamed = Vec::new();
        while let Some(info) = writer.next_entry() {
            let bytes = info.asset_path.as_deref().map(|path| {
                let image = build.image(path).unwrap();
                assert!(names.iter().any(|name| name == &image.source));
                build.process_image(path, test_png(32, 32)).unwrap()
            });
            writer.write_current(bytes.as_deref()).unwrap();
            streamed.extend(sink.take());
        }
        writer.finish().unwrap();
        streamed.extend(sink.take());

        assert!(streamed.starts_with(b"PK\x03\x04"));
        for path in ["image/0001.png", "image/0002.png"] {
            let needle = format!("item/{path}");
            assert!(streamed.windows(needle.len()).any(|window| window == needle.as_bytes()));
        }
    }

    #[test]
    fn source_metadata_requires_the_listed_image_to_exist() {
        let text = format!("{SAMPLE}\n［＃挿絵（挿絵/i1.png）入る］\n");
        let source = Arc::new(EpubTextSource::new(text, vec!["挿絵/i1.png".to_string()]));
        let preparation = prepare_book_from_source(source, &options()).unwrap();
        let error = source_assets(&preparation).err().unwrap();
        assert!(error.to_string().contains("image entry not found: 挿絵/i1.png"));
    }

    #[test]
    fn image_processing_errors_do_not_return_original_bytes() {
        let text = format!("{SAMPLE}\n［＃挿絵（挿絵/i1.png）入る］\n");
        let (_directory, path) = text_file(&text, Some(&test_png(800, 400)));
        let options = EpubBuildOptions {
            rotate_image: Some(ImageRotation::Ccw90),
            ..options()
        };
        let build = build_book(&path, &options).unwrap();
        assert!(build.process_image("image/0001.png", b"not a PNG".to_vec()).is_err());
        assert!(build.process_image("image/9999.png", Vec::new()).is_err());
        std::fs::write(path.parent().unwrap().join("挿絵/i1.png"), b"not a PNG").unwrap();
        assert!(build.resolve("image/0001.png").is_none());
        assert!(stream_epub(&build.book, Vec::new(), |path| build.resolve(path)).is_err());
    }

    /// 実寸のある PNG (8bit グレースケール) を組み立てる。`image` crate への
    /// 依存を増やさないよう、CRC と IDAT (stored deflate) を手で組む。
    fn test_png(width: u32, height: u32) -> Vec<u8> {
        fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut out = (payload.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(payload);
            let mut crc_data = kind.to_vec();
            crc_data.extend_from_slice(payload);
            out.extend_from_slice(&crc32fast::hash(&crc_data).to_be_bytes());
            out
        }
        let mut ihdr = width.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        // 各行はフィルタバイト 0 + グレー 1 バイト/px。デコードできる PNG に
        // するため、先頭のフィルタバイトは 0 のまま残す。
        let row_bytes = 1 + width as usize;
        let mut raw = vec![0u8; row_bytes * height as usize];
        for (index, byte) in raw.iter_mut().enumerate() {
            if index % row_bytes != 0 {
                *byte = (index % 251) as u8;
            }
        }
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 1);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(chunk(b"IHDR", &ihdr));
        png.extend(chunk(b"IDAT", &compressed));
        png.extend(chunk(b"IEND", &[]));
        png
    }

    /// 単ページ画像へ、回転の無効・左右それぞれの指定が同じ処理で反映される。
    #[test]
    fn rotate_image_setting_overrides_ini_for_page_images() {
        let wide = test_png(800, 400);
        let text = format!("{SAMPLE}\n［＃挿絵（挿絵/i1.png）入る］\n");
        for (rotation, angle, dimensions) in [
            (ImageRotation::Off, 0, (800, 400)),
            (ImageRotation::Cw90, 90, (400, 800)),
            (ImageRotation::Ccw90, -90, (400, 800)),
        ] {
            let (_directory, path) = text_file(&text, Some(&wide));
            let build = build_book(
                &path,
                &EpubBuildOptions {
                    rotate_image: Some(rotation),
                    ..options()
                },
            )
            .unwrap();
            assert_eq!(build.images[0].rotate, angle);
            let resolved = build.resolve("image/0001.png").unwrap();
            assert_eq!(
                aozora_epub3_lite::image::dimensions(&resolved, "image/png"),
                Some(dimensions)
            );
        }
    }

    /// `aozoraepub3dir` の INI に `RotateImage` があれば、未指定時はそちらが
    /// 優先される (ユーザーが本家ツール側で回転を切っている場合)。
    #[test]
    fn rotate_image_unset_follows_install_ini() {
        let assets = tempfile::tempdir().unwrap();
        let dir = assets.path();
        std::fs::write(dir.join("AozoraEpub3.ini"), "RotateImage=1\n").unwrap();
        let wide = test_png(800, 400);
        let text = format!("{SAMPLE}\n［＃挿絵（挿絵/i1.png）入る］\n");
        let (_directory, path) = text_file(&text, Some(&wide));
        let build = build_book(
            &path,
            &EpubBuildOptions {
                assets_dir: Some(dir.to_path_buf()),
                ..options()
            },
        )
        .unwrap();
        assert_eq!(build.images[0].rotate, 90);
    }

    /// 寸法の取得方法に依らず、連番・表紙・再フロー・扉・画像処理が一致する。
    #[test]
    fn source_and_file_inputs_share_image_layout_and_processing() {
        let text = format!(
            "{SAMPLE}\n［＃挿絵（挿絵/i2.png）入る］\n\
             ［＃挿絵（挿絵/missing.png）入る］\n［＃挿絵（挿絵/i1.png）入る］\n\
             ［＃挿絵（挿絵/i1.png）入る］\n［＃大見出し］第２章［＃大見出し終わり］\n　続き。\n"
        );
        for cover_from_first_image in [false, true] {
            let wide = test_png(800, 400);
            let icon = test_png(32, 32);
            let (_directory, path) = text_file(&text, Some(&wide));
            std::fs::write(path.parent().unwrap().join("挿絵/i2.png"), &icon).unwrap();
            let source = Arc::new(Memory::new(vec![
                ("novel.txt".to_string(), text.as_bytes().to_vec()),
                ("挿絵/i1.png".to_string(), wide),
                ("挿絵/i2.png".to_string(), icon),
            ]));
            let options = EpubBuildOptions {
                cover_from_first_image,
                vertical: !cover_from_first_image,
                rotate_image: Some(ImageRotation::Ccw90),
                extra_assets: vec![EpubAsset::new(
                    "style/extra.css",
                    "text/css",
                    b"p { color: black; }".to_vec(),
                )],
                ..options()
            };
            let native = build_book(&path, &options).unwrap();
            let preparation = prepare_book_from_source(source, &options).unwrap();
            let (assets, cover) = source_assets(&preparation).unwrap();
            let mut portable = preparation.finish(assets, cover, &options).unwrap();
            // 書誌の生成時刻だけは、別々に実行した入力とは比較しない。
            portable.book.metadata.modified = native.book.metadata.modified.clone();
            assert_eq!(portable.book, native.book);
            assert_eq!(portable.images.len(), 2);
            assert_eq!(portable.images[0].epub_path, "image/0001.png");
            assert_eq!(portable.images[1].epub_path, "image/0003.png");
            if cover_from_first_image {
                assert_eq!(portable.book.cover_asset.as_deref(), Some("image/0003.png"));
                assert_eq!(portable.book.cover_dimensions, Some((800, 400)));
            } else {
                assert!(portable.book.sections.len() > 1);
                assert_eq!(portable.image("image/0003.png").unwrap().rotate, -90);
            }
            assert!(portable.book.sections.iter().all(|section| {
                !section.body_fragment.contains("missing.png")
                    && (!cover_from_first_image || !section.body_fragment.contains("0003.png"))
            }));
            for image in &portable.images {
                let native_image = native.image(&image.epub_path).unwrap();
                assert_eq!(image.source, native_image.source);
                assert_eq!(image.is_cover, native_image.is_cover);
                assert_eq!(image.rotate, native_image.rotate);
                assert_eq!(portable.resolve(&image.epub_path), native.resolve(&image.epub_path));
            }
        }
    }

    #[test]
    fn no_illust_filter_avoids_image_metadata_in_both_inputs() {
        let assets = tempfile::tempdir().unwrap();
        std::fs::write(assets.path().join("AozoraEpub3.ini"), "NoIllust=1\n").unwrap();
        let text = format!("{SAMPLE}\n［＃挿絵（挿絵/i1.png）入る］\n");
        let (_directory, path) = text_file(&text, Some(&test_png(800, 400)));
        let options = EpubBuildOptions {
            assets_dir: Some(assets.path().to_path_buf()),
            cover_from_first_image: true,
            ..options()
        };
        let native = build_book(&path, &options).unwrap();
        let source = Arc::new(EpubTextSource::new(text, vec!["挿絵/i1.png".to_string()]));
        let preparation = prepare_book_from_source(source, &options).unwrap();
        let (collected, cover) = source_assets(&preparation).unwrap();
        let mut portable = preparation.finish(collected, cover, &options).unwrap();
        portable.book.metadata.modified = native.book.metadata.modified.clone();
        assert_eq!(portable.book, native.book);
        assert!(portable.images.is_empty());
        assert!(native.images.is_empty());
        assert!(portable.book.cover_asset.is_none());
    }

}

