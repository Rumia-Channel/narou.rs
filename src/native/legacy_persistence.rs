//! Private filesystem section lookup used by native codecs. Canonical IO belongs
//! to PersistenceService and its operation-owned reader lease/write guard.

use std::path::{Path, PathBuf};

use crate::downloader::SubtitleInfo;
use crate::error::Result;

pub(crate) fn resolve_section_file_path(section_dir: &Path, subtitle: &SubtitleInfo) -> Result<Option<PathBuf>> {
    crate::application::novel_access::require_private_path(section_dir)?;
    let exact = section_dir.join(crate::downloader::persistence::section_filename(subtitle));
    crate::application::novel_access::require_private_path(&exact)?;
    if exact.try_exists()? { return Ok(Some(exact)); }
    let prefix = format!("{} ", subtitle.index);
    let entries = match std::fs::read_dir(section_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_suffix(".yaml") else { continue; };
        if stem == subtitle.index || stem.starts_with(&prefix) {
            let path = entry.path();
            crate::application::novel_access::require_private_path(&path)?;
            return Ok(Some(path));
        }
    }
    Ok(None)
}
