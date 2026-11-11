//! Bounded transport, multipart publication and durable abandoned-work journal.

use std::ops::Range;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use super::*;
use crate::platform::{http::HttpStreamResponse, staging::{STAGE_TTL_SECONDS, is_staging_key, new_staging_key, validate_range}};

const PART_BYTES: usize = 8 * 1024 * 1024;
const JOURNAL_PREFIX: &str = ".narou-stage-journal/";
const XML_LIMIT: usize = 1024 * 1024;

#[path = "s3_stage_lease.rs"]
mod lease;

#[derive(Serialize, Deserialize)]
struct Reservation {
    key: String,
    upload_id: Option<String>,
    expires_at: i64,
    #[serde(default)]
    reclaiming: bool,
}

fn journal_key(key: &ObjectKey) -> Result<ObjectKey> {
    ObjectKey::try_new(format!("{JOURNAL_PREFIX}{}", key.as_ref().strip_prefix(crate::platform::staging::STAGING_PREFIX)
        .ok_or_else(|| NarouError::Platform("journal target is not a stage".into()))?))
}

fn encode_query(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>().replace('+', "%20")
}

fn xml_text(body: &[u8], field: &str) -> Result<String> {
    let xml = std::str::from_utf8(body).map_err(|_| NarouError::Platform("S3 returned non-UTF8 XML".into()))?;
    if super::super::s3_request::find_tag(xml, "Error").is_some() {
        return Err(NarouError::Platform("S3 operation returned Error XML".into()));
    }
    super::super::s3_request::element_text(xml, field).filter(|value| !value.is_empty())
        .ok_or_else(|| NarouError::Platform(format!("S3 response missing {field}")))
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn require_status(response: &super::super::http::HttpResponse, allowed: &[u16], operation: &str) -> Result<()> {
    if !allowed.contains(&response.status) {
        return Err(NarouError::Platform(format!("S3 {operation} failed with status {}", response.status)));
    }
    if std::str::from_utf8(&response.body).is_ok_and(|xml| super::super::s3_request::find_tag(xml, "Error").is_some()) {
        return Err(NarouError::Platform(format!("S3 {operation} returned Error XML")));
    }
    Ok(())
}

impl S3Store {
    pub(super) async fn stream_get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpStreamResponse> {
        let (path, query) = url_target(url);
        let payload_sha256 = s3_sigv4::payload_sha256(&[]);
        let amz_date = s3_sigv4::amz_date(self.clock.now_utc());
        let signed = s3_sigv4::sign(&RequestToSign {
            method: "GET", host: &self.host, path: &path, query: &query,
            headers: &[], payload_sha256: &payload_sha256, amz_date: &amz_date,
        }, &self.credentials.borrowed(), &self.region, "s3");
        let mut request = HttpRequest::get(url).with_trusted_endpoint().with_redirect(RedirectMode::Manual)
            .with_header("x-amz-date", &signed.x_amz_date)
            .with_header("x-amz-content-sha256", &signed.x_amz_content_sha256)
            .with_header("authorization", &signed.authorization);
        for (name, value) in headers { request = request.with_header(*name, *value); }
        self.http.send_stream(request).await
    }

    pub(super) async fn stream_get_object(&self, key: &ObjectKey, headers: &[(&str, &str)]) -> Result<HttpStreamResponse> {
        let response = self.stream_get(&self.location.object_url(key), headers).await?;
        if response.status == 404 && self.location.has_pool_alternate(key) {
            drop(response);
            self.stream_get(&self.location.legacy_object_url(key), headers).await
        } else { Ok(response) }
    }

    pub(super) async fn stream_read(&self, key: &ObjectKey) -> Result<Option<AssetStream>> {
        self.renew_stage_progress(key).await?;
        let response = self.stream_get_object(key, &[]).await?;
        if response.status == 404 { return Ok(None); }
        if response.status != 200 { return Err(NarouError::Platform(format!("S3 GET failed with status {}", response.status))); }
        let expected = response.header("content-length").map(|value| value.parse::<u64>())
            .transpose().map_err(|_| NarouError::Platform("invalid S3 content-length".into()))?;
        let stream = exact_stream(response.body, expected);
        if !is_staging_key(key) { return Ok(Some(stream)); }
        let owner = self.clone();
        let key = key.clone();
        Ok(Some(Box::pin(futures::stream::try_unfold((owner, key, stream), |(owner, key, mut stream)| async move {
            match stream.next().await {
                Some(chunk) => {
                    let chunk = chunk?;
                    if !chunk.is_empty() { owner.renew_stage_progress(&key).await?; }
                    Ok(Some((chunk, (owner, key, stream))))
                }
                None => Ok(None),
            }
        }))))
    }

    pub(super) async fn range_read(&self, key: &ObjectKey, range: Range<u64>) -> Result<Option<Vec<u8>>> {
        let length = validate_range(&range)?;
        self.renew_stage_progress(key).await?;
        if length == 0 {
            let metadata = self.stat_object(key).await?;
            if metadata.as_ref().is_some_and(|metadata| range.end > metadata.size) {
                return Err(NarouError::Platform("asset range exceeds object".into()));
            }
            return Ok(metadata.map(|_| Vec::new()));
        }
        let header = format!("bytes={}-{}", range.start, range.end - 1);
        let response = self.stream_get_object(key, &[("range", &header)]).await?;
        if response.status == 404 { return Ok(None); }
        validate_content_range(&response, &range)?;
        let mut stream = exact_stream(response.body, Some(length as u64));
        let mut bytes = Vec::with_capacity(length);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if chunk.len() > length - bytes.len() { return Err(NarouError::Platform("S3 range overflow".into())); }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(bytes))
    }


    pub(super) async fn delete_stage_record(&self, key: &ObjectKey) -> Result<()> {
        if is_staging_key(key) { self.delete_object_raw(&journal_key(key)?).await?; }
        self.stage_leases.forget(key);
        Ok(())
    }

    async fn abort_upload(&self, key: &ObjectKey, upload_id: &str) -> Result<()> {
        let url = format!("{}?uploadId={}", self.location.object_url(key), encode_query(upload_id));
        let (response, _) = self.send(HttpMethod::Delete, &url, &[], None, Some(XML_LIMIT)).await?;
        require_status(&response, &[200, 204, 404], "abort multipart")
    }

    pub(super) async fn multipart_write(&self, destination: &ObjectKey, stream: AssetStream) -> Result<()> {
        // Native applications have no cron binding; each new asset operation
        // performs bounded recovery. Workers use the scheduled hook instead.
        #[cfg(all(feature = "native-runtime", not(target_arch = "wasm32")))]
        self.collect_expired_stages(16).await?;
        let temporary = if is_staging_key(destination) { destination.clone() } else { new_staging_key()? };
        self.reserve_stage(&temporary).await?;
        let result = self.upload_parts(&temporary, stream).await;
        if let Err(error) = result {
            // The reservation remains if abort/delete fails or the future is
            // cancelled. Reconciliation also covers a lost initiation reply.
            let _ = self.reclaim_stage(&temporary, false).await;
            return Err(error);
        }
        if temporary != *destination {
            self.copy_object(&temporary, destination).await?;
            self.delete_object(&temporary).await?;
        }
        Ok(())
    }

    async fn next_part(&self, key: &ObjectKey, stream: &mut AssetStream, pending: &mut Option<(Vec<u8>, usize)>) -> Result<(Vec<u8>, bool)> {
        let mut part = Vec::with_capacity(PART_BYTES);
        loop {
            let (chunk, offset) = match pending.take() {
                Some(value) => value,
                None => match stream.next().await {
                    Some(chunk) => (chunk?, 0),
                    None => return Ok((part, true)),
                },
            };
            let length = (PART_BYTES - part.len()).min(chunk.len() - offset);
            if length != 0 { self.renew_stage_progress(key).await?; }
            part.extend_from_slice(&chunk[offset..offset + length]);
            if offset + length < chunk.len() { *pending = Some((chunk, offset + length)); }
            if part.len() == PART_BYTES { return Ok((part, false)); }
        }
    }

    async fn upload_parts(&self, key: &ObjectKey, mut stream: AssetStream) -> Result<()> {
        let url = self.location.object_url(key);
        let content_type = content_type_for_key(key).unwrap_or("application/octet-stream");
        let mut pending = None;
        let (first, finished) = self.next_part(key, &mut stream, &mut pending).await?;
        // A single bounded part needs no multipart session. This also keeps
        // small section/image stages within the Worker's subrequest budget.
        if finished { return self.write_object(key, first).await; }
        let (response, _) = self.send(HttpMethod::Post, &format!("{url}?uploads="),
            &[("content-type", content_type)], None, Some(XML_LIMIT)).await?;
        require_status(&response, &[200], "initiate multipart")?;
        if !std::str::from_utf8(&response.body).is_ok_and(|xml| super::super::s3_request::xml_result_body(xml, "InitiateMultipartUploadResult").is_some()) {
            return Err(NarouError::Platform("invalid S3 multipart initiation document".into()));
        }
        let upload_id = xml_text(&response.body, "UploadId")?;
        self.save_reservation(key, Some(&upload_id)).await?;
        let result = async {
            let mut total = first.len() as u64;
            let mut etags = vec![self.upload_part(&url, &upload_id, 1, first).await?];
            loop {
                self.renew_stage_progress(key).await?;
                let (part, finished) = self.next_part(key, &mut stream, &mut pending).await?;
                if !part.is_empty() {
                    total = total.checked_add(part.len() as u64).ok_or_else(|| NarouError::Platform("S3 upload size overflow".into()))?;
                    etags.push(self.upload_part(&url, &upload_id, etags.len() + 1, part).await?);
                }
                if finished { break; }
            }
            let mut completion = String::from("<CompleteMultipartUpload>");
            for (index, etag) in etags.iter().enumerate() {
                use std::fmt::Write;
                write!(completion, "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>", index + 1, xml_escape(etag)).expect("String write");
            }
            completion.push_str("</CompleteMultipartUpload>");
            let (response, _) = self.send(HttpMethod::Post, &format!("{url}?uploadId={}", encode_query(&upload_id)),
                &[("content-type", "application/xml")], Some(completion.into_bytes()), Some(XML_LIMIT)).await?;
            require_status(&response, &[200], "complete multipart")?;
            if !std::str::from_utf8(&response.body).is_ok_and(|xml| super::super::s3_request::xml_result_body(xml, "CompleteMultipartUploadResult").is_some()) {
                return Err(NarouError::Platform("invalid S3 multipart completion document".into()));
            }
            xml_text(&response.body, "ETag")?;
            let metadata = self.stat_object(key).await?.ok_or_else(|| NarouError::Platform("completed S3 object missing".into()))?;
            if metadata.size != total { return Err(NarouError::Platform("completed S3 object length mismatch".into())); }
            self.save_reservation(key, None).await
        }.await;
        if result.is_err() { let _ = self.abort_upload(key, &upload_id).await; }
        result
    }

    async fn upload_part(&self, url: &str, upload_id: &str, number: usize, part: Vec<u8>) -> Result<String> {
        if number > 10_000 { return Err(NarouError::Platform("S3 multipart part-count limit exceeded".into())); }
        let url = format!("{url}?partNumber={number}&uploadId={}", encode_query(upload_id));
        let (response, _) = self.send(HttpMethod::Put, &url, &[], Some(part), Some(XML_LIMIT)).await?;
        require_status(&response, &[200], "upload part")?;
        response.header("etag").filter(|value| !value.is_empty()).map(str::to_owned)
            .ok_or_else(|| NarouError::Platform("S3 upload part omitted ETag".into()))
    }

    async fn reclaim_stage(&self, key: &ObjectKey, expired_only: bool) -> Result<bool> {
        if !self.claim_stage(key, expired_only).await? { return Ok(false); }
        // ListMultipartUploads is required even with a saved ID: the isolate
        // may have died after S3 initiated the upload, before saving its ID.
        let storage_key = self.location.storage_key(key);
        let object_url = self.location.object_url(key);
        let suffix = super::super::s3_sigv4::canonical_path(&storage_key);
        let bucket_url = object_url.strip_suffix(suffix.trim_start_matches('/'))
            .ok_or_else(|| NarouError::Platform("cannot construct multipart listing URL".into()))?;
        let mut key_marker = None;
        let mut upload_marker = None;
        loop {
            let mut url = format!("{}?uploads=&prefix={}&max-uploads=100", bucket_url.trim_end_matches('/'), encode_query(&storage_key));
            if let Some(value) = key_marker.as_deref() { url.push_str(&format!("&key-marker={}", encode_query(value))); }
            if let Some(value) = upload_marker.as_deref() { url.push_str(&format!("&upload-id-marker={}", encode_query(value))); }
            let (response, _) = self.send(HttpMethod::Get, &url, &[], None, Some(XML_LIMIT)).await?;
            require_status(&response, &[200], "list multipart uploads")?;
            let xml = std::str::from_utf8(&response.body).map_err(|_| NarouError::Platform("invalid multipart listing".into()))?;
            let xml = super::super::s3_request::xml_result_body(xml, "ListMultipartUploadsResult")
                .ok_or_else(|| NarouError::Platform("invalid multipart listing document".into()))?;
            let mut cursor = 0;
            while let Some(start) = super::super::s3_request::find_tag(&xml[cursor..], "Upload") {
                let start = cursor + start;
                let end = start + super::super::s3_request::find_close(&xml[start..], "Upload")
                    .ok_or_else(|| NarouError::Platform("truncated multipart listing".into()))?;
                let block = &xml[start..end];
                if xml_text(block.as_bytes(), "Key")? == storage_key {
                    self.abort_upload(key, &xml_text(block.as_bytes(), "UploadId")?).await?;
                }
                cursor = end + xml[end..].find('>').expect("validated XML close") + 1;
            }
            if xml_text(&response.body, "IsTruncated")? != "true" { break; }
            key_marker = Some(xml_text(&response.body, "NextKeyMarker")?);
            upload_marker = Some(xml_text(&response.body, "NextUploadIdMarker")?);
        }
        self.delete_object_raw(key).await?;
        self.delete_stage_record(key).await?;
        Ok(true)
    }

    /// Bounded scheduled reconciliation. Journal objects outlive request and
    /// isolate cancellation; records disappear only after confirmed cleanup.
    pub async fn collect_expired_stages(&self, limit: usize) -> Result<usize> {
        if limit == 0 { return Ok(0); }
        let checkpoint = ObjectKey::try_new(".narou-stage-scan-cursor")?;
        let mut cursor: Option<String> = match self.read_object(&checkpoint, Some(64 * 1024)).await? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| NarouError::Platform(format!("invalid stage scan cursor: {error}")))?,
            None => None,
        };
        let mut reclaimed = 0;
        let mut visited = 0;
        while visited < limit {
            let mut request = ObjectListRequest::new(super::super::object_store::ObjectPrefix::new(JOURNAL_PREFIX)?,
                std::num::NonZeroUsize::new((limit - visited).clamp(1, 100)).expect("nonzero"));
            if let Some(value) = cursor.take() { request = request.after(value); }
            let page = self.list_objects(&request).await?;
            for object in page.objects {
                visited += 1;
                let Some(bytes) = self.read_object(&object.key, Some(64 * 1024)).await? else { continue; };
                let entry: Reservation = serde_json::from_slice(&bytes).map_err(|error| NarouError::Platform(format!("invalid stage journal: {error}")))?;
                let key = ObjectKey::try_new(entry.key)?;
                if !is_staging_key(&key) { return Err(NarouError::Platform("stage journal has non-staging target".into())); }
                if (entry.reclaiming || entry.expires_at <= self.clock.now_utc().timestamp())
                    && self.reclaim_stage(&key, true).await? {
                    reclaimed += 1;
                }
            }
            cursor = page.next_cursor;
            if cursor.is_none() { break; }
        }
        // Advance across live reservations too: a busy prefix must not starve
        // abandoned uploads on later pages indefinitely.
        self.write_object(&checkpoint, serde_json::to_vec(&cursor)
            .map_err(|error| NarouError::Platform(error.to_string()))?).await?;
        Ok(reclaimed)
    }
}

/// Metadata uses only headers, even when an intermediary changes Range. Never
/// consume the body of a 200 response just to determine the object's size.
pub(super) fn metadata_size(response: &HttpStreamResponse) -> Result<u64> {
    match response.status {
        200 => response.header("content-length").and_then(|value| value.parse().ok())
            .ok_or_else(|| NarouError::Platform("S3 metadata omitted a valid Content-Length".into())),
        206 => content_range(response).map(|(_, total)| total),
        416 if response.header("content-range") == Some("bytes */0") => Ok(0),
        status => Err(NarouError::Platform(format!("S3 metadata GET failed with status {status}"))),
    }
}

fn content_range(response: &HttpStreamResponse) -> Result<(Range<u64>, u64)> {
    let header = response.header("content-range").and_then(|value| value.strip_prefix("bytes "))
        .ok_or_else(|| NarouError::Platform("S3 range omitted Content-Range".into()))?;
    let (span, total) = header.split_once('/').ok_or_else(|| NarouError::Platform("invalid Content-Range".into()))?;
    let (start, end) = span.split_once('-').ok_or_else(|| NarouError::Platform("invalid Content-Range".into()))?;
    let parse = |value: &str| value.parse::<u64>().map_err(|_| NarouError::Platform("invalid Content-Range number".into()));
    let total = parse(total)?;
    let start = parse(start)?;
    let end = parse(end)?.checked_add(1).ok_or_else(|| NarouError::Platform("Content-Range overflow".into()))?;
    if start >= end || end > total {
        return Err(NarouError::Platform("invalid Content-Range bounds".into()));
    }
    if let Some(length) = response.header("content-length") && parse(length)? != end - start {
        return Err(NarouError::Platform("S3 range Content-Length mismatch".into()));
    }
    Ok((start..end, total))
}

pub(super) fn validate_content_range(response: &HttpStreamResponse, range: &Range<u64>) -> Result<u64> {
    // Reject an ignored Range before pulling a single response-body byte.
    if response.status != 206 { return Err(NarouError::Platform(format!("S3 ignored or rejected Range (status {})", response.status))); }
    let (actual, total) = content_range(response)?;
    if actual != *range {
        return Err(NarouError::Platform("S3 Content-Range does not match requested range".into()));
    }
    Ok(total)
}

fn exact_stream(stream: AssetStream, expected: Option<u64>) -> AssetStream {
    Box::pin(futures::stream::try_unfold((stream, 0u64), move |(mut stream, total)| async move {
        match stream.next().await {
            Some(chunk) => {
                let chunk = chunk?;
                let next = total.checked_add(chunk.len() as u64).ok_or_else(|| NarouError::Platform("S3 body length overflow".into()))?;
                if expected.is_some_and(|length| next > length) { return Err(NarouError::Platform("S3 response exceeds declared length".into())); }
                Ok(Some((chunk, (stream, next))))
            }
            None if expected.is_some_and(|length| total != length) => Err(NarouError::Platform("S3 response truncated".into())),
            None => Ok(None),
        }
    }))
}

#[cfg(test)]
#[path = "s3_stream_tests.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn range_validation_rejects_ignored_and_inconsistent_ranges() {
        let response = |status, header: &str| HttpStreamResponse { status, url: String::new(),
            headers: vec![("content-range".into(), header.into())], body: Box::pin(futures::stream::empty()) };
        assert!(validate_content_range(&response(200, "bytes 0-9/20"), &(0..10)).is_err());
        assert!(validate_content_range(&response(206, "bytes 1-10/20"), &(0..10)).is_err());
        assert_eq!(validate_content_range(&response(206, "bytes 0-9/20"), &(0..10)).unwrap(), 20);
    }
    #[test]
    fn completed_error_xml_is_failure() {
        assert!(xml_text(b"<Error><Code>InternalError</Code><ETag>fake</ETag></Error>", "ETag").is_err());
    }
}
