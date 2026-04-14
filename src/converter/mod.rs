pub mod converter_base;
pub mod device;
pub mod ini;
pub mod inspector;
pub mod output;
pub mod render;
pub mod settings;
pub mod user_converter;

use std::collections::HashMap;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use settings::NovelSettings;
use user_converter::UserConverter;

use crate::downloader::{SectionElement, SectionFile, TocObject};
use crate::error::{NarouError, Result};
use crate::progress::ProgressReporter;

pub struct NovelConverter {
    settings: NovelSettings,
    user_converter: Option<UserConverter>,
    section_cache: HashMap<String, CacheEntry>,
    cache_dirty: bool,
    progress: Option<Box<dyn ProgressReporter>>,
    display_inspector: bool,
    last_inspection_output: Option<String>,
}

struct CacheEntry {
    digest: String,
    converted_section: render::ConvertedSection,
}

impl NovelConverter {
    pub fn new(settings: NovelSettings) -> Self {
        Self {
            settings,
            user_converter: None,
            section_cache: HashMap::new(),
            cache_dirty: false,
            progress: None,
            display_inspector: false,
            last_inspection_output: None,
        }
    }

    pub fn with_user_converter(settings: NovelSettings, user_converter: UserConverter) -> Self {
        Self {
            settings,
            user_converter: Some(user_converter),
            section_cache: HashMap::new(),
            cache_dirty: false,
            progress: None,
            display_inspector: false,
            last_inspection_output: None,
        }
    }

    pub fn set_progress(&mut self, progress: Box<dyn ProgressReporter>) {
        self.progress = Some(progress);
    }

    pub fn set_display_inspector(&mut self, display_inspector: bool) {
        self.display_inspector = display_inspector;
    }

    pub fn take_inspection_output(&mut self) -> Option<String> {
        self.last_inspection_output.take()
    }

    pub fn convert_novel(&mut self, toc: &TocObject, sections: &[SectionFile]) -> Result<String> {
        let mut converted_story = String::new();
        if let Some(ref story) = toc.story {
            if !story.is_empty() {
                let mut converter = self.make_converter();
                let story_text = render::normalize_story_source(story);
                converted_story = converter.convert(&story_text, converter_base::TextType::Story);
            }
        }

        let mut converted_sections = Vec::new();
        let total = sections.len() as u64;

        if let Some(ref p) = self.progress {
            p.set_length(total);
            p.set_message(&format!("Convert {}", toc.title));
        }

        for (i, section) in sections.iter().enumerate() {
            if let Some(ref p) = self.progress {
                p.set_message(&format!(
                    "Convert {} [{}/{}]",
                    toc.title,
                    i + 1,
                    sections.len()
                ));
            }

            let digest = self.compute_digest(&section.element, i);

            if let Some(cached) = self.section_cache.get(&digest) {
                converted_sections.push(cached.converted_section.clone());
                if let Some(ref p) = self.progress {
                    p.inc(1);
                }
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

            let intro_text = if self.settings.enable_erase_introduction {
                String::new()
            } else if is_html && !section.element.introduction.is_empty() {
                crate::downloader::html::to_aozora(&section.element.introduction)
            } else {
                section.element.introduction.clone()
            };
            let body_text = if is_html && !section.element.body.is_empty() {
                crate::downloader::html::to_aozora(&section.element.body)
            } else {
                section.element.body.clone()
            };
            let post_text = if self.settings.enable_erase_postscript {
                String::new()
            } else if is_html && !section.element.postscript.is_empty() {
                crate::downloader::html::to_aozora(&section.element.postscript)
            } else {
                section.element.postscript.clone()
            };
            let has_intro = !intro_text.is_empty();
            let has_post = !post_text.is_empty();

            if has_intro {
                batch_inputs.push((intro_text.clone(), converter_base::TextType::Introduction));
            }
            batch_inputs.push((body_text, converter_base::TextType::Body));
            if has_post {
                batch_inputs.push((post_text.clone(), converter_base::TextType::Postscript));
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
            let conv_intro = if has_intro {
                let r = results[ri].clone();
                ri += 1;
                r
            } else {
                String::new()
            };
            let conv_body = results[ri].clone();
            ri += 1;
            let conv_post = if has_post {
                let r = results[ri].clone();
                r
            } else {
                String::new()
            };

            let cs = render::ConvertedSection {
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
            if let Some(ref p) = self.progress {
                p.inc(1);
            }
        }

        if let Some(ref p) = self.progress {
            p.finish_with_message(&format!(
                "Convert {} done ({} sections)",
                toc.title,
                sections.len()
            ));
        }

        Ok(render::render_novel_text(
            &self.settings,
            toc,
            &converted_story,
            &converted_sections,
        ))
    }

    pub fn convert_subtitles_for_hotentry(
        &mut self,
        toc: &TocObject,
        subtitles: &[crate::downloader::SubtitleInfo],
        novel_dir: &std::path::Path,
    ) -> Result<String> {
        let sections = load_sections_from_dir(novel_dir, subtitles)?;
        let empty_toc = TocObject {
            title: toc.title.clone(),
            author: toc.author.clone(),
            toc_url: toc.toc_url.clone(),
            story: None,
            subtitles: subtitles.to_vec(),
            novel_type: toc.novel_type,
        };
        let aozora_text = self.convert_novel(&empty_toc, &sections)?;
        Ok(strip_book_header_and_footer(&aozora_text))
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
        hasher.update(
            self.settings
                .enable_erase_introduction
                .to_string()
                .as_bytes(),
        );
        hasher.update(self.settings.enable_erase_postscript.to_string().as_bytes());
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
        self.last_inspection_output = None;
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
        let txt_path = output::create_output_text_path(&self.settings, id, novel_dir, &toc_object);
        std::fs::write(&txt_path, &aozora_text)?;
        save_latest_convert(id)?;
        self.inspect_converted_text(&aozora_text)?;

        Ok(txt_path.display().to_string())
    }

    pub fn convert_novel_by_id_with_device(
        &mut self,
        _id: i64,
        novel_dir: &std::path::Path,
        device: device::Device,
    ) -> Result<PathBuf> {
        self.last_inspection_output = None;
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
        let txt_path = output::create_output_text_path(&self.settings, _id, novel_dir, &toc_object);
        std::fs::write(&txt_path, &aozora_text)?;
        save_latest_convert(_id)?;
        self.inspect_converted_text(&aozora_text)?;

        let output_manager = device::OutputManager::new(device);
        let base_name = txt_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("output");
        let final_path = output_manager.convert_file(&txt_path, novel_dir, base_name)?;

        Ok(final_path)
    }

    fn inspect_converted_text(&mut self, aozora_text: &str) -> Result<()> {
        let mut inspector = inspector::Inspector::new(&self.settings);
        if self.settings.enable_inspect {
            inspector
                .inspect_end_touten_conditions(aozora_text, self.settings.enable_auto_join_line);
            inspector.countup_return_in_brackets(
                aozora_text,
                self.settings.enable_auto_join_in_brackets,
            );
        }
        inspector.save().map_err(NarouError::Io)?;
        self.last_inspection_output = if self.display_inspector {
            inspector.display_text()
        } else {
            inspector.summary_text()
        };
        Ok(())
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

fn save_latest_convert(id: i64) -> Result<()> {
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    let mut latest: std::collections::HashMap<String, serde_yaml::Value> = inventory.load(
        "latest_convert",
        crate::db::inventory::InventoryScope::Local,
    )?;
    latest.insert(
        "id".to_string(),
        serde_yaml::Value::Number(serde_yaml::Number::from(id)),
    );
    inventory.save(
        "latest_convert",
        crate::db::inventory::InventoryScope::Local,
        &latest,
    )?;
    Ok(())
}

fn strip_book_header_and_footer(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(first_page_break) = lines.iter().position(|line| *line == "［＃改ページ］")
    else {
        return text.to_string();
    };

    let mut start = first_page_break;
    while start > 0 && lines[start - 1].is_empty() {
        start -= 1;
    }

    let mut end = lines.len();
    while end > start && lines[end - 1].is_empty() {
        end -= 1;
    }

    let footer = "［＃ここから地付き］［＃小書き］（本を読み終わりました）［＃小書き終わり］［＃ここで地付き終わり］";
    if end > start && lines[end - 1] == footer {
        end -= 1;
        while end > start && lines[end - 1].is_empty() {
            end -= 1;
        }
    }

    lines[start..end].join("\n")
}
