//! Browser WASM wrapper around the shared `narou_rs::transfer` core.
//!
//! The CLI and native Web API use the same core directly. The browser uses
//! this wrapper to build an archive locally from server-provided files and to
//! validate/plan an uploaded archive before sending it to the server.
//!
//! All JSON crossing the boundary is `serde_json`; binary file bytes are
//! base64 inside JSON. The functions never touch the DOM, network or storage.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use narou_rs::transfer::{
    ArchiveFile, ArchiveManifest, ConflictPolicy, ExistingNovel, build_archive, parse_archive,
    plan_import,
};
use wasm_bindgen::prelude::*;

fn js_error(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}

/// Build a ZIP from a manifest JSON and a flat file list.
///
/// `manifest_json`: an `ArchiveManifest` without file bytes (the `files`
/// arrays may be empty; they are replaced here).
/// `files_json`: `[{ "novel": 0, "path": "toc.yaml", "data": "<base64>" }, ...]`
#[wasm_bindgen]
pub fn build_zip(manifest_json: &str, files_json: &str) -> Result<Vec<u8>, JsValue> {
    let mut manifest: ArchiveManifest =
        serde_json::from_str(manifest_json).map_err(js_error)?;
    let files: Vec<FileInput> = serde_json::from_str(files_json).map_err(js_error)?;
    for novel in &mut manifest.novels {
        novel.files.clear();
    }
    for file in files {
        let Some(novel) = manifest.novels.get_mut(file.novel) else {
            return Err(js_error(format!("unknown novel index {}", file.novel)));
        };
        let bytes = BASE64.decode(file.data.as_bytes()).map_err(js_error)?;
        novel.files.push(ArchiveFile::new(file.path, bytes));
    }
    build_archive(&manifest).map_err(js_error)
}

/// Validate an uploaded ZIP and return its manifest plus per-novel file
/// metadata (without file bytes).
#[wasm_bindgen]
pub fn parse_zip(zip_bytes: &[u8]) -> Result<String, JsValue> {
    let parsed = parse_archive(zip_bytes).map_err(js_error)?;
    let novels = parsed
        .novels
        .iter()
        .map(|novel| {
            serde_json::json!({
                "index": novel.index,
                "toc_url": novel.toc_url,
                "title": novel.title,
                "files": novel.files.iter().map(|(path, bytes)| {
                    serde_json::json!({ "path": path, "size": bytes.len() })
                }).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&serde_json::json!({
        "manifest": parsed.manifest,
        "novels": novels,
    }))
    .map_err(js_error)
}

/// Validate an uploaded ZIP and return the import plan for the current
/// library (`existing_json`: `[{ "toc_url": "...", "title": "..." }]`).
#[wasm_bindgen]
pub fn plan_zip(
    zip_bytes: &[u8],
    existing_json: &str,
    overwrite: bool,
) -> Result<String, JsValue> {
    let parsed = parse_archive(zip_bytes).map_err(js_error)?;
    let existing: Vec<ExistingNovel> =
        serde_json::from_str(existing_json).map_err(js_error)?;
    let policy = if overwrite {
        ConflictPolicy::Overwrite
    } else {
        ConflictPolicy::Skip
    };
    let plan = plan_import(&parsed, &existing, policy);
    serde_json::to_string(&serde_json::json!({
        "manifest": parsed.manifest,
        "plan": plan,
    }))
    .map_err(js_error)
}

#[derive(serde::Deserialize)]
struct FileInput {
    novel: usize,
    path: String,
    data: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use narou_rs::transfer::ArchiveNovel;

    fn manifest() -> ArchiveManifest {
        ArchiveManifest {
            format_version: narou_rs::transfer::FORMAT_VERSION,
            app_version: Some("test".into()),
            exported_at: None,
            novels: vec![ArchiveNovel {
                toc_url: "https://example.test/novel/1".into(),
                title: "title".into(),
                author: "author".into(),
                sitename: "example".into(),
                domain: Some("example.test".into()),
                tags: vec![],
                novel_type: 1,
                files: vec![],
                ncode: None,
                subtitle: None,
                general_lastup: None,
                last_update: None,
            }],
        }
    }

    #[test]
    fn build_parse_roundtrip_through_json_boundary() {
        let json = serde_json::to_string(&manifest()).unwrap();
        let files = serde_json::json!([
            { "novel": 0, "path": "toc.yaml", "data": BASE64.encode(b"toc") },
            { "novel": 0, "path": "本文/1 第一話.yaml", "data": BASE64.encode(b"body") },
        ])
        .to_string();
        let zip = build_zip(&json, &files).unwrap();
        let parsed = parse_zip(&zip).unwrap();
        let value: serde_json::Value = serde_json::from_str(&parsed).unwrap();
        assert_eq!(value["novels"][0]["files"].as_array().unwrap().len(), 2);
        let planned = plan_zip(&zip, "[]", false).unwrap();
        let value: serde_json::Value = serde_json::from_str(&planned).unwrap();
        assert_eq!(value["plan"]["entries"][0]["action"], "Create");
    }
}
