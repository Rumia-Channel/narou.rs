use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha3::{Digest, Sha3_256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[path = "scripts/transfer-artifacts.rs"]
mod transfer_artifacts;

fn main() {
    configure_windows_uac_manifest();
    configure_windows_main_thread_stack();

    println!("cargo:rerun-if-env-changed=NAROU_RS_VERSION_OVERRIDE");
    if let Ok(override_version) = std::env::var("NAROU_RS_VERSION_OVERRIDE") {
        let trimmed = override_version.trim();
        if !trimmed.is_empty() {
            println!("cargo:rustc-env=NAROU_RS_VERSION_OVERRIDE={}", trimmed);
        }
    }

    println!("cargo:rerun-if-env-changed=NAROU_RS_RELEASE_BUILD");
    if let Ok(flag) = std::env::var("NAROU_RS_RELEASE_BUILD") {
        let trimmed = flag.trim();
        if matches!(trimmed, "1" | "true" | "TRUE" | "yes" | "YES") {
            println!("cargo:rustc-env=NAROU_RS_RELEASE_BUILD=1");
        }
    }

    println!("cargo:rerun-if-env-changed=NAROU_RS_UPDATER_SHA256");
    if let Ok(hash) = std::env::var("NAROU_RS_UPDATER_SHA256") {
        let trimmed = hash.trim().to_ascii_lowercase();
        if !trimmed.is_empty() {
            println!("cargo:rustc-env=NAROU_RS_UPDATER_SHA256={}", trimmed);
        }
    }

    let asset_root = PathBuf::from("src").join("web").join("assets");
    println!("cargo:rerun-if-changed={}", asset_root.display());

    let mut assets = Vec::new();
    collect_assets(&asset_root, &asset_root, &mut assets);
    assets.sort_by(|a, b| a.0.cmp(&b.0));

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let dest = out_dir.join("web_asset_versions.rs");
    let mut file = fs::File::create(dest).expect("create generated asset version file");

    writeln!(
        file,
        "pub const ASSET_PATHS: &[&str] = &[{}];",
        assets
            .iter()
            .map(|(path, _)| format!("{:?}", path))
            .collect::<Vec<_>>()
            .join(", ")
    )
    .expect("write asset paths");

    writeln!(file, "pub fn asset_version(path: &str) -> Option<&'static str> {{")
        .expect("write asset version header");
    writeln!(file, "    match path {{").expect("write asset version match");
    for (path, hash) in assets {
        writeln!(file, "        {:?} => Some({:?}),", path, hash).expect("write asset version entry");
    }
    writeln!(file, "        _ => None,").expect("write asset version default");
    writeln!(file, "    }}").expect("write asset version footer");
    writeln!(file, "}}").expect("write asset version end");

    if std::env::var_os("CARGO_FEATURE_NATIVE_RUNTIME").is_some() {
        emit_transfer_artifacts(&out_dir);
    }
}

fn configure_windows_uac_manifest() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }

    // Avoid Windows installer/updater heuristics for test harness names such as narou_rs_updater.
    for target in ["bins", "tests"] {
        println!("cargo:rustc-link-arg-{target}=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-{target}=/MANIFESTUAC:level='asInvoker' uiAccess='false'");
    }
}

/// Reserve the stack other platforms give a process by default.
///
/// Windows starts the main thread with 1 MiB, and an unoptimized build needs
/// more than that to parse this CLI: clap builds the whole command tree inside
/// `try_parse_from`, so every subcommand and argument adds frames. Debug builds
/// died with "has overflowed its stack" on `narou version` as soon as the
/// command set grew, while the optimized build stayed far below the limit.
fn configure_windows_main_thread_stack() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }

    for target in ["bins", "tests"] {
        println!("cargo:rustc-link-arg-{target}=/STACK:8388608");
    }
}

fn collect_assets(root: &Path, dir: &Path, output: &mut Vec<(String, String)>) {
    let entries = fs::read_dir(dir).expect("read asset dir");
    for entry in entries {
        let entry = entry.expect("read asset entry");
        let path = entry.path();
        if path.is_dir() {
            collect_assets(root, &path, output);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("asset under root")
            .to_string_lossy()
            .replace('\\', "/");
        if !is_versioned_asset(&relative) {
            continue;
        }
        let bytes = fs::read(&path).expect("read asset bytes");
        output.push((relative, sha3_256_base64_url(&bytes)));
    }
}

fn is_versioned_asset(path: &str) -> bool {
    matches!(
        Path::new(path).extension().and_then(|ext| ext.to_str()),
        Some("html" | "css" | "js" | "woff2")
    )
}

fn sha3_256_base64_url(bytes: &[u8]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(bytes);
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Browser transfer WASM artifacts.
///
/// `scripts/build-transfer-wasm.mjs` compiles `transfer_wasm` and emits
/// content-hashed glue/wasm/worker files plus `manifest.json` under
/// `target/transfer-artifacts/`. We copy them into OUT_DIR and generate the
/// include used by `src/web/frontend.rs` to serve `/assets/transfer/*`.
///
/// Development builds tolerate a missing set: `web_transfer_artifact` then
/// returns `None` and the UI reports the capability as unavailable (design
/// §14.1; never a server-side ZIP fallback). Release/CI packaging sets
/// `NAROU_RS_REQUIRE_TRANSFER_ARTIFACTS=1`, which turns absence into a hard
/// failure so nothing ships as "transfer capable" without the real artifacts.
fn emit_transfer_artifacts(out_dir: &Path) {
    println!("cargo:rerun-if-env-changed=NAROU_RS_REQUIRE_TRANSFER_ARTIFACTS");
    let artifact_dir = PathBuf::from("target").join("transfer-artifacts");
    let required = std::env::var("NAROU_RS_REQUIRE_TRANSFER_ARTIFACTS")
        .map(|value| !matches!(value.trim(), "" | "0" | "false" | "FALSE"))
        .unwrap_or(false);
    let artifacts = match transfer_artifacts::load(&artifact_dir) {
        Ok(artifacts) => Some(artifacts),
        Err(error) => {
            let detail = format!(
                "transfer WASM artifacts unavailable: {error}; run `node scripts/build-transfer-wasm.mjs --install-bindgen` before building"
            );
            if required {
                panic!("{detail}");
            }
            println!("cargo:warning={detail}");
            None
        }
    };
    let mut file = fs::File::create(out_dir.join("web_transfer_artifacts.rs"))
        .expect("create generated transfer artifact file");
    if let Some(artifacts) = artifacts {
        let dest_dir = out_dir.join("transfer");
        fs::create_dir_all(&dest_dir).expect("create transfer artifact directory");
        fs::write(dest_dir.join("manifest.json"), artifacts.json)
            .expect("copy verified transfer manifest");
        writeln!(
            file,
            "pub const TRANSFER_ARTIFACT_MANIFEST: Option<&'static str> = Some(include_str!(concat!(env!(\"OUT_DIR\"), \"/transfer/manifest.json\")));"
        )
        .expect("write transfer manifest constant");
        writeln!(
            file,
            "pub const TRANSFER_ARTIFACT_BUILD_ID: Option<&'static str> = Some({:?});",
            artifacts.build_id
        )
        .expect("write transfer build id");
        writeln!(
            file,
            "pub const TRANSFER_ARTIFACT_ABI_VERSION: Option<u32> = Some({});",
            artifacts.abi_version
        )
        .expect("write transfer ABI version");
        writeln!(
            file,
            "pub fn web_transfer_artifact(name: &str) -> Option<(&'static str, &'static [u8])> {{\n    match name {{"
        )
        .expect("write transfer artifact function");
        for name in artifacts.files.keys() {
            let target = dest_dir.join(name);
            fs::create_dir_all(target.parent().expect("artifact parent"))
                .expect("create artifact subdirectory");
            fs::copy(artifact_dir.join(name), target).expect("copy verified transfer artifact");
            writeln!(
                file,
                "        {:?} => Some(({:?}, &include_bytes!(concat!(env!(\"OUT_DIR\"), \"/transfer/{}\"))[..])),",
                name,
                transfer_content_type(name),
                name
            )
            .expect("write transfer artifact entry");
        }
        writeln!(file, "        _ => None,\n    }}\n}}").expect("write artifact match end");
    } else {
        writeln!(
            file,
            "pub const TRANSFER_ARTIFACT_MANIFEST: Option<&'static str> = None;\npub const TRANSFER_ARTIFACT_BUILD_ID: Option<&'static str> = None;\npub const TRANSFER_ARTIFACT_ABI_VERSION: Option<u32> = None;\npub fn web_transfer_artifact(_name: &str) -> Option<(&'static str, &'static [u8])> {{ None }}"
        )
        .expect("write unavailable transfer capability");
    }
}

fn transfer_content_type(name: &str) -> &'static str {
    match Path::new(name).extension().and_then(|ext| ext.to_str()) {
        Some("wasm") => "application/wasm",
        Some("js" | "mjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

