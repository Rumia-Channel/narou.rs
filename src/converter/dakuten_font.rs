//! narou のカスタム CSS (行高・二分アキ・前書き・後書きなど) と
//! 濁点フォント (DMincho.ttf) の有効化/無効化。
//!
//! Ruby 版 `Narou.activate_dakuten_font_files` /
//! `Narou.inactivate_dakuten_font_files` (lib/novelconverter.rb) と等価。
//! AozoraEpub3 が EPUB を組み立てる直前に preset の濁点用 CSS と DMincho.ttf
//! を AozoraEpub3 ディレクトリへ流し込み、終了後に通常 CSS へ戻して .ttf を削除する。
//!
//! AozoraEpub3 はテンプレートのレイアウトによってカスタム CSS の配置先が
//! 変わる。1.1.1 以降の改造版 (電書協レイアウト、`template/item/style/`) では
//! `template/item/style_custom/font.css`、それ以前 (元祖レイアウト、
//! `template/OPS/css/`) では `template/OPS/css_custom/vertical_font.css` に置く。
//! どちらのレイアウトかを検出して書き込む (narou.rb 側の既知不具合
//! whiteleaf7/narou#419 に相当する問題を避ける)。

use std::path::{Path, PathBuf};

use crate::compat::load_global_setting_value;
use crate::error::{NarouError, Result};

use super::settings::{format_line_height, line_height_from_value};

const LEGACY_NORMAL_CSS_NAME: &str = "vertical_font.css";
const LEGACY_DAKUTEN_CSS_NAME: &str = "vertical_font_with_dakuten.css";
const DENSHOKYO_NORMAL_CSS_NAME: &str = "denshokyo_font.css";
const DENSHOKYO_DAKUTEN_CSS_NAME: &str = "denshokyo_font_with_dakuten.css";
const DAKUTEN_FONT_NAME: &str = "DMincho.ttf";

/// AozoraEpub3 のテンプレート配置。カスタム CSS の書き込み先が変わる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AozoraTemplateKind {
    /// 元祖 AozoraEpub3 (`template/OPS/css/vertical_font.css` がある)。
    Legacy,
    /// 1.1.1 以降の改造版 / 電書協レイアウト (`template/item/style/font.css` がある)。
    Denshokyo,
}

impl AozoraTemplateKind {
    fn normal_css_name(self) -> &'static str {
        match self {
            Self::Legacy => LEGACY_NORMAL_CSS_NAME,
            Self::Denshokyo => DENSHOKYO_NORMAL_CSS_NAME,
        }
    }

    fn dakuten_css_name(self) -> &'static str {
        match self {
            Self::Legacy => LEGACY_DAKUTEN_CSS_NAME,
            Self::Denshokyo => DENSHOKYO_DAKUTEN_CSS_NAME,
        }
    }

    fn css_relative_path(self) -> PathBuf {
        match self {
            Self::Legacy => PathBuf::from("template/OPS/css_custom").join(LEGACY_NORMAL_CSS_NAME),
            Self::Denshokyo => PathBuf::from("template/item/style_custom").join("font.css"),
        }
    }

    fn font_relative_path(self) -> PathBuf {
        match self {
            Self::Legacy => PathBuf::from("template/OPS/fonts").join(DAKUTEN_FONT_NAME),
            Self::Denshokyo => PathBuf::from("template/item/fonts").join(DAKUTEN_FONT_NAME),
        }
    }
}

/// AozoraEpub3 ディレクトリのテンプレート配置を判定する。
///
/// narou.rb (`AozoraEpub3.aozoraepub3_type`) と同じ順で判定し、どちらにも
/// 当てはまらない場合は `None` を返す (Lite の単体 exe など、テンプレートを
/// 外に持たない実行ファイルのディレクトリへは書き込まない)。
pub fn detect_template_kind(aozora_dir: &Path) -> Option<AozoraTemplateKind> {
    if aozora_dir
        .join("template/OPS/css/vertical_font.css")
        .is_file()
    {
        return Some(AozoraTemplateKind::Legacy);
    }
    if aozora_dir.join("template/item/style/font.css").is_file() {
        return Some(AozoraTemplateKind::Denshokyo);
    }
    None
}

fn preset_dir() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        candidates.push(parent.join("preset"));
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(manifest_dir.join("preset"));
    candidates.push(manifest_dir.join("sample").join("narou").join("preset"));

    candidates
        .into_iter()
        .find(|path| path.is_dir())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "narou preset directory not found",
            )
            .into()
        })
}

/// 組み込みエンジンへ渡す narou カスタム CSS (+ 必要なら濁点フォント) のアセット一式。
///
/// `lite` feature (組み込み EPUB エンジン) 専用。パス・media type・`always` の
/// 付加 CSS は [`crate::epub_lite::narou_font_assets_from`] が唯一の定義で、
/// ここはバイト源だけを preset の実体にする (preset の更新がすぐ効く)。
///
/// 組み込みエンジンの EPUB は電書協レイアウトなので、AozoraEpub3 の
/// `template/item/style_custom/font.css` に相当する CSS
/// (denshokyo_font.css) を **常に** 同梱する。二分アキ (`.half_em_space`) や
/// 行高・前書き・後書きの装飾はこの CSS が無いと一切効かない (issue #42)。
/// 濁点注記がある場合、または `convert.epub-font = always` のときだけ
/// 濁点用 CSS と DMincho.ttf を追加する。
#[cfg(feature = "lite")]
pub fn lite_font_assets(use_dakuten_font: bool) -> Result<Vec<aozora_epub3_lite::EpubAsset>> {
    let always = crate::epub_lite::epub_font_always(
        crate::compat::load_global_setting_string("convert.epub-font").as_deref(),
    );
    let needs_font = crate::epub_lite::needs_dakuten_font(use_dakuten_font, always);
    let preset = preset_dir()?;
    let css_name = if needs_font {
        DENSHOKYO_DAKUTEN_CSS_NAME
    } else {
        DENSHOKYO_NORMAL_CSS_NAME
    };
    let css_template = std::fs::read_to_string(preset.join(css_name))?;
    let font = if needs_font {
        Some(std::fs::read(preset.join(DAKUTEN_FONT_NAME))?)
    } else {
        None
    };
    Ok(crate::epub_lite::narou_font_assets_from(
        &css_template,
        font.as_deref(),
        always,
        current_line_height(),
    ))
}

fn dst_css(aozora_dir: &Path, kind: AozoraTemplateKind) -> PathBuf {
    aozora_dir.join(kind.css_relative_path())
}

fn dst_font(aozora_dir: &Path, kind: AozoraTemplateKind) -> PathBuf {
    aozora_dir.join(kind.font_relative_path())
}

fn write_normal_css(aozora_dir: &Path, kind: AozoraTemplateKind, line_height: f64) -> Result<()> {
    let preset = preset_dir()?;
    let src_normal_css = preset.join(kind.normal_css_name());
    let dst_css_path = dst_css(aozora_dir, kind);
    if let Some(parent) = dst_css_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let css_text = std::fs::read_to_string(&src_normal_css)?
        .replace("<%= line_height %>", &format_line_height(line_height));
    std::fs::write(&dst_css_path, css_text)?;
    Ok(())
}

/// 通常 CSS を AozoraEpub3 ディレクトリへ書き込む (init 用)。
///
/// 書き込み先はテンプレート配置で変わる。`kind` を呼び出し側が決めることで、
/// テンプレートがまだ無い初期化直後でも narou.rb と同じ既定 (元祖) に倒せる。
pub fn install_normal_css(
    aozora_dir: &Path,
    kind: AozoraTemplateKind,
    line_height: f64,
) -> Result<()> {
    write_normal_css(aozora_dir, kind, line_height)
}

/// テンプレート配置が判定でき、カスタム CSS がまだ無い場合だけ書き込む。
///
/// 古いバージョンの narou_rs は電書協レイアウトでも `template/OPS/css_custom/`
/// へ書いていたため、1.1.1 以降の AozoraEpub3 ではカスタム CSS が EPUB に
/// 入らず、二分アキ・行高が効かなかった。初回変換時に正しい配置へ補う。
/// 既にある場合は触らない (narou.rb も init / 濁点切替でしか書き換えない。
/// 並列変換中に他のスレッドの CSS を上書きしないため)。
pub fn ensure_normal_css(aozora_dir: &Path) -> Result<bool> {
    let Some(kind) = detect_template_kind(aozora_dir) else {
        return Ok(false);
    };
    if dst_css(aozora_dir, kind).is_file() {
        return Ok(false);
    }
    write_normal_css(aozora_dir, kind, current_line_height())?;
    Ok(true)
}

fn restore_normal_css(aozora_dir: &Path, kind: AozoraTemplateKind) -> Result<()> {
    write_normal_css(aozora_dir, kind, current_line_height())
}

fn remove_dakuten_font(aozora_dir: &Path, kind: AozoraTemplateKind) -> Result<()> {
    let dst_font_path = dst_font(aozora_dir, kind);
    if dst_font_path.exists() {
        std::fs::remove_file(dst_font_path)?;
    }
    Ok(())
}

fn cleanup_errors(
    aozora_dir: &Path,
    kind: AozoraTemplateKind,
) -> (Option<NarouError>, Option<NarouError>) {
    (
        restore_normal_css(aozora_dir, kind).err(),
        remove_dakuten_font(aozora_dir, kind).err(),
    )
}

fn warn_cleanup_errors(
    aozora_dir: &Path,
    kind: AozoraTemplateKind,
    css_error: Option<&NarouError>,
    font_error: Option<&NarouError>,
) {
    if let Some(err) = css_error {
        tracing::warn!(
            path = %dst_css(aozora_dir, kind).display(),
            error = %err,
            "failed to restore dakuten font CSS"
        );
    }
    if let Some(err) = font_error {
        tracing::warn!(
            path = %dst_font(aozora_dir, kind).display(),
            error = %err,
            "failed to remove dakuten font file"
        );
    }
}

fn combine_cleanup_errors(
    css_error: Option<NarouError>,
    font_error: Option<NarouError>,
) -> Result<()> {
    match (css_error, font_error) {
        (None, None) => Ok(()),
        (Some(err), None) | (None, Some(err)) => Err(err),
        (Some(css_err), Some(font_err)) => Err(std::io::Error::other(format!(
            "dakuten font cleanup failed: css restore error: {}; font removal error: {}",
            css_err, font_err
        ))
        .into()),
    }
}

fn current_line_height() -> f64 {
    line_height_from_value(load_global_setting_value("line-height").as_ref())
}

/// preset の濁点用 CSS と DMincho.ttf を AozoraEpub3 配下へ書き込む。
pub fn activate(aozora_dir: &Path) -> Result<()> {
    let kind = detect_template_kind(aozora_dir).unwrap_or(AozoraTemplateKind::Legacy);
    let preset = preset_dir()?;
    let src_css = preset.join(kind.dakuten_css_name());
    let src_font = preset.join(DAKUTEN_FONT_NAME);

    let dst_css_path = dst_css(aozora_dir, kind);
    let dst_font_path = dst_font(aozora_dir, kind);

    if let Some(parent) = dst_css_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = dst_font_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let css_text = std::fs::read_to_string(&src_css)?.replace(
        "<%= line_height %>",
        &format_line_height(current_line_height()),
    );
    std::fs::write(&dst_css_path, css_text)?;
    std::fs::copy(&src_font, &dst_font_path)?;
    Ok(())
}

/// 通常のカスタム CSS を再書き込みし、DMincho.ttf を削除する。
pub fn inactivate(aozora_dir: &Path) -> Result<()> {
    let kind = detect_template_kind(aozora_dir).unwrap_or(AozoraTemplateKind::Legacy);
    let (css_error, font_error) = cleanup_errors(aozora_dir, kind);
    warn_cleanup_errors(aozora_dir, kind, css_error.as_ref(), font_error.as_ref());
    combine_cleanup_errors(css_error, font_error)
}

/// activate を呼び出し、Drop で必ず inactivate する RAII ガード。
pub struct DakutenFontGuard {
    aozora_dir: PathBuf,
    armed: bool,
}

impl DakutenFontGuard {
    pub fn activate(aozora_dir: &Path) -> Result<Self> {
        activate(aozora_dir)?;
        Ok(Self {
            aozora_dir: aozora_dir.to_path_buf(),
            armed: true,
        })
    }
}

impl Drop for DakutenFontGuard {
    fn drop(&mut self) {
        if self.armed {
            let kind = detect_template_kind(&self.aozora_dir).unwrap_or(AozoraTemplateKind::Legacy);
            let (css_error, font_error) = cleanup_errors(&self.aozora_dir, kind);
            warn_cleanup_errors(
                &self.aozora_dir,
                kind,
                css_error.as_ref(),
                font_error.as_ref(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{self, Write};
    use std::panic;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct SharedWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_warnings<F: FnOnce()>(f: F) -> String {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = SharedWriter {
            buffer: buffer.clone(),
        };
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn activate_writes_dakuten_css_and_font() {
        let tmp = tempfile::tempdir().unwrap();
        let aozora_dir = tmp.path();
        activate(aozora_dir).unwrap();
        let css_path = dst_css(aozora_dir, AozoraTemplateKind::Legacy);
        let font_path = dst_font(aozora_dir, AozoraTemplateKind::Legacy);
        assert!(css_path.exists(), "vertical_font.css should be written");
        assert!(font_path.exists(), "DMincho.ttf should be copied");
        let css_text = fs::read_to_string(&css_path).unwrap();
        assert!(
            css_text.contains("DMincho"),
            "dakuten CSS should reference DMincho font"
        );
    }

    #[test]
    fn inactivate_restores_normal_css_and_removes_font() {
        let tmp = tempfile::tempdir().unwrap();
        let aozora_dir = tmp.path();
        activate(aozora_dir).unwrap();
        inactivate(aozora_dir).unwrap();
        let css_path = dst_css(aozora_dir, AozoraTemplateKind::Legacy);
        let font_path = dst_font(aozora_dir, AozoraTemplateKind::Legacy);
        assert!(css_path.exists());
        assert!(!font_path.exists(), "DMincho.ttf must be removed");
        let css_text = fs::read_to_string(&css_path).unwrap();
        assert!(
            !css_text.contains("DMincho"),
            "normal vertical_font.css must not reference DMincho"
        );
    }

    #[test]
    fn guard_inactivates_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let aozora_dir = tmp.path();
        {
            let _g = DakutenFontGuard::activate(aozora_dir).unwrap();
            assert!(dst_font(aozora_dir, AozoraTemplateKind::Legacy).exists());
        }
        assert!(!dst_font(aozora_dir, AozoraTemplateKind::Legacy).exists());
    }

    #[test]
    fn inactivate_warns_for_css_and_font_cleanup_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let aozora_dir = tmp.path();
        activate(aozora_dir).unwrap();

        let css_dir = dst_css(aozora_dir, AozoraTemplateKind::Legacy)
            .parent()
            .unwrap()
            .to_path_buf();
        fs::remove_dir_all(&css_dir).unwrap();
        fs::write(&css_dir, "not a directory").unwrap();

        let font_path = dst_font(aozora_dir, AozoraTemplateKind::Legacy);
        fs::remove_file(&font_path).unwrap();
        fs::create_dir(&font_path).unwrap();

        let output = capture_warnings(|| {
            let err = inactivate(aozora_dir).unwrap_err();
            let message = err.to_string();
            assert!(message.contains("css restore error"));
            assert!(message.contains("font removal error"));
        });

        assert!(output.contains("failed to restore dakuten font CSS"));
        assert!(output.contains("failed to remove dakuten font file"));
    }

    #[test]
    fn drop_warns_on_cleanup_failures_without_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let aozora_dir = tmp.path().to_path_buf();

        let output = capture_warnings(|| {
            let result = panic::catch_unwind(|| {
                let _guard = DakutenFontGuard::activate(&aozora_dir).unwrap();

                let css_dir = dst_css(&aozora_dir, AozoraTemplateKind::Legacy)
                    .parent()
                    .unwrap()
                    .to_path_buf();
                fs::remove_dir_all(&css_dir).unwrap();
                fs::write(&css_dir, "not a directory").unwrap();

                let font_path = dst_font(&aozora_dir, AozoraTemplateKind::Legacy);
                fs::remove_file(&font_path).unwrap();
                fs::create_dir(&font_path).unwrap();
            });
            assert!(result.is_ok(), "Drop must not panic on cleanup failure");
        });

        assert!(output.contains("failed to restore dakuten font CSS"));
        assert!(output.contains("failed to remove dakuten font file"));
    }

    /// 電書協レイアウト (`template/item/style/font.css`) を検出し、
    /// カスタム CSS を `template/item/style_custom/font.css` へ書く。
    #[test]
    fn denshokyo_layout_uses_style_custom_font_css() {
        let tmp = tempfile::tempdir().unwrap();
        let aozora_dir = tmp.path();
        let marker = aozora_dir.join("template/item/style/font.css");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, "base\n").unwrap();

        assert_eq!(
            detect_template_kind(aozora_dir),
            Some(AozoraTemplateKind::Denshokyo)
        );
        activate(aozora_dir).unwrap();

        let css =
            fs::read_to_string(aozora_dir.join("template/item/style_custom/font.css")).unwrap();
        assert!(
            css.contains("DMincho"),
            "濁点 CSS が DMincho を参照していない"
        );
        assert!(
            css.contains(".vrtl .half_em_space"),
            "二分アキの CSS が無い"
        );
        assert!(aozora_dir.join("template/item/fonts/DMincho.ttf").exists());
        assert!(
            !aozora_dir
                .join("template/OPS/css_custom/vertical_font.css")
                .exists(),
            "旧 OPS パスへ書いてしまっている"
        );

        inactivate(aozora_dir).unwrap();
        let css =
            fs::read_to_string(aozora_dir.join("template/item/style_custom/font.css")).unwrap();
        assert!(!css.contains("DMincho"));
        assert!(css.contains(".vrtl .half_em_space"));
        assert!(!aozora_dir.join("template/item/fonts/DMincho.ttf").exists());
    }

    /// 元祖レイアウトが両方のマーカーを持つ場合は元祖を優先する (narou.rb と同じ)。
    #[test]
    fn legacy_layout_wins_when_both_markers_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let aozora_dir = tmp.path();
        for relative in [
            "template/OPS/css/vertical_font.css",
            "template/item/style/font.css",
        ] {
            let path = aozora_dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "base\n").unwrap();
        }
        assert_eq!(
            detect_template_kind(aozora_dir),
            Some(AozoraTemplateKind::Legacy)
        );
    }

    #[test]
    fn ensure_normal_css_skips_unknown_layouts_and_writes_known_ones() {
        let unknown = tempfile::tempdir().unwrap();
        assert!(!ensure_normal_css(unknown.path()).unwrap());

        let known = tempfile::tempdir().unwrap();
        let marker = known.path().join("template/item/style/font.css");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, "base\n").unwrap();
        assert!(ensure_normal_css(known.path()).unwrap());
        let css =
            fs::read_to_string(known.path().join("template/item/style_custom/font.css")).unwrap();
        assert!(css.contains(".vrtl .half_em_space"));
        assert!(!css.contains("DMincho"));
        assert!(
            !ensure_normal_css(known.path()).unwrap(),
            "既存のカスタム CSS を毎回書き直してはいけない"
        );
    }
}
