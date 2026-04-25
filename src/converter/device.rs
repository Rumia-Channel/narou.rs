use std::ffi::OsString;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use regex::Regex;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::compat::{
    canonicalize_aozoraepub3_jar_dir, canonicalize_existing_path, configure_hidden_console_command,
    load_global_setting_string, resolve_java_command_path, sanitize_java_command,
};
use crate::downloader::util::decode_numeric_entities;
use crate::error::{NarouError, Result};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
unsafe extern "system" {
    fn GetVolumeInformationW(
        lp_root_path_name: *const u16,
        lp_volume_name_buffer: *mut u16,
        n_volume_name_size: u32,
        lp_volume_serial_number: *mut u32,
        lp_maximum_component_length: *mut u32,
        lp_file_system_flags: *mut u32,
        lp_file_system_name_buffer: *mut u16,
        n_file_system_name_size: u32,
    ) -> i32;
}

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
    no_strip: bool,
    use_dakuten_font: bool,
    yokogaki: bool,
}

fn file_contains_dakuten_chuki(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(s) => s.contains("［＃濁点］"),
        Err(_) => false,
    }
}

impl OutputManager {
    pub fn new(device: Device) -> Self {
        Self {
            device,
            aozora_epub3_path: Self::find_external_tool("AozoraEpub3"),
            kindlegen_path: Self::find_external_tool("kindlegen"),
            verbose: false,
            no_strip: false,
            use_dakuten_font: false,
            yokogaki: false,
        }
    }

    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    pub fn with_no_strip(mut self, no_strip: bool) -> Self {
        self.no_strip = no_strip;
        self
    }

    pub fn with_use_dakuten_font(mut self, use_dakuten_font: bool) -> Self {
        self.use_dakuten_font = use_dakuten_font;
        self
    }

    pub fn with_yokogaki(mut self, yokogaki: bool) -> Self {
        self.yokogaki = yokogaki;
        self
    }

    pub fn device(&self) -> Device {
        self.device
    }

    fn find_external_tool(name: &str) -> Option<PathBuf> {
        if name.eq_ignore_ascii_case("java") {
            return resolve_java_command_path();
        }

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

        let locator = if cfg!(windows) { "where" } else { "which" };
        let mut lookup = Command::new(locator);
        lookup.arg(name);
        configure_hidden_console_command(&mut lookup);
        if let Ok(output) = lookup.output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = path.lines().next() {
                    if !first_line.trim().is_empty() {
                        if let Some(canonical) =
                            canonicalize_existing_path(PathBuf::from(first_line.trim()))
                        {
                            return Some(canonical);
                        }
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
                    if let Some(canonical) = canonicalize_existing_path(&p) {
                        return Some(canonical);
                    }
                }
            }
        }

        None
    }

    fn find_kindlegen_next_to_aozora() -> Option<PathBuf> {
        let aozora = Self::find_aozora_epub3_from_settings()?;
        let suffix = if cfg!(windows) { ".exe" } else { "" };
        let candidate = aozora.parent()?.join(format!("kindlegen{suffix}"));
        candidate
            .exists()
            .then(|| canonicalize_existing_path(candidate))
            .flatten()
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

        candidates.into_iter().find_map(|candidate| {
            candidate
                .exists()
                .then(|| canonicalize_existing_path(candidate))
                .flatten()
        })
    }

    fn find_aozora_epub3_from_settings() -> Option<PathBuf> {
        let dir = load_global_setting_string("aozoraepub3dir")?;
        canonicalize_aozoraepub3_jar_dir(&dir)
    }

    fn aozora_device_name(&self) -> Option<&'static str> {
        match self.device {
            Device::Mobi => Some("kindle"),
            _ => None,
        }
    }

    fn aozora_ext_option<'a>(&self, output_ext: &'a str) -> Option<&'a str> {
        match self.device {
            Device::Kobo => Some(output_ext),
            _ => None,
        }
    }

    fn build_aozora_epub3_args(
        &self,
        input_txt: &Path,
        output_dir: &Path,
        output_ext: &str,
    ) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("-enc"),
            OsString::from("UTF-8"),
            OsString::from("-of"),
        ];

        if let Some(device_name) = self.aozora_device_name() {
            args.push(OsString::from("-device"));
            args.push(OsString::from(device_name));
        }

        if input_txt.parent().is_some_and(has_cover_image) {
            args.push(OsString::from("-c"));
            args.push(OsString::from("0"));
        }

        args.push(OsString::from("-dst"));
        args.push(absolutize_path(output_dir).into_os_string());

        if let Some(ext_option) = self.aozora_ext_option(output_ext) {
            args.push(OsString::from("-ext"));
            args.push(OsString::from(ext_option));
        }

        if self.yokogaki {
            args.push(OsString::from("-hor"));
        }

        args.push(absolutize_path(input_txt).into_os_string());
        args
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
            let java_path = resolve_java_command_path()
                .ok_or_else(|| NarouError::Conversion("java not found".into()))?;
            let jar_name = tool_path
                .file_name()
                .ok_or_else(|| NarouError::Conversion("Invalid AozoraEpub3 path".into()))?;
            let mut cmd = Command::new(java_path);
            // narou.rb 互換: JVM の文字コードを UTF-8 に固定する。
            // これがないと Windows 既定 (Shift-JIS) で読み書きされ、
            // UTF-8 の入力 txt 内の全角文字 (例: "１０") が文字化けする。
            cmd.arg("-Dfile.encoding=UTF-8");
            cmd.arg("-Dstdout.encoding=UTF-8");
            cmd.arg("-Dstderr.encoding=UTF-8");
            cmd.arg("-Dsun.stdout.encoding=UTF-8");
            cmd.arg("-Dsun.stderr.encoding=UTF-8");
            cmd.arg("-cp").arg(jar_name);
            cmd.arg("AozoraEpub3");
            cmd
        } else {
            Command::new(&tool_path)
        };

        sanitize_java_command(&mut cmd);
        configure_hidden_console_command(&mut cmd);
        cmd.current_dir(&working_dir);
        Ok((cmd, working_dir))
    }

    fn run_aozora_epub3(
        &self,
        input_txt: &Path,
        output_dir: &Path,
        output_ext: &str,
    ) -> Result<PathBuf> {
        let (mut cmd, working_dir) = self.build_aozora_command()?;
        let needs_dakuten = self.use_dakuten_font || file_contains_dakuten_chuki(input_txt);
        let _dakuten_guard = if needs_dakuten {
            Some(super::dakuten_font::DakutenFontGuard::activate(&working_dir)?)
        } else {
            None
        };
        let base_name = input_txt
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| NarouError::Conversion("Invalid input filename".into()))?;
        let output_path = output_dir.join(format!("{}{}", base_name, output_ext));
        let actual_output_path = absolutize_path(&output_path);

        if actual_output_path.exists() {
            std::fs::remove_file(&actual_output_path).map_err(|e| {
                NarouError::Conversion(format!(
                    "Failed to remove existing output file {}: {}",
                    output_path.display(),
                    e
                ))
            })?;
        }

        for arg in self.build_aozora_epub3_args(input_txt, output_dir, output_ext) {
            cmd.arg(arg);
        }
        if !self.verbose {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }

        let started_at = SystemTime::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| NarouError::Conversion(format!("Failed to run AozoraEpub3: {}", e)))?;

        let status = if self.verbose {
            eprintln!("AozoraEpub3でEPUBに変換しています");
            child
                .wait()
                .map_err(|e| NarouError::Conversion(format!("Failed to run AozoraEpub3: {}", e)))?
        } else {
            eprint!("AozoraEpub3でEPUBに変換しています");
            let _ = std::io::stderr().flush();
            let status = loop {
                match child.try_wait().map_err(|e| {
                    NarouError::Conversion(format!("Failed to wait for AozoraEpub3: {}", e))
                })? {
                    Some(status) => break status,
                    None => {
                        eprint!(".");
                        let _ = std::io::stderr().flush();
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
            };
            eprintln!();
            status
        };

        if !status.success() {
            return Err(NarouError::Conversion(format!(
                "AozoraEpub3 exited with status: {}",
                status
            )));
        }

        if !actual_output_path.exists() {
            return Err(NarouError::Conversion(format!(
                "AozoraEpub3 did not create expected output: {}",
                output_path.display()
            )));
        }

        if !aozora_output_looks_generated(&actual_output_path, started_at)? {
            return Err(NarouError::Conversion(format!(
                "AozoraEpub3 did not update expected output: {}",
                output_path.display()
            )));
        }

        eprintln!("変換しました");
        Ok(output_path)
    }

    fn create_ibunko_zip(&self, input_txt: &Path, include_illust: bool) -> Result<PathBuf> {
        eprintln!("zipファイルを作成中です...");
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
        data = illust_re.replace_all(&data, "<IMG SRC=\"$1\">").to_string();
        data = data.replace("［＃改ページ］", "<PBR>");
        data = data
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\n', "\r\n");

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
        eprintln!("作成しました");
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
                configure_hidden_console_command(&mut cmd2);
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

                if !self.no_strip {
                    if let Err(err) = strip_mobi_file(&mobi_output) {
                        eprintln!("{}", err);
                    }
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

#[derive(Debug, Clone)]
struct StripError(String);

impl StripError {
    fn invalid_format() -> Self {
        Self("invalid file format".to_string())
    }

    fn no_sources_section() -> Self {
        Self("File doesn't contain the sources section.".to_string())
    }

    fn invalid_srcs_section() -> Self {
        Self("SRCS section num does not point to SRCS.".to_string())
    }
}

impl fmt::Display for StripError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn strip_mobi_file(path: &Path) -> std::result::Result<(), StripError> {
    let data = std::fs::read(path).map_err(|e| StripError(e.to_string()))?;
    let stripped = strip_mobi_sources(&data)?;
    std::fs::write(path, stripped).map_err(|e| StripError(e.to_string()))?;
    Ok(())
}

fn strip_mobi_sources(datain: &[u8]) -> std::result::Result<Vec<u8>, StripError> {
    if slice_range(datain, 0x3c, 0x44)? != b"BOOKMOBI" {
        return Err(StripError::invalid_format());
    }

    let num_sections = read_be_u16(datain, 76)? as u32;
    let offset0 = read_be_u32(datain, 78)? as usize;
    let offset1 = read_be_u32(datain, 86)? as usize;
    let mobiheader = slice_range(datain, offset0, offset1)?;
    let srcs_secnum = read_be_u32(mobiheader, 0xe0)?;
    let srcs_cnt = read_be_u32(mobiheader, 0xe4)?;
    if srcs_secnum == u32::MAX || srcs_cnt == 0 {
        return Err(StripError::no_sources_section());
    }

    let next_section = srcs_secnum
        .checked_add(srcs_cnt)
        .ok_or_else(StripError::invalid_format)?;
    if srcs_secnum >= num_sections || next_section > num_sections {
        return Err(StripError::invalid_format());
    }
    let srcs_offset = read_be_u32(datain, 78 + (srcs_secnum as usize * 8))? as usize;
    let next_offset = read_be_u32(datain, 78 + (next_section as usize * 8))? as usize;
    if slice_range(datain, srcs_offset, srcs_offset + 4)? != b"SRCS" {
        return Err(StripError::invalid_srcs_section());
    }
    let srcs_length = next_offset
        .checked_sub(srcs_offset)
        .ok_or_else(StripError::invalid_format)?;
    let remaining_sections = num_sections
        .checked_sub(srcs_cnt)
        .ok_or_else(StripError::invalid_format)?;

    let mut data_file = Vec::new();
    data_file.extend_from_slice(slice_range(datain, 0, 68)?);
    data_file.extend_from_slice(&((remaining_sections * 2) + 1).to_be_bytes());
    data_file.extend_from_slice(slice_range(datain, 72, 76)?);
    data_file.extend_from_slice(&(remaining_sections as u16).to_be_bytes());

    let mut delta = -8i64 * i64::from(srcs_cnt);
    for i in 0..srcs_secnum {
        let offset = i64::from(read_be_u32(datain, 78 + (i as usize * 8))?) + delta;
        if offset < 0 {
            return Err(StripError::invalid_format());
        }
        let flgval = read_be_u32(datain, 82 + (i as usize * 8))?;
        data_file.extend_from_slice(&(offset as u32).to_be_bytes());
        data_file.extend_from_slice(&flgval.to_be_bytes());
    }

    delta -= srcs_length as i64;
    for i in next_section..num_sections {
        let offset = i64::from(read_be_u32(datain, 78 + (i as usize * 8))?) + delta;
        if offset < 0 {
            return Err(StripError::invalid_format());
        }
        let flgval = 2 * (i - srcs_cnt);
        data_file.extend_from_slice(&(offset as u32).to_be_bytes());
        data_file.extend_from_slice(&flgval.to_be_bytes());
    }

    let first_offset = read_be_u32(&data_file, 78)? as usize;
    if first_offset < data_file.len() {
        return Err(StripError::invalid_format());
    }
    data_file.resize(first_offset, 0);
    data_file.extend_from_slice(slice_range(datain, offset0, srcs_offset)?);
    data_file.extend_from_slice(slice_from(datain, srcs_offset + srcs_length)?);

    let new_offset0 = read_be_u32(&data_file, 78)? as usize;
    let new_offset1 = read_be_u32(&data_file, 86)? as usize;
    let mut new_mobiheader = slice_range(&data_file, new_offset0, new_offset1)?.to_vec();
    patch_range(&mut new_mobiheader, 0xe0, &u32::MAX.to_be_bytes())?;
    patch_range(&mut new_mobiheader, 0xe4, &0u32.to_be_bytes())?;
    update_exth121(&mut new_mobiheader, srcs_secnum, srcs_cnt);
    patch_range(&mut data_file, new_offset0, &new_mobiheader)?;

    Ok(data_file)
}

fn update_exth121(mobiheader: &mut [u8], srcs_secnum: u32, srcs_cnt: u32) {
    let Ok(mobi_length) = read_be_u32(mobiheader, 0x14) else {
        return;
    };
    let Ok(exth_flag) = read_be_u32(mobiheader, 0x80) else {
        return;
    };
    if exth_flag & 0x40 == 0 {
        return;
    }

    let exth_start = 16usize.saturating_add(mobi_length as usize);
    let Ok(exth_magic) = slice_range(mobiheader, exth_start, exth_start + 4) else {
        return;
    };
    if exth_magic != b"EXTH" {
        return;
    }
    let Ok(nitems) = read_be_u32(mobiheader, exth_start + 8) else {
        return;
    };

    let mut pos = exth_start + 12;
    for _ in 0..nitems {
        let Ok(item_type) = read_be_u32(mobiheader, pos) else {
            return;
        };
        let Ok(size) = read_be_u32(mobiheader, pos + 4) else {
            return;
        };
        if size < 8 {
            return;
        }
        if item_type == 121 {
            let Ok(boundaryptr) = read_be_u32(mobiheader, pos + 8) else {
                return;
            };
            if srcs_secnum <= boundaryptr {
                let adjusted = boundaryptr.saturating_sub(srcs_cnt).to_be_bytes();
                if patch_range(mobiheader, pos + 8, &adjusted).is_err() {
                    return;
                }
            }
        }
        pos = pos.saturating_add(size as usize);
    }
}

fn read_be_u16(data: &[u8], offset: usize) -> std::result::Result<u16, StripError> {
    let bytes = slice_range(data, offset, offset + 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_be_u32(data: &[u8], offset: usize) -> std::result::Result<u32, StripError> {
    let bytes = slice_range(data, offset, offset + 4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn patch_range(
    data: &mut [u8],
    offset: usize,
    replacement: &[u8],
) -> std::result::Result<(), StripError> {
    let range = data
        .get_mut(offset..offset + replacement.len())
        .ok_or_else(StripError::invalid_format)?;
    range.copy_from_slice(replacement);
    Ok(())
}

fn slice_range(data: &[u8], start: usize, end: usize) -> std::result::Result<&[u8], StripError> {
    data.get(start..end).ok_or_else(StripError::invalid_format)
}

fn slice_from(data: &[u8], start: usize) -> std::result::Result<&[u8], StripError> {
    data.get(start..).ok_or_else(StripError::invalid_format)
}

fn normalize_windows_verbatim_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if cfg!(windows) && raw.starts_with(r"\\?\") {
        PathBuf::from(raw.trim_start_matches(r"\\?\"))
    } else {
        path.to_path_buf()
    }
}

fn absolutize_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return normalize_windows_verbatim_path(path);
    }

    match std::env::current_dir() {
        Ok(cwd) => normalize_windows_verbatim_path(&cwd.join(path)),
        Err(_) => path.to_path_buf(),
    }
}

fn has_cover_image(dir: &Path) -> bool {
    [".jpg", ".png", ".jpeg"]
        .iter()
        .any(|ext| dir.join(format!("cover{}", ext)).is_file())
}

fn aozora_output_looks_generated(output_path: &Path, started_at: SystemTime) -> Result<bool> {
    let metadata = std::fs::metadata(output_path)?;
    let modified_is_newer = metadata
        .modified()
        .map(|modified| modified > started_at)
        .unwrap_or(false);
    Ok(modified_is_newer || metadata.len() > 0)
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

#[cfg(windows)]
fn find_windows_volume_root(volume_name: &str) -> Option<PathBuf> {
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:\\", letter as char);
        let path = PathBuf::from(&drive);
        if !path.exists() {
            continue;
        }

        if volume_matches(&drive, volume_name) {
            return Some(path);
        }
    }
    None
}

#[cfg(not(windows))]
fn find_windows_volume_root(_volume_name: &str) -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn volume_matches(root: &str, expected: &str) -> bool {
    let mut root_wide = std::ffi::OsStr::new(root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    let mut name_buffer = vec![0u16; 261];
    let mut serial = 0u32;

    let ok = unsafe {
        GetVolumeInformationW(
            root_wide.as_mut_ptr(),
            name_buffer.as_mut_ptr(),
            name_buffer.len() as u32,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return false;
    }

    let name_len = name_buffer
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(name_buffer.len());
    let label = String::from_utf16_lossy(&name_buffer[..name_len]);
    let serial_text = format!("{:04X}-{:04X}", serial >> 16, serial & 0xFFFF);
    label.eq_ignore_ascii_case(expected) || serial_text.eq_ignore_ascii_case(expected)
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        Device, OutputManager, StripError, absolutize_path, decode_ibunko_html_entities,
        normalize_windows_verbatim_path, strip_mobi_sources,
    };

    fn test_output_manager(device: Device) -> OutputManager {
        OutputManager {
            device,
            aozora_epub3_path: None,
            kindlegen_path: None,
            verbose: false,
            no_strip: false,
            use_dakuten_font: false,
            yokogaki: false,
        }
    }

    fn create_test_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-artifacts")
            .join("converter-device")
            .join(format!("{name}-{unique}"));
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stringify_args(args: Vec<std::ffi::OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

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

    #[test]
    fn kindle_aozora_args_match_ruby_flags() {
        let dir = create_test_dir("kindle-args");
        let input = dir.join("novel.txt");
        fs::write(&input, "test").unwrap();
        fs::write(dir.join("cover.jpg"), b"cover").unwrap();

        let manager = test_output_manager(Device::Mobi).with_yokogaki(true);
        let args = stringify_args(manager.build_aozora_epub3_args(&input, &dir, ".epub"));

        assert_eq!(args[0], "-enc");
        assert_eq!(args[1], "UTF-8");
        assert_eq!(args[2], "-of");
        assert!(args.windows(2).any(|pair| pair == ["-device", "kindle"]));
        assert!(args.windows(2).any(|pair| pair == ["-c", "0"]));
        assert!(args.contains(&"-hor".to_string()));
        assert!(!args.contains(&"-ext".to_string()));
        assert_eq!(PathBuf::from(args.last().unwrap()), absolutize_path(&input));
    }

    #[test]
    fn kobo_aozora_args_use_kepub_extension_only() {
        let dir = create_test_dir("kobo-args");
        let input = dir.join("novel.txt");
        fs::write(&input, "test").unwrap();

        let manager = test_output_manager(Device::Kobo);
        let args = stringify_args(manager.build_aozora_epub3_args(&input, &dir, ".kepub.epub"));

        assert!(args.windows(2).any(|pair| pair == ["-ext", ".kepub.epub"]));
        assert!(!args.contains(&"-device".to_string()));
        assert!(!args.contains(&"-hor".to_string()));
    }

    #[test]
    fn strip_mobi_sources_rejects_non_mobi_input() {
        let err = strip_mobi_sources(b"not-a-mobi").unwrap_err();
        assert_eq!(err.to_string(), StripError::invalid_format().to_string());
    }

    #[test]
    fn strip_mobi_sources_reports_missing_srcs_section() {
        let mut data = vec![0u8; 512];
        data[0x3c..0x44].copy_from_slice(b"BOOKMOBI");
        data[76..78].copy_from_slice(&2u16.to_be_bytes());
        data[78..82].copy_from_slice(&100u32.to_be_bytes());
        data[86..90].copy_from_slice(&400u32.to_be_bytes());
        data[100 + 0xe0..100 + 0xe4].copy_from_slice(&u32::MAX.to_be_bytes());
        data[100 + 0xe4..100 + 0xe8].copy_from_slice(&0u32.to_be_bytes());

        let err = strip_mobi_sources(&data).unwrap_err();
        assert_eq!(
            err.to_string(),
            StripError::no_sources_section().to_string()
        );
    }
}
