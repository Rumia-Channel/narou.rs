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

    pub fn convert_file(
        &self,
        input_txt: &Path,
        output_dir: &Path,
        base_name: &str,
    ) -> Result<PathBuf> {
        match self.device {
            Device::Text => {
                let output = output_dir.join(format!("{}{}", base_name, self.device.extension()));
                std::fs::copy(input_txt, &output)?;
                Ok(output)
            }
            Device::Epub | Device::Kobo => {
                let tool_path = self
                    .aozora_epub3_path
                    .as_ref()
                    .ok_or_else(|| NarouError::Conversion("AozoraEpub3 not found".into()))?;
                let epub_output =
                    output_dir.join(format!("{}{}", base_name, self.device.extension()));

                let mut cmd = Command::new(tool_path);
                cmd.arg("-i").arg(input_txt);
                cmd.arg("-o").arg(&epub_output);

                if self.device == Device::Kobo {
                    cmd.arg("--device").arg("kobo");
                }

                let status = cmd.status().map_err(|e| {
                    NarouError::Conversion(format!("Failed to run AozoraEpub3: {}", e))
                })?;

                if !status.success() {
                    return Err(NarouError::Conversion(format!(
                        "AozoraEpub3 exited with status: {}",
                        status
                    )));
                }

                Ok(epub_output)
            }
            Device::Mobi => {
                let aozora_path = self
                    .aozora_epub3_path
                    .as_ref()
                    .ok_or_else(|| NarouError::Conversion("AozoraEpub3 not found".into()))?;
                let epub_temp = output_dir.join(format!("{}_temp.epub", base_name));

                let mut cmd = Command::new(aozora_path);
                cmd.arg("-i").arg(input_txt);
                cmd.arg("-o").arg(&epub_temp);

                let status = cmd.status().map_err(|e| {
                    NarouError::Conversion(format!("Failed to run AozoraEpub3: {}", e))
                })?;

                if !status.success() {
                    return Err(NarouError::Conversion(format!(
                        "AozoraEpub3 exited with status: {}",
                        status
                    )));
                }

                let kindlegen_path = self
                    .kindlegen_path
                    .as_ref()
                    .ok_or_else(|| NarouError::Conversion("kindlegen not found".into()))?;
                let mobi_output =
                    output_dir.join(format!("{}{}", base_name, self.device.extension()));

                let mut cmd2 = Command::new(kindlegen_path);
                cmd2.arg(&epub_temp);
                cmd2.arg("-o").arg(&mobi_output);

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
