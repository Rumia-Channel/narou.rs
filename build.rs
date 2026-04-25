use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha3::{Digest, Sha3_256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
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
        let bytes = fs::read(&path).expect("read asset bytes");
        output.push((relative, sha3_256_base64_url(&bytes)));
    }
}

fn sha3_256_base64_url(bytes: &[u8]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(bytes);
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}
