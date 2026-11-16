fn main() {
    // The shipped WASM identity is embedded via `option_env!`; rebuild when the
    // artifact pipeline re-fingerprints the source/configuration set.
    println!("cargo:rerun-if-env-changed=NAROU_TRANSFER_BUILD_ID");
}
