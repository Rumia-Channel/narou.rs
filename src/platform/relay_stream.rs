//! Metadata-first relay framing. Only the bounded envelope is materialized.
use futures::{StreamExt, stream};
use serde::Deserialize;

use crate::platform::http::{HttpStreamResponse, is_safe_header_value};
use crate::platform::object_store::AssetStream;
use crate::platform::relay::MAX_RELAY_METADATA;
use crate::error::{NarouError, Result};

pub const STREAM_CONTENT_TYPE: &str = "application/vnd.narou.relay.v1+octet-stream";
const MAGIC: &[u8; 4] = b"NRS1";
const CHUNK_BYTES: usize = 64 * 1024;
// The relay transport has always limited decoded upstream bodies to 16 MiB.
const MAX_RELAY_BODY: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    status: u16,
    content_type: Option<String>,
    location: Option<String>,
    #[serde(default)]
    set_cookie: Vec<String>,
    via: Option<String>,
    body_length: u64,
}

fn invalid(message: &str) -> NarouError {
    NarouError::Http(message.to_owned())
}

struct Input {
    stream: AssetStream,
    chunk: Vec<u8>,
    offset: usize,
}

impl Input {
    async fn refill(&mut self) -> Result<bool> {
        while self.offset == self.chunk.len() {
            self.chunk = match self.stream.next().await {
                Some(chunk) => chunk?,
                None => return Ok(false),
            };
            self.offset = 0;
        }
        Ok(true)
    }

    async fn read_exact(&mut self, output: &mut [u8]) -> Result<()> {
        let mut position = 0;
        while position < output.len() {
            if !self.refill().await? {
                return Err(invalid("relay stream metadata is truncated"));
            }
            let count = (output.len() - position).min(self.chunk.len() - self.offset);
            output[position..position + count].copy_from_slice(&self.chunk[self.offset..self.offset + count]);
            position += count;
            self.offset += count;
        }
        Ok(())
    }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.refill().await? {
            return Ok(None);
        }
        let count = (self.chunk.len() - self.offset).min(CHUNK_BYTES);
        if count == self.chunk.len() - self.offset {
            if self.offset != 0 {
                self.chunk.drain(..self.offset);
            }
            self.offset = 0;
            Ok(Some(std::mem::take(&mut self.chunk)))
        } else {
            let chunk = self.chunk[self.offset..self.offset + count].to_vec();
            self.offset += count;
            Ok(Some(chunk))
        }
    }
}

/// Decode a negotiated relay response without collecting its binary body.
///
/// The caller supplies the original upstream URL in `response.url`. A legacy
/// JSON reply requires a relay update: its `body` can precede its status and
/// cookies, so pretending it is a stream would merely move buffering elsewhere.
pub async fn decode_relay_stream(response: HttpStreamResponse) -> Result<HttpStreamResponse> {
    if response.status != 200 {
        return Err(NarouError::Http(format!("relay stream endpoint returned HTTP {}", response.status)));
    }
    let streaming = response.headers.iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .is_some_and(|(_, value)| value.split(';').next().unwrap_or_default().trim().eq_ignore_ascii_case(STREAM_CONTENT_TYPE));
    if !streaming {
        return Err(invalid("relay does not support streaming responses; update narou_relay"));
    }
    let mut input = Input { stream: response.body, chunk: Vec::new(), offset: 0 };
    let mut prefix = [0; 8];
    input.read_exact(&mut prefix).await?;
    if &prefix[..4] != MAGIC {
        return Err(invalid("invalid relay stream framing"));
    }
    let length = u32::from_be_bytes(prefix[4..].try_into().expect("four-byte frame length")) as usize;
    if length == 0 || length > MAX_RELAY_METADATA {
        return Err(invalid("relay stream metadata exceeds limit"));
    }
    let mut encoded = vec![0; length];
    input.read_exact(&mut encoded).await?;
    let metadata: Metadata = serde_json::from_slice(&encoded)
        .map_err(|_| invalid("invalid relay stream metadata"))?;
    if metadata.body_length > MAX_RELAY_BODY {
        return Err(invalid("relay stream body exceeds limit"));
    }
    let mut headers = Vec::with_capacity(metadata.set_cookie.len() + 4);
    for (name, value) in [
        ("content-type", metadata.content_type),
        ("location", metadata.location),
        ("x-narou-relay-via", metadata.via),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            if !is_safe_header_value(&value) {
                return Err(invalid("invalid relay stream header"));
            }
            headers.push((name.to_owned(), value));
        }
    }
    for value in metadata.set_cookie {
        if !is_safe_header_value(&value) {
            return Err(invalid("invalid relay stream cookie header"));
        }
        headers.push(("set-cookie".to_owned(), value));
    }
    headers.push(("content-length".to_owned(), metadata.body_length.to_string()));
    let body = stream::try_unfold((input, metadata.body_length), |(mut input, remaining)| async move {
        match input.next_chunk().await? {
            Some(chunk) if chunk.len() as u64 <= remaining => {
                let remaining = remaining - chunk.len() as u64;
                Ok(Some((chunk, (input, remaining))))
            }
            Some(_) => Err(invalid("relay stream body exceeds declared length")),
            None if remaining == 0 => Ok(None),
            None => Err(invalid("relay stream body is truncated")),
        }
    });
    Ok(HttpStreamResponse { status: metadata.status, url: response.url, headers, body: Box::pin(body) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

    fn prefix(length: u64) -> Vec<u8> {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "status": 302, "contentType": "image/png", "location": "/next",
            "setCookie": ["sid=first", "pref=second"], "via": "curl", "bodyLength": length,
        })).unwrap();
        let mut bytes = Vec::from(MAGIC.as_slice());
        bytes.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&metadata);
        bytes
    }

    fn response(chunks: Vec<Vec<u8>>, pulls: Arc<AtomicUsize>) -> HttpStreamResponse {
        HttpStreamResponse {
            status: 200,
            url: "https://source.example/image".into(),
            headers: vec![("Content-Type".into(), STREAM_CONTENT_TYPE.into())],
            body: Box::pin(stream::iter(chunks.into_iter().map(move |chunk| {
                pulls.fetch_add(1, Ordering::SeqCst);
                Ok(chunk)
            }))),
        }
    }

    #[test]
    fn body_is_pulled_only_on_demand_and_drop_stops_upstream() {
        futures::executor::block_on(async {
            let pulls = Arc::new(AtomicUsize::new(0));
            let mut decoded = decode_relay_stream(response(vec![prefix(6), b"abc".to_vec(), b"def".to_vec()], pulls.clone())).await.unwrap();
            assert_eq!(pulls.load(Ordering::SeqCst), 1);
            assert_eq!(decoded.status, 302);
            assert_eq!(decoded.url, "https://source.example/image");
            assert_eq!(decoded.headers.iter().filter(|(name, _)| name == "set-cookie").map(|(_, value)| value.as_str()).collect::<Vec<_>>(), ["sid=first", "pref=second"]);
            assert_eq!(decoded.body.next().await.unwrap().unwrap(), b"abc");
            assert_eq!(pulls.load(Ordering::SeqCst), 2);
            drop(decoded);
            assert_eq!(pulls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn arbitrary_wire_boundaries_preserve_payload_and_finish_at_eof() {
        futures::executor::block_on(async {
            let mut wire = prefix(5);
            wire.extend_from_slice(&[0, 255, 128, b'\n', 42]);
            let input = response(wire.into_iter().map(|byte| vec![byte]).collect(), Arc::default());
            let mut output = decode_relay_stream(input).await.unwrap();
            let mut body = Vec::new();
            while let Some(chunk) = output.body.next().await { body.extend(chunk.unwrap()); }
            assert_eq!(body, [0, 255, 128, b'\n', 42]);
        });
    }

    #[test]
    fn truncation_and_surplus_never_finish_successfully() {
        futures::executor::block_on(async {
            for (declared, body) in [(4, b"abc".to_vec()), (2, b"abc".to_vec())] {
                let mut output = decode_relay_stream(response(vec![prefix(declared), body], Arc::default())).await.unwrap();
                let mut failed = false;
                while let Some(chunk) = output.body.next().await {
                    if chunk.is_err() { failed = true; break; }
                }
                assert!(failed);
            }
        });
    }

    #[test]
    fn oversized_envelope_and_legacy_json_are_rejected_before_body_pulls() {
        futures::executor::block_on(async {
            let pulls = Arc::new(AtomicUsize::new(0));
            let mut huge = Vec::from(MAGIC.as_slice());
            huge.extend_from_slice(&((MAX_RELAY_METADATA + 1) as u32).to_be_bytes());
            assert!(decode_relay_stream(response(vec![huge, vec![0; 32]], pulls.clone())).await.is_err());
            assert_eq!(pulls.load(Ordering::SeqCst), 1);
            let mut old = response(vec![b"{\"body\":\"".to_vec()], pulls.clone());
            old.headers[0].1 = "application/json".into();
            assert!(decode_relay_stream(old).await.is_err());
            assert_eq!(pulls.load(Ordering::SeqCst), 1);
        });
    }
}
