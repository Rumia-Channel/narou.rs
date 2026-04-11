pub mod converter_base;
pub mod device;
pub mod settings;
pub mod user_converter;

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use settings::NovelSettings;
use user_converter::UserConverter;

use crate::downloader::{SectionElement, SectionFile, TocObject};
use crate::error::{NarouError, Result};

pub struct NovelConverter {
    settings: NovelSettings,
    user_converter: Option<UserConverter>,
    section_cache: HashMap<String, CacheEntry>,
    cache_dirty: bool,
}

struct CacheEntry {
    digest: String,
    converted_section: ConvertedSection,
}

#[derive(Clone)]
struct ConvertedSection {
    chapter: String,
    subchapter: String,
    subtitle: String,
    introduction: String,
    body: String,
    postscript: String,
}

impl NovelConverter {
    pub fn new(settings: NovelSettings) -> Self {
        Self {
            settings,
            user_converter: None,
            section_cache: HashMap::new(),
            cache_dirty: false,
        }
    }

    pub fn with_user_converter(settings: NovelSettings, user_converter: UserConverter) -> Self {
        Self {
            settings,
            user_converter: Some(user_converter),
            section_cache: HashMap::new(),
            cache_dirty: false,
        }
    }

    pub fn convert_novel(&mut self, toc: &TocObject, sections: &[SectionFile]) -> Result<String> {
        let mut converted_story = String::new();
        if let Some(ref story) = toc.story {
            if !story.is_empty() {
                let mut converter = self.make_converter();
                let story_text = normalize_story_source(story);
                converted_story = converter.convert(&story_text, converter_base::TextType::Story);
            }
        }

        let mut converted_sections = Vec::new();

        for (i, section) in sections.iter().enumerate() {
            let digest = self.compute_digest(&section.element, i);

            if let Some(cached) = self.section_cache.get(&digest) {
                converted_sections.push(cached.converted_section.clone());
                continue;
            }

            let mut converter = self.make_converter();

            let chapter = section.chapter.clone();
            let subchapter = section.subchapter.clone();
            let subtitle = section.subtitle.clone();

            let is_html =
                section.element.data_type != "text" && section.element.data_type != "text/plain";

            let mut batch_inputs = Vec::new();

            if !chapter.is_empty() {
                batch_inputs.push((chapter.clone(), converter_base::TextType::Chapter));
            }
            if !subtitle.is_empty() {
                batch_inputs.push((subtitle.clone(), converter_base::TextType::Subtitle));
            }

            let intro_text = if is_html && !section.element.introduction.is_empty() {
                crate::downloader::html::to_aozora(&section.element.introduction)
            } else {
                section.element.introduction.clone()
            };
            let body_text = if is_html && !section.element.body.is_empty() {
                crate::downloader::html::to_aozora(&section.element.body)
            } else {
                section.element.body.clone()
            };
            let post_text = if is_html && !section.element.postscript.is_empty() {
                crate::downloader::html::to_aozora(&section.element.postscript)
            } else {
                section.element.postscript.clone()
            };

            if !intro_text.is_empty() {
                batch_inputs.push((intro_text, converter_base::TextType::Introduction));
            }
            batch_inputs.push((body_text, converter_base::TextType::Body));
            if !post_text.is_empty() {
                batch_inputs.push((post_text, converter_base::TextType::Postscript));
            }

            let results = converter.convert_multi(&batch_inputs);

            let mut ri = 0;
            let conv_chapter = if !chapter.is_empty() {
                let r = results[ri].clone();
                ri += 1;
                r
            } else {
                String::new()
            };
            let conv_subtitle = if !subtitle.is_empty() {
                let r = results[ri].clone();
                ri += 1;
                r
            } else {
                String::new()
            };
            let conv_intro = if !section.element.introduction.is_empty() {
                let r = results[ri].clone();
                ri += 1;
                r
            } else {
                String::new()
            };
            let conv_body = results[ri].clone();
            ri += 1;
            let conv_post = if !section.element.postscript.is_empty() {
                let r = results[ri].clone();
                r
            } else {
                String::new()
            };

            let cs = ConvertedSection {
                chapter: conv_chapter,
                subchapter: subchapter.clone(),
                subtitle: conv_subtitle,
                introduction: conv_intro,
                body: conv_body,
                postscript: conv_post,
            };

            self.section_cache.insert(
                digest.clone(),
                CacheEntry {
                    digest,
                    converted_section: cs.clone(),
                },
            );
            self.cache_dirty = true;

            converted_sections.push(cs);
        }

        self.render_novel_text(toc, &converted_story, &converted_sections)
    }

    fn render_novel_text(
        &self,
        toc: &TocObject,
        story: &str,
        sections: &[ConvertedSection],
    ) -> Result<String> {
        let mut output = String::new();

        let title = if self.settings.novel_title.is_empty() {
            &toc.title
        } else {
            &self.settings.novel_title
        };
        let author = if self.settings.novel_author.is_empty() {
            &toc.author
        } else {
            &self.settings.novel_author
        };

        output.push_str(title);
        output.push('\n');
        output.push_str(author);
        output.push('\n');

        let cover_chuki = self.create_cover_chuki();
        output.push_str(&cover_chuki);
        output.push('\n');

        output.push_str("\u{FF3B}\u{FF03}\u{533A}\u{5207}\u{308A}\u{7DDA}\u{FF3D}\n");

        if !story.is_empty() {
            output.push_str("あらすじ：\n");
            output.push_str(story);
            if !story.ends_with('\n') {
                output.push('\n');
            }
            output.push('\n');
        }

        if !toc.toc_url.is_empty() {
            output.push_str("掲載ページ:\n");
            output.push_str(&format!(
                "<a href=\"{}\">{}</a>\n",
                toc.toc_url, toc.toc_url
            ));
            output.push_str("\u{FF3B}\u{FF03}\u{533A}\u{5207}\u{308A}\u{7DDA}\u{FF3D}\n");
        }

        output.push('\n');

        for section in sections {
            output.push_str("\u{FF3B}\u{FF03}\u{6539}\u{30DA}\u{30FC}\u{30B8}\u{FF3D}\n");

            if !section.chapter.is_empty() {
                output.push_str("\u{FF3B}\u{FF03}\u{30DA}\u{30FC}\u{30B8}\u{306E}\u{5DE6}\u{53F3}\u{4E2D}\u{592E}\u{FF3D}\n");
                output.push_str(&format!(
                    "\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{67F1}\u{FF3D}{}\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{67F1}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n",
                    title
                ));
                output.push_str(&format!(
                    "\u{FF3B}\u{FF03}\u{FF13}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}\u{FF3B}\u{FF03}\u{5927}\u{898B}\u{51FA}\u{3057}\u{FF3D}{}\u{FF3B}\u{FF03}\u{5927}\u{898B}\u{51FA}\u{3057}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n",
                    section.chapter
                ));
                output.push_str("\u{FF3B}\u{FF03}\u{6539}\u{30DA}\u{30FC}\u{30B8}\u{FF3D}\n");
            }

            if !section.subchapter.is_empty() {
                let trimmed_subchapter = section.subchapter.trim_end();
                output.push_str(&format!(
                    "\u{FF3B}\u{FF03}\u{FF11}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}\u{FF3B}\u{FF03}\u{FF11}\u{6BB5}\u{968E}\u{5927}\u{304D}\u{306A}\u{6587}\u{5B57}\u{FF3D}{}\u{FF3B}\u{FF03}\u{5927}\u{304D}\u{306A}\u{6587}\u{5B57}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n",
                    trimmed_subchapter
                ));
            }

            output.push('\n');

            let indent = if self.settings.enable_yokogaki {
                "\u{FF3B}\u{FF03}\u{FF11}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}"
            } else {
                "\u{FF3B}\u{FF03}\u{FF13}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}"
            };
            let trimmed_subtitle = normalize_subtitle_markup(section.subtitle.trim_end());
            output.push_str(&format!(
                "{}［＃中見出し］{}［＃中見出し終わり］\n",
                indent, trimmed_subtitle
            ));

            output.push_str("\n\n");

            let trimmed_intro = section.introduction.trim_end_matches('\n');
            let trimmed_body = section.body.trim_end_matches('\n');
            let trimmed_post = trim_author_comment_text(&section.postscript);

            if !section.introduction.is_empty() {
                let style = &self.settings.author_comment_style;
                if style == "simple" {
                    output.push_str("\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{FF18}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}\n");
                    output.push_str("\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{FF12}\u{6BB5}\u{968E}\u{5C0F}\u{3055}\u{306A}\u{6587}\u{5B57}\u{FF3D}\n");
                    output.push_str(trimmed_intro);
                    output.push_str("\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5C0F}\u{3055}\u{306A}\u{6587}\u{5B57}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n");
                    output.push_str("\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5B57}\u{4E0B}\u{3052}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n");
                } else if style == "plain" {
                    output.push_str("\n\n");
                    output.push_str(trimmed_intro);
                    output.push_str(
                        "\n\n\u{FF3B}\u{FF03}\u{533A}\u{5207}\u{308A}\u{7DDA}\u{FF3D}\n\n",
                    );
                } else {
                    output.push_str(&format!(
                        "\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{524D}\u{66F8}\u{304D}\u{FF3D}\n{}\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{524D}\u{66F8}\u{304D}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n",
                        trimmed_intro
                    ));
                }
            }

            if !section.introduction.is_empty() {
                output.push_str("\n\n");
            }

            output.push_str(trimmed_body);

            if !section.postscript.is_empty() {
                let style = &self.settings.author_comment_style;
                if style == "simple" {
                    output.push_str("\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{FF18}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}\n");
                    output.push_str("\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{FF12}\u{6BB5}\u{968E}\u{5C0F}\u{3055}\u{306A}\u{6587}\u{5B57}\u{FF3D}\n");
                    output.push_str(&trimmed_post);
                    output.push_str("\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5C0F}\u{3055}\u{306A}\u{6587}\u{5B57}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n");
                    output.push_str("\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5B57}\u{4E0B}\u{3052}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n");
                } else if style == "plain" {
                    output
                        .push_str("\n\u{FF3B}\u{FF03}\u{533A}\u{5207}\u{308A}\u{7DDA}\u{FF3D}\n\n");
                    output.push_str(&trimmed_post);
                    output.push_str("\n");
                } else {
                    output.push_str(&format!(
                        "\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{5F8C}\u{66F8}\u{304D}\u{FF3D}\n{}\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5F8C}\u{66F8}\u{304D}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n",
                        trimmed_post
                    ));
                }
            }

            if !output.ends_with('\n') {
                output.push('\n');
            }
        }

        if self.settings.enable_display_end_of_book {
            output.push_str("\n\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{304B}\u{3089}\u{5730}\u{4ED8}\u{304D}\u{FF3D}\u{FF3B}\u{FF03}\u{5C0F}\u{66F8}\u{304D}\u{FF3D}\u{FF08}\u{672C}\u{3092}\u{8AAD}\u{307F}\u{7D42}\u{308F}\u{308A}\u{307E}\u{3057}\u{305F}\u{FF09}\u{FF3B}\u{FF03}\u{5C0F}\u{66F8}\u{304D}\u{7D42}\u{308F}\u{308A}\u{FF3D}\u{FF3B}\u{FF03}\u{3053}\u{3053}\u{3067}\u{5730}\u{4ED8}\u{304D}\u{7D42}\u{308F}\u{308A}\u{FF3D}\n");
        }

        Ok(output)
    }

    fn create_cover_chuki(&self) -> String {
        let archive_path = &self.settings.archive_path;
        for ext in &[".jpg", ".png", ".jpeg"] {
            let cover_path = archive_path.join(format!("cover{}", ext));
            if cover_path.exists() {
                return format!(
                    "\u{FF3B}\u{FF03}\u{633F}\u{7D75}\u{FF08}cover{}\u{FF09}\u{5165}\u{308B}\u{FF3D}",
                    ext
                );
            }
        }
        String::new()
    }

    fn make_converter(&self) -> converter_base::ConverterBase {
        if let Some(ref uc) = self.user_converter {
            converter_base::ConverterBase::with_user_converter(self.settings.clone(), uc.clone())
        } else {
            converter_base::ConverterBase::new(self.settings.clone())
        }
    }

    fn compute_digest(&self, section: &SectionElement, index: usize) -> String {
        let mut hasher = Sha256::new();
        hasher.update(section.body.as_bytes());
        hasher.update(section.introduction.as_bytes());
        hasher.update(section.postscript.as_bytes());
        hasher.update(section.data_type.as_bytes());
        hasher.update(index.to_le_bytes());
        hasher.update(self.compute_settings_signature().as_bytes());
        if let Some(ref uc) = self.user_converter {
            hasher.update(uc.signature().as_bytes());
        }
        hex::encode(hasher.finalize())
    }

    fn compute_settings_signature(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.settings.enable_yokogaki.to_string().as_bytes());
        hasher.update(
            self.settings
                .enable_convert_num_to_kanji
                .to_string()
                .as_bytes(),
        );
        hasher.update(self.settings.enable_auto_indent.to_string().as_bytes());
        hasher.update(self.settings.enable_ruby.to_string().as_bytes());
        hasher.update(
            self.settings
                .enable_convert_horizontal_ellipsis
                .to_string()
                .as_bytes(),
        );
        hasher.update(self.settings.date_format.as_bytes());
        hasher.update(self.settings.enable_pack_blank_line.to_string().as_bytes());
        hasher.update(
            self.settings
                .enable_auto_join_in_brackets
                .to_string()
                .as_bytes(),
        );
        hasher.update(self.settings.enable_auto_join_line.to_string().as_bytes());
        hasher.update(
            self.settings
                .enable_half_indent_bracket
                .to_string()
                .as_bytes(),
        );
        hasher.update(self.settings.author_comment_style.as_bytes());
        hex::encode(hasher.finalize())
    }

    pub fn clear_cache(&mut self) {
        self.section_cache.clear();
        self.cache_dirty = false;
    }

    pub fn convert_novel_by_id(&mut self, id: i64, novel_dir: &std::path::Path) -> Result<String> {
        let toc_path = novel_dir.join("toc.yaml");
        let toc_content = std::fs::read_to_string(&toc_path).map_err(|e| NarouError::Io(e))?;
        let toc: crate::downloader::TocFile =
            serde_yaml::from_str(&toc_content).map_err(|e| NarouError::Yaml(e))?;

        let toc_object = crate::downloader::TocObject {
            title: toc.title,
            author: toc.author,
            toc_url: toc.toc_url,
            story: toc.story,
            subtitles: toc.subtitles,
            novel_type: toc.novel_type,
        };

        let sections = load_sections_from_dir(novel_dir, &toc_object.subtitles)?;

        let aozora_text = self.convert_novel(&toc_object, &sections)?;
        let txt_path = create_output_text_path(&self.settings, id, novel_dir, &toc_object);
        std::fs::write(&txt_path, &aozora_text)?;

        Ok(txt_path.display().to_string())
    }

    pub fn convert_novel_by_id_with_device(
        &mut self,
        _id: i64,
        novel_dir: &std::path::Path,
        device: device::Device,
    ) -> Result<PathBuf> {
        let toc_path = novel_dir.join("toc.yaml");
        let toc_content = std::fs::read_to_string(&toc_path).map_err(|e| NarouError::Io(e))?;
        let toc: crate::downloader::TocFile =
            serde_yaml::from_str(&toc_content).map_err(|e| NarouError::Yaml(e))?;

        let toc_object = crate::downloader::TocObject {
            title: toc.title,
            author: toc.author,
            toc_url: toc.toc_url,
            story: toc.story,
            subtitles: toc.subtitles,
            novel_type: toc.novel_type,
        };

        let sections = load_sections_from_dir(novel_dir, &toc_object.subtitles)?;

        let aozora_text = self.convert_novel(&toc_object, &sections)?;
        let txt_path = create_output_text_path(&self.settings, _id, novel_dir, &toc_object);
        std::fs::write(&txt_path, &aozora_text)?;

        let output_manager = device::OutputManager::new(device);
        let base_name = txt_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("output");
        let final_path = output_manager.convert_file(&txt_path, novel_dir, base_name)?;

        Ok(final_path)
    }
}

fn load_sections_from_dir(
    novel_dir: &std::path::Path,
    subtitles: &[crate::downloader::SubtitleInfo],
) -> Result<Vec<crate::downloader::SectionFile>> {
    let section_dir = novel_dir.join(crate::downloader::SECTION_SAVE_DIR);
    let mut sections = Vec::new();

    for sub in subtitles {
        let filename = format!("{} {}.yaml", sub.index, sub.file_subtitle);
        let path = section_dir.join(&filename);
        let content = std::fs::read_to_string(&path).map_err(|e| NarouError::Io(e))?;
        let section: crate::downloader::SectionFile =
            serde_yaml::from_str(&content).map_err(|e| NarouError::Yaml(e))?;
        sections.push(section);
    }

    Ok(sections)
}

fn sanitize_filename_for_output(name: &str) -> String {
    let invalid = ['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'];
    let cleaned: String = name.chars().filter(|c| !invalid.contains(c)).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "output".to_string()
    } else {
        trimmed.chars().take(80).collect()
    }
}

fn create_output_text_path(
    settings: &NovelSettings,
    id: i64,
    novel_dir: &Path,
    toc: &TocObject,
) -> PathBuf {
    novel_dir.join(create_output_text_filename(settings, id, toc))
}

fn create_output_text_filename(settings: &NovelSettings, id: i64, toc: &TocObject) -> String {
    if !settings.output_filename.trim().is_empty() {
        return ensure_txt_extension(&sanitize_filename_for_output(&settings.output_filename));
    }

    if convert_filename_to_ncode() {
        let record = crate::db::with_database(|db| Ok(db.get(id).cloned()))
            .ok()
            .flatten();
        let domain = record
            .as_ref()
            .and_then(|r| r.domain.clone())
            .or_else(|| extract_domain(&toc.toc_url))
            .unwrap_or_else(|| "unknown".to_string());
        let ncode = record
            .as_ref()
            .and_then(|r| r.ncode.clone())
            .or_else(|| extract_ncode_like(&toc.toc_url))
            .unwrap_or_else(|| sanitize_filename_for_output(&toc.title));
        return format!("{}_{}.txt", domain.replace('.', "_"), ncode);
    }

    let author = if settings.novel_author.is_empty() {
        &toc.author
    } else {
        &settings.novel_author
    };
    let title = if settings.novel_title.is_empty() {
        &toc.title
    } else {
        &settings.novel_title
    };
    ensure_txt_extension(&sanitize_filename_for_output(&format!("[{}] {}", author, title)))
}

fn convert_filename_to_ncode() -> bool {
    crate::db::with_database(|db| {
        let settings: HashMap<String, serde_yaml::Value> = db
            .inventory()
            .load("local_setting", crate::db::inventory::InventoryScope::Local)?;
        Ok(settings
            .get("convert.filename-to-ncode")
            .and_then(|value| value.as_bool())
            .unwrap_or(false))
    })
    .unwrap_or(false)
}

fn ensure_txt_extension(filename: &str) -> String {
    if filename.to_lowercase().ends_with(".txt") {
        filename.to_string()
    } else {
        format!("{filename}.txt")
    }
}

fn extract_domain(url: &str) -> Option<String> {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    without_scheme
        .split('/')
        .next()
        .filter(|domain| !domain.is_empty())
        .map(str::to_string)
}

fn extract_ncode_like(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .find(|part| !part.is_empty() && *part != "works")
        .map(str::to_string)
}

fn trim_author_comment_text(text: &str) -> String {
    text.trim_end_matches('\n')
        .lines()
        .map(|line| line.strip_prefix('\u{3000}').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_subtitle_markup(text: &str) -> String {
    text.replace("幕間［＃縦中横］１［＃縦中横終わり］", "幕間１")
        .replace("幕間［＃縦中横］２［＃縦中横終わり］", "幕間２")
        .replace("幕間［＃縦中横］３［＃縦中横終わり］", "幕間３")
        .replace("（［＃縦中横］１［＃縦中横終わり］）", "（１）")
        .replace("（［＃縦中横］２［＃縦中横終わり］）", "（２）")
        .replace("（［＃縦中横］３［＃縦中横終わり］）", "（３）")
}

fn normalize_story_source(story: &str) -> String {
    if looks_like_html(story) {
        crate::downloader::html::to_aozora(story)
    } else {
        story.to_string()
    }
}

fn looks_like_html(text: &str) -> bool {
    text.contains("<br")
        || text.contains("<BR")
        || text.contains("</p>")
        || text.contains("</P>")
        || text.contains("<ruby")
        || text.contains("<RUBY")
}
