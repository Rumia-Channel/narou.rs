//! End-to-end tests for `narou_rs::mail`.
//!
//! These tests stand up a minimal SMTP listener on `127.0.0.1:0` so that
//! `send_mail` can exercise the real `lettre` SMTP transport stack without any
//! network dependency. The plain SMTP path is selected by the same explicit
//! opt-in that production users have to set (`allow_insecure: true` +
//! `enable_starttls_auto: false`); TLS handshakes are deliberately out of
//! scope because they require a self-signed certificate fixture that does not
//! add coverage beyond what unit tests already provide.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use mailparse::{parse_mail, MailHeaderMap, ParsedMail};
use narou_rs::mail::{send_mail, MailSetting};

/// Greets the client and reflects every essential SMTP command back as
/// `250 OK`. Stores the DATA payload in the provided channel.
fn run_smtp_session(stream: TcpStream, tx: Sender<String>) {
    let read_stream = match stream.try_clone() {
        Ok(handle) => handle,
        Err(_) => return,
    };
    let mut reader = BufReader::new(read_stream);
    let mut writer = stream;

    let _ = writer.write_all(b"220 narou-rs-stub ESMTP ready\r\n");

    let mut data_buf: Vec<u8> = Vec::new();
    let mut in_data = false;
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }

        if in_data {
            if line == "." {
                in_data = false;
                let body = String::from_utf8_lossy(&data_buf).to_string();
                let _ = tx.send(body);
                data_buf.clear();
                let _ = writer.write_all(b"250 OK\r\n");
            } else {
                // SMTP transparency: a leading "." is escaped as ".."
                let stripped = line.strip_prefix('.').unwrap_or(&line);
                data_buf.extend_from_slice(stripped.as_bytes());
                data_buf.extend_from_slice(b"\r\n");
            }
            continue;
        }

        let upper = line.to_ascii_uppercase();
        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            let _ = writer.write_all(b"250-narou-rs-stub\r\n250 OK\r\n");
        } else if upper.starts_with("MAIL FROM") {
            let _ = writer.write_all(b"250 OK\r\n");
        } else if upper.starts_with("RCPT TO") {
            let _ = writer.write_all(b"250 OK\r\n");
        } else if upper.starts_with("DATA") {
            in_data = true;
            let _ = writer.write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n");
        } else if upper.starts_with("QUIT") {
            let _ = writer.write_all(b"221 Bye\r\n");
            break;
        } else if upper.starts_with("RSET") {
            let _ = writer.write_all(b"250 OK\r\n");
        } else if upper.starts_with("NOOP") {
            let _ = writer.write_all(b"250 OK\r\n");
        } else {
            // STARTTLS / AUTH / etc. We do not implement them; `lettre` is
            // expected to obey the configured `SmtpTlsMode` and never send
            // these commands in this test setup.
            let _ = writer.write_all(b"502 Command not implemented\r\n");
        }
    }
}

struct StubSmtp {
    port: u16,
    received: Receiver<String>,
}

fn start_stub_smtp() -> StubSmtp {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local_addr").port();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        // One transaction per listener keeps the assertions deterministic.
        if let Ok((stream, _)) = listener.accept() {
            run_smtp_session(stream, tx);
        }
        // Drop the listener so the next StubSmtp can bind a fresh port
        // immediately, even on platforms with TIME_WAIT quirks.
        drop(listener);
    });

    StubSmtp { port, received: rx }
}

/// Builds a `MailSetting` configured for plain SMTP against the given port.
/// Without `allow_insecure: true` and `enable_starttls_auto: false` the
/// default policy refuses to deliver over plaintext, so these fields are
/// mandatory for tests that want to speak to a non-TLS listener.
fn plain_smtp_setting(port: u16, extras: HashMap<String, serde_yaml::Value>) -> MailSetting {
    let mut via_options = HashMap::new();
    via_options.insert(
        "address".to_string(),
        serde_yaml::Value::String("127.0.0.1".to_string()),
    );
    via_options.insert(
        "port".to_string(),
        serde_yaml::Value::Number(serde_yaml::Number::from(port)),
    );
    via_options.insert(
        "enable_starttls_auto".to_string(),
        serde_yaml::Value::Bool(false),
    );
    via_options.insert(
        "allow_insecure".to_string(),
        serde_yaml::Value::Bool(true),
    );

    MailSetting {
        from: "sender@example.com".to_string(),
        to: "receiver@example.com".to_string(),
        subject: "テスト件名".to_string(),
        via: "smtp".to_string(),
        via_options,
        extras,
    }
}

fn find_attachment<'a>(parsed: &'a ParsedMail<'a>) -> Option<&'a ParsedMail<'a>> {
    parsed
        .subparts
        .iter()
        .find(|part| part.ctype.mimetype.eq_ignore_ascii_case("application/epub+zip"))
}

/// Returns the raw header text for `name` on the given parsed mail,
/// concatenating the value across multi-value headers so we can search for
/// filename markers regardless of RFC 2231 / RFC 5987 folding.
fn header_text(parsed: &ParsedMail<'_>, name: &str) -> String {
    parsed
        .headers
        .get_all_values(name)
        .into_iter()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Percent-encodes a UTF-8 string so callers can detect the RFC 2231
/// `filename*=utf-8''...` form that `lettre` emits for non-ASCII filenames.
fn percent_encode_utf8(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.as_bytes() {
        out.push_str(&format!("%{:02X}", byte));
    }
    out
}

#[test]
fn send_mail_delivers_message_via_plain_smtp() {
    let stub = start_stub_smtp();

    let tmp = tempfile::tempdir().expect("tempdir");
    let attachment = tmp.path().join("sample.epub");
    std::fs::write(&attachment, b"PK\x03\x04 epub-fixture").expect("write attachment");

    let setting = plain_smtp_setting(stub.port, HashMap::new());

    send_mail(&setting, "1", "本文テキストテスト", &attachment).expect("send_mail");

    let raw = stub
        .received
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("smtp data payload");

    let parsed = parse_mail(raw.as_bytes()).expect("parse_mail");
    assert_eq!(
        parsed.headers.get_first_value("From").as_deref(),
        Some("sender@example.com")
    );
    assert_eq!(
        parsed.headers.get_first_value("To").as_deref(),
        Some("receiver@example.com")
    );
    assert_eq!(
        parsed.headers.get_first_value("Subject").as_deref(),
        Some("テスト件名")
    );

    let body_part = parsed
        .subparts
        .iter()
        .find(|part| part.ctype.mimetype.eq_ignore_ascii_case("text/plain"))
        .expect("text/plain part");
    let body = body_part.get_body_raw().expect("decode body");
    assert_eq!(std::str::from_utf8(body.as_slice()).unwrap(), "本文テキストテスト");

    let attachment_part = find_attachment(&parsed).expect("attachment part");
    let disposition = header_text(attachment_part, "Content-Disposition");
    assert!(
        disposition.contains("sample.epub"),
        "Content-Disposition should embed original filename: {disposition}"
    );
    assert_eq!(
        attachment_part.get_body_raw().expect("decode attachment"),
        b"PK\x03\x04 epub-fixture".to_vec()
    );
}

#[test]
fn send_mail_keeps_original_filename_for_attachments() {
    let stub = start_stub_smtp();

    let tmp = tempfile::tempdir().expect("tempdir");
    let attachment = tmp.path().join("明治雪華.kepub.epub");
    std::fs::write(&attachment, b"PK\x03\x04 kepub-fixture").expect("write attachment");

    let setting = plain_smtp_setting(stub.port, HashMap::new());

    send_mail(&setting, "1", "body", &attachment).expect("send_mail");

    let raw = stub
        .received
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("smtp data payload");

    let parsed = parse_mail(raw.as_bytes()).expect("parse_mail");
    let attachment_part = find_attachment(&parsed).expect("application/epub+zip part");

    let disposition = header_text(attachment_part, "Content-Disposition");
    let content_type = header_text(attachment_part, "Content-Type");
    let combined = format!("{disposition}\n{content_type}");

    let encoded = percent_encode_utf8("明治雪華");
    assert!(
        combined.contains(&encoded),
        "RFC 2231 / RFC 5987 percent-encoded UTF-8 of original filename missing\n--- disposition ---\n{disposition}\n--- content-type ---\n{content_type}\nexpected substring: {encoded}"
    );

    assert_eq!(
        attachment_part.get_body_raw().expect("decode attachment"),
        b"PK\x03\x04 kepub-fixture".to_vec()
    );
}

#[test]
fn send_mail_splits_multiple_recipients() {
    let stub = start_stub_smtp();

    let tmp = tempfile::tempdir().expect("tempdir");
    let attachment = tmp.path().join("multi.epub");
    std::fs::write(&attachment, b"PK\x03\x04 multi").expect("write attachment");

    let mut extras = HashMap::new();
    extras.insert(
        "cc".to_string(),
        serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("cc1@example.com".to_string()),
            serde_yaml::Value::String("cc2@example.com".to_string()),
        ]),
    );

    let mut setting = plain_smtp_setting(stub.port, extras);
    setting.to = "to1@example.com, to2@example.com".to_string();

    send_mail(&setting, "1", "body", &attachment).expect("send_mail");

    let raw = stub
        .received
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("smtp data payload");

    let parsed = parse_mail(raw.as_bytes()).expect("parse_mail");
    let to_header = parsed
        .headers
        .get_first_value("To")
        .unwrap_or_default();
    assert!(to_header.contains("to1@example.com"));
    assert!(to_header.contains("to2@example.com"));

    let cc_header = parsed
        .headers
        .get_first_value("Cc")
        .unwrap_or_default();
    assert!(cc_header.contains("cc1@example.com"));
    assert!(cc_header.contains("cc2@example.com"));
}

#[test]
fn send_mail_rejects_plain_smtp_without_explicit_opt_in() {
    // No listener is started: the test exercises the default rejection policy
    // that protects users from accidentally enabling plaintext SMTP.
    let tmp = tempfile::tempdir().expect("tempdir");
    let attachment = tmp.path().join("sample.epub");
    std::fs::write(&attachment, b"dummy").expect("write attachment");

    let mut via_options = HashMap::new();
    via_options.insert(
        "address".to_string(),
        serde_yaml::Value::String("127.0.0.1".to_string()),
    );
    via_options.insert(
        "port".to_string(),
        serde_yaml::Value::Number(serde_yaml::Number::from(1)),
    );
    via_options.insert(
        "enable_starttls_auto".to_string(),
        serde_yaml::Value::Bool(false),
    );
    // Note: allow_insecure is intentionally omitted.

    let setting = MailSetting {
        from: "sender@example.com".to_string(),
        to: "receiver@example.com".to_string(),
        subject: "subject".to_string(),
        via: "smtp".to_string(),
        via_options,
        extras: HashMap::new(),
    };

    let err = send_mail(&setting, "1", "body", &attachment).unwrap_err();
    assert!(
        err.contains("安全でない SMTP 設定")
            || err.contains("allow_insecure"),
        "expected explicit-opt-in error, got: {err}"
    );
}
