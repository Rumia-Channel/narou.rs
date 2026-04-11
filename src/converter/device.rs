use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{NarouError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Text,
    Epub,
    Mobi,
    Kobo,
}

impl Device {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "epub" => Device::Epub,
            "mobi" | "kindle" => Device::Mobi,
            "kobo" => Device::Kobo,
            _ => Device::Text,
        }
    }

    pub fn extension(&self) -> &str {
        match self {
            Device::Text => ".txt",
            Device::Epub => ".epub",
            Device::Mobi => ".mobi",
            Device::Kobo => ".epub",
        }
    }
}

pub struct OutputManager {
    device: Device,
    aozora_epub3_path: Option<PathBuf>,
    kindlegen_path: Option<PathBuf>,
}

impl OutputManager {
    pub fn new(device: Device) -> Self {
        Self {
            device,
            aozora_epub3_path: Self::find_external_tool("AozoraEpub3"),
            kindlegen_path: Self::find_external_tool("kindlegen"),
        }
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
        let tool_path = self
            .aozora_epub3_path
            .as_ref()
            .ok_or_else(|| NarouError::Conversion("AozoraEpub3 not found".into()))?;

        let working_dir = tool_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let mut cmd = if tool_path.extension().and_then(|ext| ext.to_str()) == Some("jar") {
            let java_path =
                Self::find_external_tool("java").unwrap_or_else(|| PathBuf::from("java"));
            let mut cmd = Command::new(java_path);
            cmd.arg("-cp").arg(tool_path);
            cmd.arg("AozoraEpub3");
            cmd
        } else {
            Command::new(tool_path)
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

    pub fn convert_file(
        &self,
        input_txt: &Path,
        output_dir: &Path,
        base_name: &str,
    ) -> Result<PathBuf> {
        match self.device {
            Device::Text => Ok(input_txt.to_path_buf()),
            Device::Epub => self.run_aozora_epub3(input_txt, output_dir, ".epub"),
            Device::Kobo => self.run_aozora_epub3(input_txt, output_dir, ".kepub.epub"),
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

                let status2 = cmd2.status().map_err(|e| {
                    NarouError::Conversion(format!("Failed to run kindlegen: {}", e))
                })?;

                let _ = std::fs::remove_file(&epub_temp);

                if !status2.success() {
                    return Err(NarouError::Conversion(format!(
                        "kindlegen exited with status: {}",
                        status2
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
        ];
        devices
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
