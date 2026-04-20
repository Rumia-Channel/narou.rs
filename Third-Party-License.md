# Third-Party License Summary

This file summarizes third-party license information used or referenced by
narou_rs. The license for narou_rs itself is stored separately in `LICENSE`.

This summary is based on Cargo metadata for the Rust dependency graph and the
license notices that accompany the narou.rb reference software used during
development.

If dependency versions change, this file should be reviewed and updated.


## 1. narou_rs itself


- Project: narou_rs
- License: BSD 2-Clause
- Authoritative file: `LICENSE`


## 2. Referenced narou.rb software

narou_rs is developed with reference to narou.rb for compatibility checking.
The release packages produced for narou_rs do not distribute that reference
tree, but the upstream narou.rb software itself is MIT licensed.

- Project: Narou.rb
- License: MIT

The narou.rb license notice also covered bundled third-party components such as:

- bootbox.js: MIT
- CSS Toggle Switch: Unlicense / public domain style notice
- shortcut.js: BSD
- web_socket.rb: New BSD License
- Bootstrap: MIT


## 3. Direct Rust dependencies declared by narou_rs

The following table lists the direct Rust dependencies declared in
`Cargo.toml`, along with the license expression reported by Cargo metadata at
the time of this audit.

| Dependency | Version req | License |
| --- | --- | --- |
| askama | ^0.12 | MIT OR Apache-2.0 |
| axum | ^0.8 | MIT |
| base64 | ^0.22 | MIT OR Apache-2.0 |
| chrono | ^0.4 | MIT OR Apache-2.0 |
| chrono-tz | ^0.10.4 | MIT OR Apache-2.0 |
| clap | ^4 | MIT OR Apache-2.0 |
| csv | ^1.4 | Unlicense/MIT |
| ctrlc | ^3.5 | MIT/Apache-2.0 |
| curl | ^0.4 | MIT |
| dashmap | ^6 | MIT |
| encoding_rs | ^0.8 | (Apache-2.0 OR MIT) AND BSD-3-Clause |
| fancy-regex | ^0.17 | MIT |
| futures | ^0.3 | MIT OR Apache-2.0 |
| hex | ^0.4 | MIT OR Apache-2.0 |
| indicatif | ^0.18 | MIT |
| lettre | ^0.11 | MIT |
| open | ^5 | MIT |
| parking_lot | ^0.12 | MIT OR Apache-2.0 |
| pest | ^2.8 | MIT OR Apache-2.0 |
| pest_derive | ^2.8 | MIT OR Apache-2.0 |
| regex | ^1 | MIT OR Apache-2.0 |
| reqwest | ^0.12 | MIT OR Apache-2.0 |
| serde | ^1 | MIT OR Apache-2.0 |
| serde_json | ^1 | MIT OR Apache-2.0 |
| serde_yaml | ^0.9 | MIT OR Apache-2.0 |
| sha2 | ^0.10 | MIT OR Apache-2.0 |
| sha3 | ^0.11 | MIT OR Apache-2.0 |
| shell-words | ^1.1 | MIT/Apache-2.0 |
| similar | ^3.1 | Apache-2.0 |
| socket2 | ^0.6 | MIT OR Apache-2.0 |
| tempfile | ^3.27 | MIT OR Apache-2.0 |
| thiserror | ^2 | MIT OR Apache-2.0 |
| tokio | ^1 | MIT |
| tokio-stream | ^0.1 | MIT |
| tower-http | ^0.6 | MIT |
| tracing | ^0.1 | MIT |
| tracing-subscriber | ^0.3 | MIT |
| ua_generator | ^0.5 | MIT |
| unicode-normalization | ^0.1 | MIT OR Apache-2.0 |
| zip | ^8.5 | MIT |


## 4. Note

- This file is a summary, not a substitute for each upstream project's own
  license text.
- narou.rb itself is not part of the narou_rs release archives; it is listed
  here because it is a referenced upstream work used for compatibility work.
- For Rust crates, the authoritative source remains each crate's published
  metadata and upstream repository.
- Re-run the Cargo metadata audit after changing dependencies or updating
  `Cargo.lock`.
