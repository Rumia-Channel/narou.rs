//! 起動時の CA バンドル検証。
//!
//! 2026-10-20 に公表された eMudhra 中間 CA 侵害では、侵害された中間 CA の
//! 秘密鍵が証明書から復元可能でした。失効情報を扱わないクライアントは
//! `AAA Certificate Services` / `emSign ECC Root CA - G3` 等へ連鎖する偽造
//! 証明書を受け入れてしまうため、リレーは TLS を委ねる curl / wget に
//! `NAROU_RELAY_CACERT` を渡す前に、バンドル内のルート指紋を検査し、
//! 既知の侵害ルートが含まれていれば起動を中止します (fail closed)。
//!
//! このモジュールは PEM の外枠 DER だけを検査し、証明書の内容検証は
//! curl / wget に任せます。検査の目的は「侵害ルートを含むバンドルを使わない
//! こと」と「明らかに壊れたバンドルで起動しないこと」です。

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

/// 既知の侵害ルート CA。`(表示名, SHA-256 (DER 全体, 小文字 hex))`。
///
/// 出所: <https://emudhra.we-have-a-problem.com/roots.json>
pub const COMPROMISED_CA_ROOTS: [(&str, &str); 4] = [
    (
        "AAA Certificate Services",
        "d7a7a0fb5d7e2731d771e9484ebcdef71d5f0c3e0a2948782bc83ee0ea699ef4",
    ),
    (
        "emSign ECC Root CA - G3",
        "86a1ecba089c4a8d3bbe2734c612ba341d813e043cf9e8a862cd5c57a36bbe6b",
    ),
    (
        "emSign Root TLS CA - G3",
        "7dd78d5f4f13459a83dff9abbba62edbaf6f2d102bf257fd712f4d9f2746ed8d",
    ),
    (
        "emSign Root CS CA - G3",
        "f2edaaff3d9b701fb4d14c9f285f8c0bad90fc3bcc1fa07d47f4718edaba34a5",
    ),
];

/// CA バンドル検証の失敗理由。
#[derive(Debug, thiserror::Error)]
pub enum CaError {
    #[error("CA バンドルに証明書がありません")]
    NoCertificates,
    #[error("CA バンドルに解析できない証明書があります")]
    UnparseableCertificate,
    #[error("CA バンドルに侵害されたルート証明書が含まれています: {0} — 更新された CA バンドルを用意してください")]
    Compromised(String),
    #[error("{0}")]
    Io(String),
}

/// ファイルを読んで検証する。
pub fn verify_cacert_file(path: &std::path::Path) -> Result<usize, CaError> {
    let bytes = std::fs::read(path).map_err(|error| CaError::Io(error.to_string()))?;
    verify_cacert(&bytes)
}

/// PEM バンドルを検証し、含まれる証明書数を返す。
pub fn verify_cacert(bytes: &[u8]) -> Result<usize, CaError> {
    verify_with_denylist(bytes, &COMPROMISED_CA_ROOTS)
}

/// 拒否リストを差し替えて検証する (テスト・将来の侵害ルート追加用)。
pub fn verify_with_denylist(bytes: &[u8], denied: &[(&str, &str)]) -> Result<usize, CaError> {
    let certificates = pem_certificates(bytes)?;
    let mut compromised = Vec::new();
    for certificate in &certificates {
        let fingerprint = sha256_hex(certificate);
        if let Some((name, _)) = denied
            .iter()
            .find(|(_, denied_fingerprint)| denied_fingerprint.eq_ignore_ascii_case(&fingerprint))
        {
            compromised.push((*name).to_string());
        }
    }
    if !compromised.is_empty() {
        return Err(CaError::Compromised(compromised.join(", ")));
    }
    Ok(certificates.len())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// PEM ブロックを DER にデコードし、外枠の ASN.1 SEQUENCE が壊れていないか
/// だけ確認する。
fn pem_certificates(bytes: &[u8]) -> Result<Vec<Vec<u8>>, CaError> {
    let text = String::from_utf8_lossy(bytes);
    let mut certificates = Vec::new();
    let mut rest = text.as_ref();
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    while let Some(begin) = rest.find(BEGIN) {
        let after_begin = &rest[begin + BEGIN.len()..];
        let Some(end) = after_begin.find(END) else {
            break;
        };
        let encoded = &after_begin[..end];
        let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
        let der = STANDARD
            .decode(cleaned.as_bytes())
            .map_err(|_| CaError::UnparseableCertificate)?;
        if !is_der_sequence(&der) {
            return Err(CaError::UnparseableCertificate);
        }
        certificates.push(der);
        rest = &after_begin[end + END.len()..];
    }
    if certificates.is_empty() {
        return Err(CaError::NoCertificates);
    }
    Ok(certificates)
}

/// 先頭が ASN.1 SEQUENCE で、長さがバッファ全体と一致するか。
fn is_der_sequence(der: &[u8]) -> bool {
    let Some(&tag) = der.first() else {
        return false;
    };
    if tag != 0x30 {
        return false;
    }
    let Some((&first_length, rest)) = der.get(1).map(|byte| (byte, &der[2..])) else {
        return false;
    };
    let (length, header_len) = if first_length & 0x80 == 0 {
        (usize::from(first_length), 2)
    } else {
        let count = usize::from(first_length & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return false;
        }
        let mut length = 0usize;
        for &byte in &rest[..count] {
            length = (length << 8) | usize::from(byte);
        }
        (length, 2 + count)
    };
    header_len.saturating_add(length) == der.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_CERT: &str = include_str!("../tests/fixtures/relay.invalid-cert.pem");

    #[test]
    fn accepts_the_test_certificate_bundle() {
        assert_eq!(verify_cacert(FIXTURE_CERT.as_bytes()).unwrap(), 1);
    }

    #[test]
    fn rejects_bundles_without_certificates() {
        assert!(matches!(
            verify_cacert(b"not a certificate\n"),
            Err(CaError::NoCertificates)
        ));
        assert!(matches!(verify_cacert(b""), Err(CaError::NoCertificates)));
    }

    #[test]
    fn rejects_unparseable_pem_blocks() {
        let broken = "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----";
        assert!(matches!(
            verify_cacert(broken.as_bytes()),
            Err(CaError::UnparseableCertificate)
        ));
        let not_der = "-----BEGIN CERTIFICATE-----\naGVsbG8=\n-----END CERTIFICATE-----";
        assert!(matches!(
            verify_cacert(not_der.as_bytes()),
            Err(CaError::UnparseableCertificate)
        ));
    }

    #[test]
    fn detects_a_denied_certificate_by_sha256_fingerprint() {
        let text = String::from_utf8_lossy(FIXTURE_CERT.as_bytes());
        let cleaned: String = text
            .split("-----BEGIN CERTIFICATE-----")
            .nth(1)
            .unwrap()
            .split("-----END CERTIFICATE-----")
            .next()
            .unwrap()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let der = STANDARD.decode(cleaned).unwrap();
        let fingerprint = sha256_hex(&der);
        let error = verify_with_denylist(FIXTURE_CERT.as_bytes(), &[("fixture root", &fingerprint)])
            .unwrap_err();
        match error {
            CaError::Compromised(names) => assert_eq!(names, "fixture root"),
            other => panic!("expected Compromised, got {other}"),
        }
        // 大文字小文字を無視して照合する。
        let upper = fingerprint.to_uppercase();
        assert!(verify_with_denylist(FIXTURE_CERT.as_bytes(), &[("fixture root", &upper)]).is_err());
    }

    #[test]
    fn compromized_root_constants_are_lowercase_sha256_hex() {
        for (name, fingerprint) in COMPROMISED_CA_ROOTS {
            assert!(!name.is_empty());
            assert_eq!(fingerprint.len(), 64, "{name}");
            assert!(
                fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "{name}"
            );
        }
    }
}
