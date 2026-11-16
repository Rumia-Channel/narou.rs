//! Portable virtual-parent identity rules.
//!
//! Kept in a dependency-free leaf module so the portable transfer core
//! (`transfer-runtime`) can classify virtual novels without pulling in
//! `crate::virtual_novel`'s runtime-gated library behaviour.
//! [`crate::virtual_novel`] re-exports these predicates.

/// 仮想小説の ncode 接頭辞。
pub const VIRTUAL_NCODE_PREFIX: &str = "vs_";

/// Internal membership marker tag (`vs_<ID>`); never travels in an archive.
pub fn is_internal_tag(tag: &str) -> bool {
    tag.strip_prefix(VIRTUAL_NCODE_PREFIX).is_some_and(|id| {
        !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// Whether `ncode` belongs to a virtual parent (`vs_<ID>`).
pub fn is_virtual_ncode(ncode: &str) -> bool {
    is_internal_tag(ncode)
}

/// Portable identity is independent of the library-local `vs_<ID>` ncode.
pub fn is_portable_toc_url(url: &str) -> bool {
    url.strip_prefix("https://narou.rs/virtual/").is_some_and(|identity| {
        identity.len() == 32
            && identity
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}
