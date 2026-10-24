extern crate self as narou_rs;

#[cfg(feature = "native-runtime")]
#[macro_use]
mod output_macros;
#[cfg(test)]
pub(crate) mod test_support;

pub mod application;
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
pub mod db;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod downloader;
pub mod error;
pub mod hidden;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod illustration_animation;
pub mod illustration_store;
// ZIP は `illustration-animation` が有効なときだけビルドされるため、この
// cfg が native / Worker (wasm32) の両方で zip が存在する条件と一致する。
#[cfg(feature = "illustration-animation")]
pub mod transfer;
#[cfg(feature = "native-runtime")]
pub mod logger;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod login;
#[cfg(feature = "native-runtime")]
pub mod mail;
#[cfg(feature = "native-runtime")]
pub mod native;
pub mod platform;
#[cfg(feature = "native-runtime")]
pub mod progress;
#[cfg(feature = "native-runtime")]
pub mod queue;
pub mod setting_core;
pub mod setting_info;
#[cfg(feature = "native-runtime")]
pub mod startup_backup;
pub mod author;
#[cfg(feature = "native-runtime")]
pub mod tag_colors;
#[cfg(feature = "native-runtime")]
pub mod termcolor;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod title;
#[cfg(feature = "native-runtime")]
pub mod updater_promote;
#[cfg(feature = "native-runtime")]
pub mod version;
#[cfg(feature = "native-runtime")]
pub mod web;
