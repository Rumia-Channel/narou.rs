//! 濁点フォント (DMincho.ttf + vertical_font_with_dakuten.css) の有効化/無効化。
//!
//! Ruby 版 `Narou.activate_dakuten_font_files` /
//! `Narou.inactivate_dakuten_font_files` (lib/novelconverter.rb) と等価。
//! AozoraEpub3 が EPUB を組み立てる直前に preset の濁点用 CSS と DMincho.ttf
//! を AozoraEpub3 ディレクトリへ流し込み、終了後に通常 CSS へ戻して .ttf を削除する。

use std::path::{Path, PathBuf};

use crate::compat::load_global_setting_value;
use crate::error::Result;

const DAKUTEN_CSS_NAME: &str = "vertical_font_with_dakuten.css";
const NORMAL_CSS_NAME: &str = "vertical_font.css";
const DAKUTEN_FONT_NAME: &str = "DMincho.ttf";

fn preset_dir() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("preset"));
        }
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

fn format_line_height(line_height: f64) -> String {
    let mut text = line_height.to_string();
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.push('0');
        }
    }
    text
}

fn dst_css(aozora_dir: &Path) -> PathBuf {
    aozora_dir
        .join("template")
        .join("OPS")
        .join("css_custom")
        .join(NORMAL_CSS_NAME)
}

fn dst_font(aozora_dir: &Path) -> PathBuf {
    aozora_dir
        .join("template")
        .join("OPS")
        .join("fonts")
        .join(DAKUTEN_FONT_NAME)
}

fn current_line_height() -> f64 {
    load_global_setting_value("line-height")
        .and_then(|v| match v {
            serde_yaml::Value::Number(n) => n.as_f64().or_else(|| n.as_i64().map(|i| i as f64)),
            serde_yaml::Value::String(s) => s.parse::<f64>().ok(),
            _ => None,
        })
        .unwrap_or(1.8)
}

/// preset の濁点用 CSS と DMincho.ttf を AozoraEpub3 配下へ書き込む。
pub fn activate(aozora_dir: &Path) -> Result<()> {
    let preset = preset_dir()?;
    let src_css = preset.join(DAKUTEN_CSS_NAME);
    let src_font = preset.join(DAKUTEN_FONT_NAME);

    let dst_css_path = dst_css(aozora_dir);
    let dst_font_path = dst_font(aozora_dir);

    if let Some(parent) = dst_css_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = dst_font_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let css_text = std::fs::read_to_string(&src_css)?
        .replace("<%= line_height %>", &format_line_height(current_line_height()));
    std::fs::write(&dst_css_path, css_text)?;
    std::fs::copy(&src_font, &dst_font_path)?;
    Ok(())
}

/// 通常 vertical_font.css を再書き込みし、DMincho.ttf を削除する。
pub fn inactivate(aozora_dir: &Path) -> Result<()> {
    let preset = preset_dir()?;
    let src_normal_css = preset.join(NORMAL_CSS_NAME);

    let dst_css_path = dst_css(aozora_dir);
    if let Some(parent) = dst_css_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let css_text = std::fs::read_to_string(&src_normal_css)?
        .replace("<%= line_height %>", &format_line_height(current_line_height()));
    std::fs::write(&dst_css_path, css_text)?;

    let dst_font_path = dst_font(aozora_dir);
    if dst_font_path.exists() {
        let _ = std::fs::remove_file(&dst_font_path);
    }
    Ok(())
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
            let _ = inactivate(&self.aozora_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn activate_writes_dakuten_css_and_font() {
        let tmp = tempfile::tempdir().unwrap();
        let aozora_dir = tmp.path();
        activate(aozora_dir).unwrap();
        let css_path = dst_css(aozora_dir);
        let font_path = dst_font(aozora_dir);
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
        let css_path = dst_css(aozora_dir);
        let font_path = dst_font(aozora_dir);
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
            assert!(dst_font(aozora_dir).exists());
        }
        assert!(!dst_font(aozora_dir).exists());
    }
}
