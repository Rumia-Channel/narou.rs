#[cfg(feature = "native-runtime")]
#[macro_use]
mod output_macros;
#[cfg(test)]
pub(crate) mod test_support;

pub mod application;
#[cfg(feature = "native-runtime")]
pub mod compat;
#[cfg(feature = "native-runtime")]
pub mod converter;
#[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
pub mod converter {
    #[path = "ini.rs"]
    pub mod ini;
}
pub mod db;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod downloader;
pub mod error;
#[cfg(any(feature = "native-runtime", feature = "worker-runtime"))]
pub mod illustration_store;
#[cfg(feature = "native-runtime")]
pub mod logger;
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
