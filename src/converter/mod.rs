pub mod converter_base;
pub mod settings;

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use settings::NovelSettings;

use crate::downloader::{SectionElement, TocObject};
use crate::error::{NarouError, Result};

pub struct NovelConverter {
    settings: NovelSettings,
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

            let mut converter = converter_base::ConverterBase::new(self.settings.clone());

            let mut batch_inputs = Vec::new();

            if let Some(intro) = &section.introduction {
                if !intro.is_empty() {
                    batch_inputs.push((intro.clone(), converter_base::TextType::Introduction));
                }
            }

            batch_inputs.push((section.body.clone(), converter_base::TextType::Body));

            if let Some(post) = &section.postscript {
                if !post.is_empty() {
                    batch_inputs.push((post.clone(), converter_base::TextType::Postscript));
                }
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

        let title = self.settings.novel_title.as_str();
        let author = self.settings.novel_author.as_str();

        output.push_str(&format!("{}\n", title));
        output.push_str(&format!("{}\n", author));
        output.push('\n');

        if let Some(ref story) = toc.story {
            if !story.is_empty() {
                output.push_str(&format!("{}\n", story));
                output.push('\n');
            }
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
        if let Some(ref intro) = section.introduction {
            hasher.update(intro.as_bytes());
        }
        if let Some(ref post) = section.postscript {
            hasher.update(post.as_bytes());
        }
        hasher.update(index.to_le_bytes());
        hasher.update(self.compute_settings_signature().as_bytes());
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

    pub fn convert_novel_by_id(
        &mut self,
        id: i64,
        novel_dir: &std::path::Path,
    ) -> Result<String> {
        let toc_path = novel_dir.join("toc.yaml");
        let toc_content = std::fs::read_to_string(&toc_path)
            .map_err(|e| NarouError::Io(e))?;
        let toc: crate::downloader::TocFile = serde_yaml::from_str(&toc_content)
            .map_err(|e| NarouError::Yaml(e))?;

        let toc_object = crate::downloader::TocObject {
            title: toc.title,
            author: toc.author,
            toc_url: toc.toc_url,
            story: toc.story,
            subtitles: toc.subtitles,
            novel_type: toc.novel_type,
        };

        let section_dir = novel_dir.join(crate::downloader::SECTION_SAVE_DIR);
        let mut sections = Vec::new();

        for sub in &toc_object.subtitles {
            let filename = format!("{} {}.yaml", sub.index, sub.file_subtitle);
            let path = section_dir.join(&filename);
            let content = std::fs::read_to_string(&path)
                .map_err(|e| NarouError::Io(e))?;
            let section: crate::downloader::SectionElement = serde_yaml::from_str(&content)
                .map_err(|e| NarouError::Yaml(e))?;
            sections.push(section);
        }

        self.convert_novel(&toc_object, &sections)
    }
}
