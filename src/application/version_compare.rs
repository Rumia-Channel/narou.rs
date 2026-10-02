//! バージョン文字列の比較（native の自己更新と Worker の `/api/version/*` が共有する）。
//!
//! `src/version.rs` は native 専用ビルドに閉じているため、比較だけをここへ切り出して
//! 両方のビルドから使えるようにする。依存は標準ライブラリだけ。

/// Extract the numeric `x.y.z` core from a version string, ignoring `v`
/// prefixes, suffixes like `(develop)`/`(local-build)`, and any invisible
/// characters that may slip into release metadata.
pub fn version_core(version: &str) -> String {
    let mut core = String::new();
    for ch in version.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            core.push(ch);
        } else if !core.is_empty() {
            break;
        }
    }
    core.trim_end_matches('.').to_string()
}

/// Numeric semver-style comparison of two version cores.
/// Returns `Some(Ordering)` when both sides parse, `None` otherwise.
pub fn version_compare(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let parse = |v: &str| -> Option<Vec<u64>> {
        let parts: Option<Vec<u64>> = v.split('.').map(|p| p.parse().ok()).collect();
        parts.filter(|p| !p.is_empty())
    };
    let (av, bv) = (parse(a)?, parse(b)?);
    let len = av.len().max(bv.len());
    for i in 0..len {
        let x = av.get(i).copied().unwrap_or(0);
        let y = bv.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => {}
            ord => return Some(ord),
        }
    }
    Some(std::cmp::Ordering::Equal)
}

/// `true` when `current` is at most `boundary` (e.g. `version_at_most("0.4.0")`
/// for the self-update variant prompt). Unparseable input returns `false`.
pub fn version_at_most(current: &str, boundary: &str) -> bool {
    matches!(
        version_compare(&version_core(current), boundary),
        Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
    )
}
