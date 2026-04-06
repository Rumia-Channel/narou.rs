pub mod converter_base;
pub mod device;
pub mod settings;
pub mod user_converter;

use std::collections::HashMap;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use settings::NovelSettings;
use user_converter::UserConverter;

use crate::downloader::{SectionElement, TocObject};
use crate::error::{NarouError, Result};

pub struct NovelConverter {
    settings: NovelSettings,
    user_converter: Option<UserConverter>,
    section_cache: HashMap<String, CacheEntry>,
    cache_dirty: bool,
}

struct CacheEntry {
    digest: String,
    converted_lines: Vec<String>,
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

    pub fn convert_novel(
        &mut self,
        toc: &TocObject,
        sections: &[SectionElement],
    ) -> Result<String> {
        let mut converted_sections = Vec::new();

        for (i, section) in sections.iter().enumerate() {
            let digest = self.compute_digest(section, i);

            if let Some(cached) = self.section_cache.get(&digest) {
                converted_sections.push(cached.converted_lines.clone());
                continue;
            }

            let mut converter = if let Some(ref uc) = self.user_converter {
                converter_base::ConverterBase::with_user_converter(
                    self.settings.clone(),
                    uc.clone(),
                )
            } else {
                converter_base::ConverterBase::new(self.settings.clone())
            };

            let mut batch_inputs = Vec::new();

            if !section.introduction.is_empty() {
                batch_inputs.push((
                    section.introduction.clone(),
                    converter_base::TextType::Introduction,
                ));
            }

            batch_inputs.push((section.body.clone(), converter_base::TextType::Body));

            if !section.postscript.is_empty() {
                batch_inputs.push((
                    section.postscript.clone(),
                    converter_base::TextType::Postscript,
                ));
            }

            let results = converter.convert_multi(&batch_inputs);

            let mut section_lines = Vec::new();
            section_lines
                .push("\u{FF3B}\u{FF23}\u{6539}\u{30DA}\u{30FC}\u{30B8}\u{FF3D}".to_string());

            if i < toc.subtitles.len() {
                let sub = &toc.subtitles[i];
                if !sub.chapter.is_empty() {
                    section_lines.push(format!(
                        "\u{FF3B}\u{FF23}\u{4E09}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}\u{FF3B}\u{FF23}\u{5927}\u{898B}\u{51FA}\u{3057}\u{FF3D}{}\u{FF3B}\u{FF23}\u{5927}\u{898B}\u{51FA}\u{3057}\u{7D42}\u{308F}\u{308A}\u{FF3D}",
                        sub.chapter
                    ));
                }
                if !sub.subtitle.is_empty() {
                    section_lines.push(format!(
                        "\u{FF3B}\u{FF23}\u{4E09}\u{5B57}\u{4E0B}\u{3052}\u{FF3D}\u{FF3B}\u{FF23}\u{4E2D}\u{898B}\u{51FA}\u{3057}\u{FF3D}{}\u{FF3B}\u{FF23}\u{4E2D}\u{898B}\u{51FA}\u{3057}\u{7D42}\u{308F}\u{308A}\u{FF3D}",
                        sub.subtitle
                    ));
                }
            }

            for result_text in &results {
                for line in result_text.lines() {
                    section_lines.push(line.to_string());
                }
            }

            self.section_cache.insert(
                digest.clone(),
                CacheEntry {
                    digest,
                    converted_lines: section_lines.clone(),
                },
            );
            self.cache_dirty = true;

            converted_sections.push(section_lines);
        }

        self.render_novel_text(toc, &converted_sections)
    }

    fn render_novel_text(&self, toc: &TocObject, sections: &[Vec<String>]) -> Result<String> {
        let mut output = String::new();

        let title = toc.title.as_str();
        let author = toc.author.as_str();

        output.push_str(title);
        output.push('\n');
        output.push_str(author);
        output.push('\n');
        output.push('\n');

        output.push_str("\u{FF3B}\u{FF30}\u{533A}\u{5207}\u{7DDA}\u{FF3D}\n");

        if let Some(ref story) = toc.story {
            if !story.is_empty() {
                output.push_str("あらすじ：\n");
                output.push_str(story);
                output.push('\n');
            }
        }

        if !toc.toc_url.is_empty() {
            output.push_str("掲載ページ:\n");
            output.push_str(&format!("<{}>\n", toc.toc_url));
            output.push_str("\u{FF3B}\u{FF30}\u{533A}\u{5207}\u{7DDA}\u{FF3D}\n");
        }

        for section_lines in sections {
            for line in section_lines {
                output.push_str(line);
                output.push('\n');
            }
        }

        if self.settings.enable_display_end_of_book {
            output.push_str("\u{FF08}\u{672C}\u{3092}\u{8AAD}\u{307F}\u{7D42}\u{308F}\u{308A}\u{307E}\u{3057}\u{305F}\u{FF09}\n");
        }

        Ok(output)
    }

    fn compute_digest(&self, section: &SectionElement, index: usize) -> String {
        let mut hasher = Sha256::new();
        hasher.update(section.body.as_bytes());
        hasher.update(section.introduction.as_bytes());
        hasher.update(section.postscript.as_bytes());
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
        hex::encode(hasher.finalize())
    }

    pub fn clear_cache(&mut self) {
        self.section_cache.clear();
        self.cache_dirty = false;
    }

    pub fn convert_novel_by_id(&mut self, _id: i64, novel_dir: &std::path::Path) -> Result<String> {
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
        let output_dir = novel_dir.join("output");
        std::fs::create_dir_all(&output_dir)?;

        let base_name = sanitize_filename_for_output(&toc_object.title);
        let txt_path = output_dir.join(format!("{}.txt", base_name));
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
        let output_dir = novel_dir.join("output");
        std::fs::create_dir_all(&output_dir)?;

        let base_name = sanitize_filename_for_output(&toc_object.title);
        let txt_path = output_dir.join(format!("{}.txt", base_name));
        std::fs::write(&txt_path, &aozora_text)?;

        let output_manager = device::OutputManager::new(device);
        let final_path = output_manager.convert_file(&txt_path, &output_dir, &base_name)?;

        Ok(final_path)
    }
}

fn load_sections_from_dir(
    novel_dir: &std::path::Path,
    subtitles: &[crate::downloader::SubtitleInfo],
) -> Result<Vec<crate::downloader::SectionElement>> {
    let section_dir = novel_dir.join(crate::downloader::SECTION_SAVE_DIR);
    let mut sections = Vec::new();

    for sub in subtitles {
        let filename = format!("{} {}.yaml", sub.index, sub.file_subtitle);
        let path = section_dir.join(&filename);
        let content = std::fs::read_to_string(&path).map_err(|e| NarouError::Io(e))?;
        let section = if content.starts_with("---") {
            let without_front = content.replacen("---", "", 1);
            let section_file: crate::downloader::SectionFile =
                serde_yaml::from_str(without_front.trim_start())
                    .map_err(|e| NarouError::Yaml(e))?;
            section_file.element
        } else {
            let section: crate::downloader::SectionElement =
                serde_yaml::from_str(&content).map_err(|e| NarouError::Yaml(e))?;
            section
        };
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
