//! Shared build-time verification for Native and Worker transfer assets.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub struct Artifacts {
    #[allow(dead_code, reason = "Native embeds the manifest; Worker serves it as a static asset.")]
    pub json: String,
    pub build_id: String,
    pub abi_version: u32,
    #[allow(dead_code, reason = "Native copies verified files; Worker only embeds their build identity.")]
    pub files: BTreeMap<String, String>,
}

fn take_string(fields: &mut Map<String, Value>, key: &str) -> Result<String, String> {
    match fields.remove(key) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(format!("transfer manifest requires string field {key}")),
    }
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\\')
        && name.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
}

fn verify_file(path: &Path, expected: &str) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!("{} is not a nonempty regular artifact", path.display()));
    }
    let mut file = fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    if path.extension().is_some_and(|extension| extension == "wasm") {
        let mut header = [0; 8];
        file.read_exact(&mut header).map_err(|error| format!("invalid WASM header: {error}"))?;
        if header != [0, b'a', b's', b'm', 1, 0, 0, 0] {
            return Err("transfer codec is not a WASM version 1 module".into());
        }
        hasher.update(header);
    }
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| format!("cannot hash artifact: {error}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    // The caller has validated all 64 lowercase hex digits.
    let nibble = |byte: u8| if byte.is_ascii_digit() { byte - b'0' } else { byte - b'a' + 10 };
    let expected_bytes: [u8; 32] = std::array::from_fn(|index| {
        nibble(expected.as_bytes()[index * 2]) * 16 + nibble(expected.as_bytes()[index * 2 + 1])
    });
    if hasher.finalize().as_slice() != expected_bytes {
        return Err(format!("artifact SHA-256 mismatch: {}", path.display()));
    }
    Ok(())
}

pub fn load(directory: &Path) -> Result<Artifacts, String> {
    let manifest_path = directory.join("manifest.json");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let json = fs::read_to_string(&manifest_path).map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    let Value::Object(mut fields) = serde_json::from_str(&json).map_err(|error| format!("invalid transfer manifest: {error}"))? else {
        return Err("transfer manifest must be an object".into());
    };
    for key in ["protocol_version", "abi_version"] {
        if fields.get(key).and_then(Value::as_u64) != Some(2) {
            return Err(format!("transfer manifest {key} must be 2"));
        }
    }
    let build_id = take_string(&mut fields, "build_id")?;
    if !is_hex_digest(&build_id) {
        return Err("transfer build_id must contain 64 lowercase hex digits".into());
    }
    let urls = [
        (take_string(&mut fields, "worker_url")?, "transfer-worker.", ".js"),
        (take_string(&mut fields, "glue_url")?, "transfer-codec.", ".js"),
        (take_string(&mut fields, "wasm_url")?, "transfer-codec.", ".wasm"),
    ];
    let Some(Value::Object(entries)) = fields.remove("files") else {
        return Err("transfer manifest files must be an object".into());
    };
    let mut files = BTreeMap::new();
    for (name, digest) in entries {
        let Value::String(digest) = digest else {
            return Err("transfer file digest must be a string".into());
        };
        if !is_safe_name(&name) || name == "manifest.json" || !is_hex_digest(&digest) {
            return Err(format!("invalid transfer artifact declaration: {name}"));
        }
        let path = directory.join(&name);
        println!("cargo:rerun-if-changed={}", path.display());
        verify_file(&path, &digest)?;
        files.insert(name, digest);
    }
    for (url, prefix, suffix) in urls {
        let name = url.strip_prefix("/assets/transfer/").ok_or("invalid transfer asset URL")?;
        let digest = files.get(name).ok_or("transfer URL target is not in files")?;
        if name.strip_prefix(prefix).and_then(|value| value.strip_suffix(suffix)) != Some(&digest[..16]) {
            return Err("transfer asset URL is not named by its content hash".into());
        }
    }
    Ok(Artifacts { json, build_id, abi_version: 2, files })
}
