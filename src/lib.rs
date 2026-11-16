extern crate self as narou_rs;

#[cfg(feature = "native-runtime")]
#[macro_use]
mod output_macros;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(all(test, feature = "native-runtime"))]
pub(crate) mod test_database;

#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod application;

/// Work-type classification constants shared by every runtime, including the
/// lean `transfer-runtime` build (no Lite/GPL, no application services).
pub mod attribute_tag;

/// Portable virtual-parent identity rules shared by every runtime.
pub mod virtual_identity;
#[cfg(feature = "lite")]
pub mod epub_lite;
#[cfg(feature = "native-runtime")]
pub mod compat;
// `converter` は native の fs/プロセス経路と、Worker のポータブル経路の両方を含む。
// worker ビルドでは native 専用の補助 (レポート整形・出力ファイル名など) が
// 未使用になるため、その構成に限り dead_code を許可する。
#[cfg_attr(not(feature = "native-runtime"), allow(dead_code))]
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod converter;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod db;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod downloader;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod error;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod hidden;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod illustration_animation;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod illustration_store;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod illustration_refs;
// `transfer-runtime` needs the codec + projection/protocol core only; the
// platform-store staging parts inside `transfer` stay gated on runtimes.
#[cfg(any(feature = "illustration-animation", feature = "transfer-runtime"))]
pub mod transfer;
#[cfg(feature = "native-runtime")]
pub mod logger;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod login;
#[cfg(feature = "native-runtime")]
pub mod mail;
#[cfg(feature = "native-runtime")]
pub mod native;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod platform;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod preset;
#[cfg(feature = "native-runtime")]
pub mod progress;
#[cfg(feature = "native-runtime")]
pub mod queue;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod setting_core;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod setting_info;
#[cfg(feature = "native-runtime")]
pub mod startup_backup;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod author;
#[cfg(feature = "native-runtime")]
pub mod tag_colors;
#[cfg(feature = "native-runtime")]
pub mod termcolor;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod title;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod virtual_novel;
#[cfg(feature = "native-runtime")]
pub mod updater_promote;
#[cfg(feature = "native-runtime")]
pub mod version;
#[cfg(feature = "native-runtime")]
pub mod web;
