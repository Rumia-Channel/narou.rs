pub mod converter_base;
pub mod device;
pub mod ini;
pub mod inspector;
pub mod output;
pub mod render;
pub mod settings;
pub mod user_converter;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

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
    inspector: Rc<RefCell<inspector::Inspector>>,
    display_inspector: bool,
    last_inspection_output: Option<String>,
}

struct CacheEntry {
    digest: String,
    converted_section: render::ConvertedSection,
}

impl NovelConverter {
    pub fn new(settings: NovelSettings) -> Self {
        let inspector = Rc::new(RefCell::new(inspector::Inspector::new(&settings)));
        Self {
            settings,
            user_converter: None,
            section_cache: HashMap::new(),
            cache_dirty: false,
            progress: None,
            inspector,
            display_inspector: false,
            last_inspection_output: None,
        }
    }

    pub fn with_user_converter(settings: NovelSettings, user_converter: UserConverter) -> Self {
        let inspector = Rc::new(RefCell::new(inspector::Inspector::new(&settings)));
        Self {
            settings,
            user_converter: Some(user_converter),
            section_cache: HashMap::new(),
            cache_dirty: false,
            progress: None,
            inspector,
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
        let mut erased_intro_count = 0usize;
        let mut erased_post_count = 0usize;
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

            let chapter = section.chapter.clone();
            let subchapter = section.subchapter.clone();
            let subtitle = section.subtitle.clone();
            let inspect_subtitle = if !subtitle.trim().is_empty() {
                subtitle.trim().to_string()
            } else if !subchapter.trim().is_empty() {
                subchapter.trim().to_string()
            } else {
                chapter.trim().to_string()
            };
            self.inspector.borrow_mut().set_subtitle(inspect_subtitle);

            let is_html =
                section.element.data_type != "text" && section.element.data_type != "text/plain";
            let resolved_element = if is_html {
                self.resolve_section_html_illustrations(section)
            } else {
                section.element.clone()
            };
            let digest = self.compute_digest(&resolved_element, i);

            if let Some(cached) = self.section_cache.get(&digest) {
                converted_sections.push(cached.converted_section.clone());
                if let Some(ref p) = self.progress {
                    p.inc(1);
                }
                continue;
            }

            let mut converter = self.make_converter();

            let mut batch_inputs = Vec::new();

            if !chapter.is_empty() {
                batch_inputs.push((chapter.clone(), converter_base::TextType::Chapter));
            }
            if !subtitle.is_empty() {
                batch_inputs.push((subtitle.clone(), converter_base::TextType::Subtitle));
            }

            let intro_text = if self.settings.enable_erase_introduction {
                if !resolved_element.introduction.is_empty() {
                    erased_intro_count += 1;
                }
                String::new()
            } else if is_html && !resolved_element.introduction.is_empty() {
                crate::downloader::html::to_aozora(&resolved_element.introduction)
            } else {
                resolved_element.introduction.clone()
            };
            let body_text = if is_html && !resolved_element.body.is_empty() {
                crate::downloader::html::to_aozora(&resolved_element.body)
            } else {
                resolved_element.body.clone()
            };
            let post_text = if self.settings.enable_erase_postscript {
                if !resolved_element.postscript.is_empty() {
                    erased_post_count += 1;
                }
                String::new()
            } else if is_html && !resolved_element.postscript.is_empty() {
                crate::downloader::html::to_aozora(&resolved_element.postscript)
            } else {
                resolved_element.postscript.clone()
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

        if self.settings.enable_erase_introduction && erased_intro_count > 0 {
            self.inspector.borrow_mut().info(format!(
                "前書きをすべて削除しました。削除した数は{}個です。",
                erased_intro_count
            ));
        }
        if self.settings.enable_erase_postscript && erased_post_count > 0 {
            self.inspector.borrow_mut().info(format!(
                "後書きをすべて削除しました。削除した数は{}個です。",
                erased_post_count
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
            converter_base::ConverterBase::with_user_converter_and_inspector(
                self.settings.clone(),
                uc.clone(),
                self.inspector.clone(),
            )
        } else {
            converter_base::ConverterBase::with_inspector(
                self.settings.clone(),
                self.inspector.clone(),
            )
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

    fn resolve_section_html_illustrations(
        &mut self,
        section: &crate::downloader::SectionFile,
    ) -> SectionElement {
        let illust_dir = self.settings.archive_path.join("挿絵");
        let mut illust_count = 0usize;
        SectionElement {
            data_type: section.element.data_type.clone(),
            body: self.resolve_html_img_sources(
                &section.element.body,
                &illust_dir,
                &section.index,
                &mut illust_count,
            ),
            introduction: self.resolve_html_img_sources(
                &section.element.introduction,
                &illust_dir,
                &section.index,
                &mut illust_count,
            ),
            postscript: self.resolve_html_img_sources(
                &section.element.postscript,
                &illust_dir,
                &section.index,
                &mut illust_count,
            ),
        }
    }

    fn resolve_html_img_sources(
        &mut self,
        html: &str,
        illust_dir: &Path,
        section_index: &str,
        illust_count: &mut usize,
    ) -> String {
        let re = regex::Regex::new(r#"(?i)(<img[^>]+src=["'])([^"']+)(["'][^>]*>)"#).unwrap();
        re.replace_all(html, |caps: &regex::Captures| {
            let source = caps[2].to_string();
            let resolved = self.resolve_section_illustration_source(
                illust_dir,
                section_index,
                *illust_count,
                &source,
            );
            *illust_count += 1;
            match resolved {
                Some(localized) => format!("{}{}{}", &caps[1], localized, &caps[3]),
                None => String::new(),
            }
        })
        .to_string()
    }

    fn resolve_section_illustration_source(
        &mut self,
        illust_dir: &Path,
        section_index: &str,
        illust_index: usize,
        source: &str,
    ) -> Option<String> {
        if let Some(filename) =
            find_saved_section_illustration_filename(illust_dir, section_index, illust_index)
        {
            return Some(format!("挿絵/{}", filename));
        }

        if !is_remote_illustration_source(source) {
            return Some(source.to_string());
        }

        self.download_section_illustration(illust_dir, section_index, illust_index, source)
    }

    fn download_section_illustration(
        &mut self,
        illust_dir: &Path,
        section_index: &str,
        illust_index: usize,
        source: &str,
    ) -> Option<String> {
        let url = normalize_illustration_url(source);
        let (bytes, content_type) = match fetch_illustration_bytes(&url) {
            Ok((bytes, content_type)) => (bytes, content_type),
            Err(err) => {
                self.inspector.borrow_mut().error(format!(
                    "Illustration#download_image: {} を処理中に例外が発生しました({})",
                    url, err
                ));
                return None;
            }
        };
        let ext = match illustration_extension_from_content_type(&content_type) {
            Some(ext) => ext,
            None => {
                self.inspector.borrow_mut().error(format!(
                    "Illustration#download_image: {} は未対応の画像フォーマットです(content-type: {})",
                    url, content_type
                ));
                return None;
            }
        };

        if std::fs::create_dir_all(illust_dir).is_err() {
            return None;
        }

        let filename = format!("{}-{}.{}", section_index, illust_index, ext);
        if std::fs::write(illust_dir.join(&filename), &bytes).is_err() {
            return None;
        }

        self.inspector
            .borrow_mut()
            .info(format!("挿絵「{}」を保存しました。", filename));
        Some(format!("挿絵/{}", filename))
    }

    pub fn clear_cache(&mut self) {
        self.section_cache.clear();
        self.cache_dirty = false;
    }

    pub fn convert_text_file(&mut self, text: &str) -> Result<String> {
        self.last_inspection_output = None;
        self.inspector.borrow_mut().reset();

        let mut converter = self.make_converter();
        let mut aozora_text = converter.convert(text, converter_base::TextType::TextFile);
        if !self.settings.enable_enchant_midashi {
            self.inspector.borrow_mut().info(
                "テキストファイルの処理を実行しましたが、改行直後の見出し付与は有効になっていません。setting.ini の enable_enchant_midashi を true にすることをお薦めします。".to_string(),
            );
        }

        aozora_text = render::insert_cover_chuki_for_textfile(&self.settings, &aozora_text);
        let txt_path = output::create_output_text_path_for_textfile(&self.settings, &aozora_text);
        if let Some(parent) = txt_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&txt_path, &aozora_text)?;
        self.inspect_converted_text(&aozora_text)?;

        Ok(txt_path.display().to_string())
    }

    pub fn convert_novel_by_id(&mut self, id: i64, novel_dir: &std::path::Path) -> Result<String> {
        self.last_inspection_output = None;
        self.inspector.borrow_mut().reset();
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
        self.inspector.borrow_mut().reset();
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
        if self.settings.enable_inspect {
            self.inspector
                .borrow_mut()
                .inspect_end_touten_conditions(aozora_text, self.settings.enable_auto_join_line);
            self.inspector.borrow_mut().countup_return_in_brackets(
                aozora_text,
                self.settings.enable_auto_join_in_brackets,
            );
        }
        self.inspector.borrow().save().map_err(NarouError::Io)?;
        self.last_inspection_output = if self.display_inspector {
            self.inspector.borrow().display_text()
        } else {
            self.inspector.borrow().summary_text()
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

fn find_saved_section_illustration_filename(
    illust_dir: &Path,
    section_index: &str,
    illust_index: usize,
) -> Option<String> {
    let prefix = format!("{}-{}.", section_index, illust_index);
    std::fs::read_dir(illust_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .find(|filename| filename.starts_with(&prefix))
}

fn normalize_illustration_url(source: &str) -> String {
    let prefixed = if source.starts_with("//") {
        format!("https:{}", source)
    } else {
        source.to_string()
    };
    if prefixed.contains(".mitemin.net") {
        prefixed.replace("viewimagebig", "viewimage")
    } else {
        prefixed
    }
}

fn is_remote_illustration_source(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://") || source.starts_with("//")
}

fn illustration_extension_from_content_type(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/bmp" => Some("bmp"),
        _ => None,
    }
}

fn fetch_illustration_bytes(url: &str) -> std::result::Result<(Vec<u8>, String), String> {
    let user_agent = ua_generator::ua::spoof_firefox_ua().to_string();
    let mut handle = curl::easy::Easy::new();
    handle.url(url).map_err(|err| err.to_string())?;
    handle
        .useragent(&user_agent)
        .map_err(|err| err.to_string())?;
    handle
        .follow_location(true)
        .map_err(|err| err.to_string())?;
    let _ = handle.accept_encoding("gzip, deflate");

    let mut headers = curl::easy::List::new();
    headers
        .append("Accept: image/webp,image/apng,image/*,*/*;q=0.8")
        .map_err(|err| err.to_string())?;
    headers
        .append("Accept-Language: ja,en-US;q=0.9,en;q=0.8")
        .map_err(|err| err.to_string())?;
    headers
        .append("Accept-Charset: utf-8")
        .map_err(|err| err.to_string())?;
    headers
        .append("Connection: keep-alive")
        .map_err(|err| err.to_string())?;
    handle
        .http_headers(headers)
        .map_err(|err| err.to_string())?;

    let mut body = Vec::new();
    let mut content_type: Option<String> = None;
    {
        let mut transfer = handle.transfer();
        transfer
            .write_function(|data| {
                body.extend_from_slice(data);
                Ok(data.len())
            })
            .map_err(|err| err.to_string())?;
        transfer
            .header_function(|header| {
                if let Ok(line) = std::str::from_utf8(header) {
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("Content-Type") {
                            content_type = Some(
                                value
                                    .trim()
                                    .split(';')
                                    .next()
                                    .unwrap_or("")
                                    .trim()
                                    .to_string(),
                            );
                        }
                    }
                }
                true
            })
            .map_err(|err| err.to_string())?;
        transfer.perform().map_err(|err| err.to_string())?;
    }

    let code = handle.response_code().map_err(|err| err.to_string())?;
    if code >= 400 {
        return Err(format!("HTTP {}", code));
    }

    Ok((body, content_type.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::{
        NovelConverter, find_saved_section_illustration_filename,
        illustration_extension_from_content_type, normalize_illustration_url,
    };
    use crate::{
        converter::settings::NovelSettings,
        downloader::{SectionElement, SectionFile, TocObject},
    };

    fn make_temp_illustration_root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "narou-rs-illust-localize-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn make_illustration_section() -> SectionFile {
        SectionFile {
            index: "16".to_string(),
            href: String::new(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: "１６　発狂　（挿絵あり）".to_string(),
            file_subtitle: "１６　発狂　（挿絵あり）".to_string(),
            subdate: String::new(),
            subupdate: None,
            download_time: None,
            element: SectionElement {
                data_type: "html".to_string(),
                introduction: String::new(),
                postscript: String::new(),
                body: r#"<p>前</p><p><a href="//29644.mitemin.net/i422674/" target="_blank"><img src="//29644.mitemin.net/userpageimage/viewimagebig/icode/i422674/" alt="挿絵(By みてみん)" border="0" /></a></p><p>後</p>"#
                    .to_string(),
            },
        }
    }

    #[test]
    fn localize_section_html_illustrations_rewrites_existing_saved_images() {
        let root = make_temp_illustration_root();
        let illust_dir = root.join("挿絵");
        std::fs::create_dir_all(&illust_dir).unwrap();
        std::fs::write(illust_dir.join("16-0.png"), b"dummy").unwrap();

        let mut settings = NovelSettings::default();
        settings.archive_path = root.clone();
        let section = make_illustration_section();
        let mut converter = NovelConverter::new(settings);
        let resolved = converter.resolve_section_html_illustrations(&section);
        assert!(resolved.body.contains(r#"src="挿絵/16-0.png""#));
        assert_eq!(
            find_saved_section_illustration_filename(&illust_dir, "16", 0).as_deref(),
            Some("16-0.png")
        );
        assert_eq!(
            normalize_illustration_url(
                "https://29644.mitemin.net/userpageimage/viewimagebig/icode/i422674/"
            ),
            "https://29644.mitemin.net/userpageimage/viewimage/icode/i422674/"
        );
        assert_eq!(
            illustration_extension_from_content_type("image/png"),
            Some("png")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn convert_novel_keeps_localized_illustration_annotation() {
        let root = make_temp_illustration_root();
        let illust_dir = root.join("挿絵");
        std::fs::create_dir_all(&illust_dir).unwrap();
        std::fs::write(illust_dir.join("16-0.jpg"), b"dummy").unwrap();

        let mut settings = NovelSettings::default();
        settings.archive_path = root.clone();
        let toc = TocObject {
            title: "title".to_string(),
            author: "author".to_string(),
            toc_url: String::new(),
            story: None,
            subtitles: Vec::new(),
            novel_type: Some(0),
        };
        let mut converter = NovelConverter::new(settings);
        let text = converter
            .convert_novel(&toc, &[make_illustration_section()])
            .unwrap();

        assert!(text.contains("［＃挿絵（挿絵/16-0.jpg）入る］"), "{text}");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn convert_text_file_records_enchant_midashi_recommendation() {
        let root = make_temp_illustration_root();

        let mut settings = NovelSettings::default();
        settings.archive_path = root.clone();
        settings.output_filename = "converted.txt".to_string();
        settings.enable_enchant_midashi = false;
        settings.enable_inspect = true;

        let mut converter = NovelConverter::new(settings);
        converter.set_display_inspector(true);
        let output_path = converter
            .convert_text_file("タイトル\n作者\n本文です。\n")
            .unwrap();

        assert!(std::path::Path::new(&output_path).exists());
        let inspection = converter.take_inspection_output().unwrap_or_default();
        assert!(inspection.contains("改行直後の見出し付与は有効になっていません"));

        let saved_log = std::fs::read_to_string(root.join("調査ログ.txt")).unwrap();
        assert!(saved_log.contains("改行直後の見出し付与は有効になっていません"));

        let _ = std::fs::remove_dir_all(root);
    }
}
