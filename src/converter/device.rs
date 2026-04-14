use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regex::Regex;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::downloader::util::decode_numeric_entities;
use crate::error::{NarouError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Text,
    Epub,
    Mobi,
    Kobo,
    Ibunko,
    Reader,
    Ibooks,
}

impl Device {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "epub" => Device::Epub,
            "mobi" | "kindle" => Device::Mobi,
            "kobo" => Device::Kobo,
            "ibunko" => Device::Ibunko,
            "reader" => Device::Reader,
            "ibooks" => Device::Ibooks,
            _ => Device::Text,
        }
    }

    pub fn extension(&self) -> &str {
        match self {
            Device::Text => ".txt",
            Device::Epub => ".epub",
            Device::Mobi => ".mobi",
            Device::Kobo => ".epub",
            Device::Ibunko => ".zip",
            Device::Reader => ".epub",
            Device::Ibooks => ".epub",
        }
    }

    pub fn ebook_file_ext(&self) -> &str {
        match self {
            Device::Text => ".txt",
            Device::Epub => ".epub",
            Device::Mobi => ".mobi",
            Device::Kobo => ".kepub.epub",
            Device::Ibunko => ".zip",
            Device::Reader => ".epub",
            Device::Ibooks => ".epub",
        }
    }

    pub fn matches_ebook_file(&self, path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| {
                name.to_ascii_lowercase()
                    .ends_with(&self.ebook_file_ext().to_ascii_lowercase())
            })
            .unwrap_or(false)
    }

    pub fn display_name(&self) -> &str {
        match self {
            Device::Text => "text",
            Device::Epub => "EPUB",
            Device::Mobi => "Kindle",
            Device::Kobo => "Kobo",
            Device::Ibunko => "i文庫",
            Device::Reader => "SonyReader",
            Device::Ibooks => "iBooks",
        }
    }

    pub fn physical_support(&self) -> bool {
        matches!(self, Device::Mobi | Device::Kobo | Device::Reader)
    }

    pub fn volume_name(&self) -> Option<&'static str> {
        match self {
            Device::Mobi => Some("Kindle"),
            Device::Kobo => Some("KOBOeReader"),
            Device::Reader => Some("READER"),
            _ => None,
        }
    }

    pub fn documents_path_candidates(&self) -> &'static [&'static str] {
        match self {
            Device::Mobi => &["documents", "Documents", "Books"],
            Device::Kobo => &["/"],
            Device::Reader => &["Sony_Reader/media/books"],
            _ => &[],
        }
    }
}

pub struct OutputManager {
    device: Device,
    aozora_epub3_path: Option<PathBuf>,
    kindlegen_path: Option<PathBuf>,
    verbose: bool,
}

impl OutputManager {
    pub fn new(device: Device) -> Self {
        Self {
            device,
            aozora_epub3_path: Self::find_external_tool("AozoraEpub3"),
            kindlegen_path: Self::find_external_tool("kindlegen"),
            verbose: false,
        }
    }

    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    pub fn device(&self) -> Device {
        self.device
    }

    fn find_external_tool(name: &str) -> Option<PathBuf> {
        if name.eq_ignore_ascii_case("AozoraEpub3") {
            if let Some(path) = Self::find_aozora_epub3_from_settings() {
                return Some(path);
            }
        }

        if name.eq_ignore_ascii_case("kindlegen") {
            if let Some(path) = Self::find_kindlegen_next_to_aozora() {
                return Some(path);
            }
            if let Some(path) = Self::find_kindlegen_from_kindle_previewer() {
                return Some(path);
            }
        }

        if let Ok(output) = Command::new("where").arg(name).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = path.lines().next() {
                    if !first_line.trim().is_empty() {
                        return Some(PathBuf::from(first_line.trim()));
                    }
                }
            }
        }

        if cfg!(windows) {
            let candidates = [
                format!("C:\\Tools\\{}\\{}.bat", name, name),
                format!("C:\\Tools\\{}\\{}", name, name),
            ];
            for candidate in &candidates {
                let p = PathBuf::from(candidate);
                if p.exists() {
                    return Some(p);
                }
            }
        }

        None
    }

    fn find_kindlegen_next_to_aozora() -> Option<PathBuf> {
        let aozora = Self::find_aozora_epub3_from_settings()?;
        let suffix = if cfg!(windows) { ".exe" } else { "" };
        let candidate = aozora.parent()?.join(format!("kindlegen{suffix}"));
        candidate.exists().then_some(candidate)
    }

    fn find_kindlegen_from_kindle_previewer() -> Option<PathBuf> {
        if !cfg!(windows) {
            return None;
        }

        let relative = Path::new("Amazon")
            .join("Kindle Previewer 3")
            .join("lib")
            .join("fc")
            .join("bin")
            .join("kindlegen.exe");

        let mut candidates = Vec::new();
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(PathBuf::from(local_app_data).join(&relative));
        }
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            candidates.push(PathBuf::from(program_files).join(&relative));
        }
        if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
            candidates.push(PathBuf::from(program_files_x86).join(&relative));
        }

        candidates.into_iter().find(|candidate| candidate.exists())
    }

    fn find_aozora_epub3_from_settings() -> Option<PathBuf> {
        let settings_path = home_dir()?
            .join(".narousetting")
            .join("global_setting.yaml");
        let raw = std::fs::read_to_string(settings_path).ok()?;
        let settings =
            serde_yaml::from_str::<std::collections::BTreeMap<String, serde_yaml::Value>>(&raw)
                .ok()?;
        let dir = settings.get("aozoraepub3dir")?.as_str()?;
        let jar = PathBuf::from(dir).join("AozoraEpub3.jar");
        jar.exists().then_some(jar)
    }

    fn build_aozora_command(&self) -> Result<(Command, PathBuf)> {
        let tool_path = normalize_windows_verbatim_path(
            self.aozora_epub3_path
                .as_ref()
                .ok_or_else(|| NarouError::Conversion("AozoraEpub3 not found".into()))?,
        );

        let working_dir = tool_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let mut cmd = if tool_path.extension().and_then(|ext| ext.to_str()) == Some("jar") {
            let java_path =
                Self::find_external_tool("java").unwrap_or_else(|| PathBuf::from("java"));
            let jar_name = tool_path
                .file_name()
                .ok_or_else(|| NarouError::Conversion("Invalid AozoraEpub3 path".into()))?;
            let mut cmd = Command::new(java_path);
            cmd.arg("-cp").arg(jar_name);
            cmd.arg("AozoraEpub3");
            cmd
        } else {
            Command::new(&tool_path)
        };

        cmd.current_dir(&working_dir);
        Ok((cmd, working_dir))
    }

    fn run_aozora_epub3(
        &self,
        input_txt: &Path,
        output_dir: &Path,
        output_ext: &str,
    ) -> Result<PathBuf> {
        let (mut cmd, _) = self.build_aozora_command()?;
        let base_name = input_txt
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| NarouError::Conversion("Invalid input filename".into()))?;
        let output_path = output_dir.join(format!("{}{}", base_name, output_ext));

        cmd.arg("-d").arg(output_dir);
        cmd.arg("-ext").arg(output_ext);
        cmd.arg("-of");
        cmd.arg(input_txt);
        if !self.verbose {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }

        let status = cmd
            .status()
            .map_err(|e| NarouError::Conversion(format!("Failed to run AozoraEpub3: {}", e)))?;

        if !status.success() {
            return Err(NarouError::Conversion(format!(
                "AozoraEpub3 exited with status: {}",
                status
            )));
        }

        if !output_path.exists() {
            return Err(NarouError::Conversion(format!(
                "AozoraEpub3 did not create expected output: {}",
                output_path.display()
            )));
        }

        Ok(output_path)
    }

    fn create_ibunko_zip(
        &self,
        input_txt: &Path,
        include_illust: bool,
    ) -> Result<PathBuf> {
        let mut data = std::fs::read_to_string(input_txt)?;
        let html_re = Regex::new(r"</?[^>]+>").unwrap();
        loop {
            let next = html_re.replace_all(&data, "").to_string();
            if next == data {
                break;
            }
            data = next;
        }

        data = decode_ibunko_html_entities(&data);

        let illust_re = Regex::new(r"［＃挿絵（(.+?)）入る］").unwrap();
        data = illust_re
            .replace_all(&data, "<IMG SRC=\"$1\">")
            .to_string();
        data = data.replace("［＃改ページ］", "<PBR>");
        data = data.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n");

        let sanitized_txt_path = input_txt.with_file_name(format!(
            "{}.ibunko.txt",
            input_txt
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("output")
        ));
        std::fs::write(&sanitized_txt_path, data)?;

        let zipfile_path = input_txt.with_extension("zip");
        if zipfile_path.exists() {
            let _ = std::fs::remove_file(&zipfile_path);
        }

        {
            let file = std::fs::File::create(&zipfile_path)?;
            let mut zip = ZipWriter::new(file);
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

            zip.start_file(
                input_txt
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| NarouError::Conversion("Invalid iBunko filename".into()))?,
                options,
            )
            .map_err(|e| NarouError::Conversion(e.to_string()))?;
            zip.write_all(&std::fs::read(&sanitized_txt_path)?)?;

            if include_illust {
                let illust_dirpath = input_txt
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("挿絵");
                if illust_dirpath.exists() {
                    for entry in std::fs::read_dir(&illust_dirpath)? {
                        let entry = entry?;
                        if !entry.file_type()?.is_file() {
                            continue;
                        }
                        let name = entry.file_name().to_string_lossy().to_string();
                        zip.start_file(format!("挿絵/{name}"), options)
                            .map_err(|e| NarouError::Conversion(e.to_string()))?;
                        zip.write_all(&std::fs::read(entry.path())?)?;
                    }
                }
            }

            for ext in [".jpg", ".png", ".jpeg"] {
                let cover_name = format!("cover{ext}");
                let cover_path = input_txt
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(&cover_name);
                if cover_path.exists() {
                    zip.start_file(cover_name, options)
                        .map_err(|e| NarouError::Conversion(e.to_string()))?;
                    zip.write_all(&std::fs::read(cover_path)?)?;
                    break;
                }
            }

            zip.finish()
                .map_err(|e| NarouError::Conversion(e.to_string()))?;
        }

        let _ = std::fs::remove_file(sanitized_txt_path);
        Ok(zipfile_path)
    }

    pub fn convert_file(
        &self,
        input_txt: &Path,
        output_dir: &Path,
        base_name: &str,
        include_illust: bool,
    ) -> Result<PathBuf> {
        match self.device {
            Device::Text => Ok(input_txt.to_path_buf()),
            Device::Epub => self.run_aozora_epub3(input_txt, output_dir, ".epub"),
            Device::Kobo => self.run_aozora_epub3(input_txt, output_dir, ".kepub.epub"),
            Device::Ibunko => self.create_ibunko_zip(input_txt, include_illust),
            Device::Reader => self.run_aozora_epub3(input_txt, output_dir, ".epub"),
            Device::Ibooks => self.run_aozora_epub3(input_txt, output_dir, ".epub"),
            Device::Mobi => {
                let temp_input = output_dir.join(format!("{}_mobi_source.txt", base_name));
                std::fs::copy(input_txt, &temp_input)?;
                let epub_temp = match self.run_aozora_epub3(&temp_input, output_dir, ".epub") {
                    Ok(path) => path,
                    Err(err) => {
                        let _ = std::fs::remove_file(&temp_input);
                        return Err(err);
                    }
                };
                let _ = std::fs::remove_file(&temp_input);

                let kindlegen_path = self
                    .kindlegen_path
                    .as_ref()
                    .ok_or_else(|| NarouError::Conversion("kindlegen not found".into()))?;
                let mobi_output =
                    output_dir.join(format!("{}{}", base_name, self.device.extension()));

                let mut cmd2 = Command::new(kindlegen_path);
                cmd2.current_dir(output_dir);
                cmd2.arg(&epub_temp);
                cmd2.arg("-o").arg(
                    mobi_output
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| NarouError::Conversion("Invalid output filename".into()))?,
                );
                if !self.verbose {
                    cmd2.stdout(Stdio::null());
                    cmd2.stderr(Stdio::null());
                }

                let status2 = cmd2.status().map_err(|e| {
                    NarouError::Conversion(format!("Failed to run kindlegen: {}", e))
                })?;

                let _ = std::fs::remove_file(&epub_temp);

                if let Some(code) = status2.code() {
                    if code == 2 {
                        return Err(NarouError::Conversion(format!(
                            "kindlegen exited with status: {}",
                            status2
                        )));
                    }
                } else {
                    return Err(NarouError::Conversion(format!(
                        "kindlegen exited with status: {}",
                        status2
                    )));
                }

                if !mobi_output.exists() {
                    return Err(NarouError::Conversion(format!(
                        "kindlegen did not create expected output: {}",
                        mobi_output.display()
                    )));
                }

                Ok(mobi_output)
            }
        }
    }

    pub fn available_devices() -> Vec<(String, bool)> {
        let devices = vec![
            ("text".to_string(), true),
            (
                "epub".to_string(),
                Self::find_external_tool("AozoraEpub3").is_some(),
            ),
            ("mobi".to_string(), {
                Self::find_external_tool("kindlegen").is_some()
                    && Self::find_external_tool("AozoraEpub3").is_some()
            }),
            (
                "kobo".to_string(),
                Self::find_external_tool("AozoraEpub3").is_some(),
            ),
            ("ibunko".to_string(), true),
            (
                "reader".to_string(),
                Self::find_external_tool("AozoraEpub3").is_some(),
            ),
            (
                "ibooks".to_string(),
                Self::find_external_tool("AozoraEpub3").is_some(),
            ),
        ];
        devices
    }

    pub fn get_documents_path(&self) -> Option<PathBuf> {
        let volume_name = self.device.volume_name()?;
        let root = if cfg!(windows) {
            find_windows_volume_root(volume_name)
        } else if cfg!(target_os = "macos") {
            let path = PathBuf::from("/Volumes").join(volume_name);
            path.exists().then_some(path)
        } else {
            find_unix_volume_root(volume_name)
        }?;

        for relative in self.device.documents_path_candidates() {
            let candidate = if *relative == "/" {
                root.clone()
            } else {
                root.join(relative)
            };
            if candidate.is_dir() {
                return Some(candidate);
            }
        }

        None
    }

    pub fn connecting(&self) -> bool {
        self.device.physical_support() && self.get_documents_path().is_some()
    }

    pub fn ebook_file_old(&self, src_file: &Path) -> bool {
        let Some(documents_path) = self.get_documents_path() else {
            return true;
        };
        let dst_path = documents_path.join(
            src_file
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        );
        if !dst_path.exists() {
            return true;
        }
        let src_time = std::fs::metadata(src_file).and_then(|m| m.modified()).ok();
        let dst_time = std::fs::metadata(dst_path).and_then(|m| m.modified()).ok();
        match (src_time, dst_time) {
            (Some(src), Some(dst)) => src > dst,
            _ => true,
        }
    }

    pub fn copy_to_documents(&self, src_file: &Path) -> Result<Option<PathBuf>> {
        let Some(documents_path) = self.get_documents_path() else {
            return Ok(None);
        };
        let dst_path = documents_path.join(
            src_file
                .file_name()
                .ok_or_else(|| NarouError::Conversion("Invalid source filename".into()))?,
        );
        std::fs::copy(src_file, &dst_path)?;
        if crate::compat::load_local_setting_list("economy")
            .iter()
            .any(|v| v == "send_delete")
        {
            let _ = std::fs::remove_file(src_file);
        }
        Ok(Some(dst_path))
    }
}

fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home));
    }

    if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    } else {
        None
    }
}

fn normalize_windows_verbatim_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if cfg!(windows) && raw.starts_with(r"\\?\") {
        PathBuf::from(raw.trim_start_matches(r"\\?\"))
    } else {
        path.to_path_buf()
    }
}

fn decode_ibunko_html_entities(text: &str) -> String {
    let mut data = text
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    decode_numeric_entities(&mut data);
    data
}

fn find_windows_volume_root(volume_name: &str) -> Option<PathBuf> {
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:\\", letter as char);
        let path = PathBuf::from(&drive);
        if !path.exists() {
            continue;
        }

        let output = Command::new("cmd")
            .args(["/C", "vol", &drive])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.to_lowercase().contains(&volume_name.to_lowercase()) {
            return Some(path);
        }
    }
    None
}

fn find_unix_volume_root(volume_name: &str) -> Option<PathBuf> {
    let mut roots = vec![PathBuf::from("/media"), PathBuf::from("/mnt")];
    if let Some(home) = home_dir() {
        if let Some(user) = home.file_name().and_then(|v| v.to_str()) {
            roots.push(PathBuf::from("/run/media").join(user));
            roots.push(PathBuf::from("/media").join(user));
        }
    }

    for root in roots {
        let path = root.join(volume_name);
        if path.is_dir() {
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Device, decode_ibunko_html_entities, normalize_windows_verbatim_path};

    #[test]
    fn kobo_matches_kepub_output_suffix() {
        assert!(Device::Kobo.matches_ebook_file(Path::new("novel.kepub.epub")));
        assert!(!Device::Kobo.matches_ebook_file(Path::new("novel.epub")));
    }

    #[test]
    fn normalize_windows_verbatim_path_strips_prefix() {
        assert_eq!(
            normalize_windows_verbatim_path(Path::new(r"\\?\C:\Tools\AozoraEpub3\AozoraEpub3.jar")),
            Path::new(r"C:\Tools\AozoraEpub3\AozoraEpub3.jar")
        );
    }

    #[test]
    fn decode_ibunko_html_entities_decodes_ampersand_last() {
        assert_eq!(decode_ibunko_html_entities("&amp;lt;"), "&lt;");
        assert_eq!(decode_ibunko_html_entities("&lt;tag&gt;"), "<tag>");
    }
}
