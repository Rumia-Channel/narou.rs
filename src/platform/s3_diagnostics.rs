//! Bounded, categorical S3 diagnostics. Never return response values or credentials.
//!
//! The server echoes are evidence about a received request, not proof that the
//! credentials are valid. An absent or malformed echo is inconclusive.

use std::sync::OnceLock;

use super::s3_sigv4::{self, Credentials, RequestToSign, SignedHeaders};

const MAX_ERROR_XML_BYTES: usize = 64 * 1024;
const MAX_XML_DEPTH: usize = 16;
const MAX_STRING_TO_SIGN_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field<'a> {
    Absent,
    Malformed,
    Value(&'a str),
}

/// Extract a unique, direct, exact child of a simple `<Error>` envelope.
///
/// This intentionally accepts only a small XML subset. Attributes, namespaces,
/// comments, CDATA, declarations other than the usual XML prolog, malformed
/// nesting and oversized bodies are inconclusive. No entity expansion occurs.
/// Nested fields and duplicate target fields are never treated as evidence.
fn field<'a>(body: &'a str, wanted: &str) -> Field<'a> {
    if body.len() > MAX_ERROR_XML_BYTES
        || body
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
    {
        return Field::Malformed;
    }
    let mut body = body.trim();
    for prolog in [
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>",
        "<?xml version=\"1.0\"?>",
    ] {
        if let Some(rest) = body.strip_prefix(prolog) {
            body = rest.trim_start();
            break;
        }
    }
    let Some(inner) = body
        .strip_prefix("<Error>")
        .and_then(|s| s.strip_suffix("</Error>"))
    else {
        return Field::Malformed;
    };
    let mut stack: Vec<(&str, usize)> = Vec::new();
    let mut offset = 0;
    let mut found = Field::Absent;
    let mut occurrences = 0;
    while let Some(relative) = inner[offset..].find('<') {
        let start = offset + relative;
        if stack.is_empty() && !inner[offset..start].trim().is_empty() {
            return Field::Malformed;
        }
        let Some(end_relative) = inner[start..].find('>') else {
            return Field::Malformed;
        };
        let end = start + end_relative;
        let tag = &inner[start + 1..end];
        let closing = tag.starts_with('/');
        let empty = !closing && tag.ends_with('/');
        let name = if closing {
            &tag[1..]
        } else if empty {
            &tag[..tag.len() - 1]
        } else {
            tag
        };
        // Exact plain tags only: do not find apparent fields inside attributes,
        // comments, CDATA, namespace aliases or an entity declaration.
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            || !name.as_bytes()[0].is_ascii_alphabetic()
        {
            return Field::Malformed;
        }
        if closing {
            let Some((opened, content_start)) = stack.pop() else {
                return Field::Malformed;
            };
            if opened != name {
                return Field::Malformed;
            }
            if name == wanted {
                let value = &inner[content_start..start];
                found = if occurrences == 1 && stack.is_empty() && !value.contains('<') {
                    Field::Value(value)
                } else {
                    Field::Malformed
                };
            }
        } else {
            if name == wanted {
                occurrences += 1;
                if occurrences != 1 || !stack.is_empty() || empty {
                    found = Field::Malformed;
                }
            }
            if !empty {
                if stack.len() >= MAX_XML_DEPTH {
                    return Field::Malformed;
                }
                stack.push((name, end + 1));
            }
        }
        offset = end + 1;
    }
    if !stack.is_empty() || !inner[offset..].trim().is_empty() {
        return Field::Malformed;
    }
    if occurrences > 1 {
        Field::Malformed
    } else {
        found
    }
}

/// Return unique scalar text for an in-memory comparison. Entity text remains
/// encoded so the caller can normalize only the representations it supports.
/// This value must never be included in diagnostics or logs.
pub(super) fn unique_text<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    match field(body, tag) {
        Field::Value(value) => Some(value),
        _ => None,
    }
}

/// Only static allowlisted names may be displayed, never a server-supplied code.
pub(super) fn error_code(body: &str) -> &'static str {
    match field(body, "Code") {
        Field::Value("SignatureDoesNotMatch") => "SignatureDoesNotMatch",
        Field::Value("AccessDenied") => "AccessDenied",
        Field::Value("InvalidAccessKeyId") => "InvalidAccessKeyId",
        Field::Value("InvalidSecurity") => "InvalidSecurity",
        Field::Value("InvalidToken") => "InvalidToken",
        Field::Value("ExpiredToken") => "ExpiredToken",
        Field::Value("TokenRefreshRequired") => "TokenRefreshRequired",
        Field::Value("AuthorizationHeaderMalformed") => "AuthorizationHeaderMalformed",
        Field::Value("AuthorizationQueryParametersError") => "AuthorizationQueryParametersError",
        Field::Value("RequestTimeTooSkewed") => "RequestTimeTooSkewed",
        Field::Value("RequestExpired") => "RequestExpired",
        Field::Value("InvalidRequest") => "InvalidRequest",
        Field::Value("InvalidArgument") => "InvalidArgument",
        Field::Value("InvalidDigest") => "InvalidDigest",
        Field::Value("BadDigest") => "BadDigest",
        Field::Value("NoSuchBucket") => "NoSuchBucket",
        Field::Value("NoSuchKey") => "NoSuchKey",
        Field::Value("NoSuchUpload") => "NoSuchUpload",
        Field::Value("NotImplemented") => "NotImplemented",
        Field::Value("MethodNotAllowed") => "MethodNotAllowed",
        Field::Value("PreconditionFailed") => "PreconditionFailed",
        Field::Value("PermanentRedirect") => "PermanentRedirect",
        Field::Value("TemporaryRedirect") => "TemporaryRedirect",
        Field::Value("SlowDown") => "SlowDown",
        Field::Value("ServiceUnavailable") => "ServiceUnavailable",
        Field::Value("InternalError") => "InternalError",
        _ => "Unknown",
    }
}

fn valid_signature(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_access_key_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn comparison(value: Field<'_>, expected: Option<&str>, valid: fn(&str) -> bool) -> &'static str {
    match value {
        Field::Absent => "absent",
        Field::Value(actual) if valid(actual) => match expected.filter(|value| valid(value)) {
            Some(expected) if actual == expected => "match",
            Some(_) => "mismatch",
            None => "malformed",
        },
        _ => "malformed",
    }
}

fn string_to_sign_bytes_comparison(value: Field<'_>, expected: &str) -> &'static str {
    let value = match value {
        Field::Absent => return "absent",
        Field::Malformed => return "malformed",
        Field::Value(value) => value,
    };
    if value.len() > MAX_STRING_TO_SIGN_BYTES * 3 || expected.is_empty() {
        return "malformed";
    }
    let mut count = 0;
    let mut matches = true;
    for token in value.split_ascii_whitespace() {
        if token.len() != 2 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return "malformed";
        }
        let Ok(byte) = u8::from_str_radix(token, 16) else {
            return "malformed";
        };
        if count >= MAX_STRING_TO_SIGN_BYTES {
            return "malformed";
        }
        matches &= expected.as_bytes().get(count) == Some(&byte);
        count += 1;
    }
    if count == 0 {
        "malformed"
    } else if matches && count == expected.len() {
        "match"
    } else {
        "mismatch"
    }
}

/// Uses only the AWS documentation's public fake credentials and fixed inputs.
/// It never accesses the real request's credentials or performs network I/O.
fn aws_public_signing_self_test() -> bool {
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(|| {
        // Same official GET /test.txt fixture as s3_sigv4's published-example
        // regression test. The expected signature is independently published:
        // https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sig-v4-header-based-auth.html
        let credentials = Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        };
        let payload = s3_sigv4::payload_sha256(b"");
        let signed = s3_sigv4::sign(
            &RequestToSign {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/test.txt",
                query: "",
                headers: &[("range", "bytes=0-9")],
                payload_sha256: &payload,
                amz_date: "20130524T000000Z",
            },
            &credentials,
            "us-east-1",
            "s3",
        );
        signed.authorization == concat!(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, ",
            "SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, ",
            "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        )
    })
}

/// Only categorical evidence is returned. No response content, signatures,
/// identifiers, hashes, lengths or decoded byte values can reach the output.
pub(super) fn signature_context(body: &str, signed: &SignedHeaders) -> String {
    if error_code(body) != "SignatureDoesNotMatch" {
        return String::new();
    }
    let signature = signed
        .authorization
        .rsplit_once(", Signature=")
        .map(|(_, value)| value);
    let access_key_id = signed
        .authorization
        .strip_prefix("AWS4-HMAC-SHA256 Credential=")
        .and_then(|value| value.split_once('/'))
        .map(|(value, _)| value);
    let signature = comparison(field(body, "SignatureProvided"), signature, valid_signature);
    let access_key_id = comparison(
        field(body, "AWSAccessKeyId"),
        access_key_id,
        valid_access_key_id,
    );
    let bytes =
        string_to_sign_bytes_comparison(field(body, "StringToSignBytes"), &signed.string_to_sign);
    let self_test = if aws_public_signing_self_test() {
        "pass"
    } else {
        "fail"
    };
    format!(
        " [narou: server SignatureProvided={signature}; server AWSAccessKeyId={access_key_id}; \
         server StringToSignBytes={bytes}; public AWS signing self-test={self_test}]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed() -> SignedHeaders {
        SignedHeaders {
            authorization: format!(
                "AWS4-HMAC-SHA256 Credential=PRIVATE_ID_SENTINEL/scope, SignedHeaders=host, Signature={}",
                "ab".repeat(32)
            ),
            x_amz_date: "PRIVATE_DATE_SENTINEL".to_string(),
            x_amz_content_sha256: "PRIVATE_HASH_SENTINEL".to_string(),
            canonical_request: "PRIVATE_REQUEST_SENTINEL".to_string(),
            string_to_sign: "PRIVATE_STRING_TO_SIGN_SENTINEL".to_string(),
        }
    }

    fn envelope(fields: &str) -> String {
        format!("<Error><Code>SignatureDoesNotMatch</Code>{fields}</Error>")
    }

    #[test]
    fn error_codes_are_static_allowlisted_and_require_unique_plain_fields() {
        for code in [
            "SignatureDoesNotMatch",
            "AccessDenied",
            "InvalidAccessKeyId",
            "SlowDown",
            "NoSuchKey",
        ] {
            assert_eq!(
                error_code(&format!("<Error><Code>{code}</Code></Error>")),
                code
            );
        }
        for body in [
            "<Error><Code>PRIVATE_RESPONSE_SENTINEL</Code></Error>",
            "<Error><Code>AccessDenied</Code><Code>AccessDenied</Code></Error>",
            "<Error><Code>AccessDenied</Code><Code>PRIVATE_RESPONSE_SENTINEL</Code></Error>",
            "<Error><Code><nested>AccessDenied</nested></Code></Error>",
            "<Error><Other><Code>AccessDenied</Code></Other></Error>",
            "<Error><Code> AccessDenied </Code></Error>",
            "<Error><Code>Access&#68;enied</Code></Error>",
            "<Error/>",
        ] {
            assert_eq!(error_code(body), "Unknown");
        }
    }

    #[test]
    fn unique_text_preserves_entities_but_rejects_ambiguous_structure() {
        let body = envelope("<StringToSign>one&#10;two&#xA;three\\nfour</StringToSign>");
        assert_eq!(
            unique_text(&body, "StringToSign"),
            Some("one&#10;two&#xA;three\\nfour")
        );
        for fragment in [
            "<StringToSign>one</StringToSign><StringToSign>one</StringToSign>",
            "<StringToSign><Nested>one</Nested></StringToSign>",
            "<Other><StringToSign>one</StringToSign></Other>",
            "<StringToSign>one</Other>",
            "<StringToSign attribute=\"one\">one</StringToSign>",
            "<!-- <StringToSign>one</StringToSign> -->",
        ] {
            assert_eq!(unique_text(&envelope(fragment), "StringToSign"), None);
        }
        let too_deep = format!(
            "{}<StringToSign>one</StringToSign>{}",
            "<Other>".repeat(MAX_XML_DEPTH),
            "</Other>".repeat(MAX_XML_DEPTH)
        );
        assert_eq!(unique_text(&envelope(&too_deep), "StringToSign"), None);
    }

    #[test]
    fn absent_echoes_are_explicit_and_public_self_test_passes() {
        assert_eq!(
            signature_context(&envelope(""), &signed()),
            " [narou: server SignatureProvided=absent; server AWSAccessKeyId=absent; server StringToSignBytes=absent; public AWS signing self-test=pass]"
        );
        assert!(aws_public_signing_self_test());
        assert!(aws_public_signing_self_test());
    }

    #[test]
    fn valid_echoes_compare_only_in_memory() {
        let signed = signed();
        let bytes = signed
            .string_to_sign
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let body = envelope(&format!(
            "<SignatureProvided>{}</SignatureProvided><AWSAccessKeyId>PRIVATE_ID_SENTINEL</AWSAccessKeyId><StringToSignBytes>{bytes}</StringToSignBytes>",
            "ab".repeat(32)
        ));
        let context = signature_context(&body, &signed);
        assert!(context.contains("SignatureProvided=match"));
        assert!(context.contains("AWSAccessKeyId=match"));
        assert!(context.contains("StringToSignBytes=match"));
        assert!(!context.contains("PRIVATE_"));
        assert!(!context.contains(&"ab".repeat(32)));
        assert!(!context.contains(&bytes));

        let context = signature_context(
            &envelope(&format!(
                "<SignatureProvided>{}</SignatureProvided><AWSAccessKeyId>OTHER_PRIVATE_ID</AWSAccessKeyId><StringToSignBytes>00</StringToSignBytes>",
                "cd".repeat(32)
            )),
            &signed,
        );
        assert!(context.contains("SignatureProvided=mismatch"));
        assert!(context.contains("AWSAccessKeyId=mismatch"));
        assert!(context.contains("StringToSignBytes=mismatch"));
        assert!(!context.contains("PRIVATE_"));
    }

    #[test]
    fn duplicate_empty_nested_or_invalid_echoes_are_malformed() {
        for (tag, value) in [
            ("SignatureProvided", "PRIVATE_SIGNATURE_SENTINEL"),
            ("AWSAccessKeyId", "PRIVATE ID SENTINEL"),
            ("StringToSignBytes", "50 52 zz"),
        ] {
            for fragment in [
                format!("<{tag}>{value}</{tag}>"),
                format!("<{tag}></{tag}>"),
                format!("<{tag}/>"),
                format!("<{tag}><Nested>{value}</Nested></{tag}>"),
                format!("<Other><{tag}>{value}</{tag}></Other>"),
                format!("<{tag}>{value}</{tag}><{tag}>{value}</{tag}>"),
                format!("<{tag}>{value}</{tag}><{tag}/>"),
                format!("<{tag}/><{tag}>{value}</{tag}>"),
                format!("<{tag}>PRIVATE&amp;SENTINEL</{tag}>"),
            ] {
                let context = signature_context(&envelope(&fragment), &signed());
                assert!(context.contains(&format!("{tag}=malformed")), "{tag}");
                assert!(!context.contains("PRIVATE"));
            }
        }
    }

    #[test]
    fn malformed_xml_and_hostile_fragments_never_produce_evidence() {
        for body in [
            "<Error><Code>SignatureDoesNotMatch</Code><AWSAccessKeyId>PRIVATE_ID</Error>",
            "<Error><Code>SignatureDoesNotMatch</Code><AWSAccessKeyId attribute=\"PRIVATE_ID\">a</AWSAccessKeyId></Error>",
            "<Error><!-- <Code>SignatureDoesNotMatch</Code> --></Error>",
            "<Error><Message><![CDATA[<Code>SignatureDoesNotMatch</Code>]]></Message></Error>",
            "<!DOCTYPE Error [<!ENTITY x 'PRIVATE_ID'>]><Error><Code>SignatureDoesNotMatch</Code></Error>",
            "<Error><Code>SignatureDoesNotMatch</Code><x:AWSAccessKeyId>PRIVATE_ID</x:AWSAccessKeyId></Error>",
            "<Error><Code>SignatureDoesNotMatch</Code></Error><Error></Error>",
            "garbage<Error><Code>SignatureDoesNotMatch</Code></Error>",
            "<Error><Code>SignatureDoesNotMatch</Code><Message>PRIVATE\0SENTINEL</Message></Error>",
        ] {
            assert_eq!(error_code(body), "Unknown");
            assert!(signature_context(body, &signed()).is_empty());
        }
        let oversized = envelope(&format!(
            "<Message>{}</Message>",
            "x".repeat(MAX_ERROR_XML_BYTES)
        ));
        assert_eq!(error_code(&oversized), "Unknown");
        assert!(signature_context(&oversized, &signed()).is_empty());
    }

    #[test]
    fn strict_hex_byte_comparison_rejects_non_bytes_and_excess_size() {
        assert_eq!(
            string_to_sign_bytes_comparison(Field::Absent, "A"),
            "absent"
        );
        assert_eq!(
            string_to_sign_bytes_comparison(Field::Malformed, "A"),
            "malformed"
        );
        assert_eq!(
            string_to_sign_bytes_comparison(Field::Value("41 0A\r\n42"), "A\nB"),
            "match"
        );
        assert_eq!(
            string_to_sign_bytes_comparison(Field::Value("41"), "B"),
            "mismatch"
        );
        assert_eq!(
            string_to_sign_bytes_comparison(Field::Value("41 42"), "A"),
            "mismatch"
        );
        for value in [
            "",
            " ",
            "4",
            "041",
            "0x41",
            "4142",
            "41,42",
            "gg",
            "41\u{a0}42",
        ] {
            assert_eq!(
                string_to_sign_bytes_comparison(Field::Value(value), "A"),
                "malformed"
            );
        }
        let oversized = "41 ".repeat(MAX_STRING_TO_SIGN_BYTES + 1);
        assert_eq!(
            string_to_sign_bytes_comparison(Field::Value(&oversized), "A"),
            "malformed"
        );
    }

    #[test]
    fn non_signature_errors_do_not_run_or_report_the_self_test() {
        for body in [
            "<Error><Code>AccessDenied</Code><Message>SignatureDoesNotMatch</Message></Error>",
            "<Error><Code>InvalidAccessKeyId</Code></Error>",
            "<Error><Code>PRIVATE_RESPONSE_SENTINEL</Code></Error>",
        ] {
            assert!(signature_context(body, &signed()).is_empty());
        }
    }

    #[test]
    fn usual_xml_prolog_is_supported_and_bad_local_authorization_is_inconclusive() {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{}",
            envelope(
                "<SignatureProvided>abababababababababababababababababababababababababababababababab</SignatureProvided><AWSAccessKeyId>PRIVATE_ID_SENTINEL</AWSAccessKeyId>"
            )
        );
        let mut signed = signed();
        signed.authorization = "PRIVATE_MALFORMED_AUTHORIZATION".to_string();
        let context = signature_context(&body, &signed);
        assert!(context.contains("SignatureProvided=malformed"));
        assert!(context.contains("AWSAccessKeyId=malformed"));
        assert!(!context.contains("PRIVATE"));
    }
}
