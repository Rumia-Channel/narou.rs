//! Progress leases use conditional object writes, never resurrecting a deleted
//! reservation. Expired work is claimed before any destructive reconciliation.

use super::*;

impl S3Store {
    async fn load_reservation(&self, key: &ObjectKey) -> Result<Option<(Reservation, String)>> {
        let (response, _) = self.send(HttpMethod::Get, &self.location.object_url(&journal_key(key)?), &[], None, Some(64 * 1024)).await?;
        if response.status == 404 { return Ok(None); }
        require_status(&response, &[200], "read stage lease")?;
        let etag = response.header("etag").filter(|value| !value.is_empty()).map(str::to_owned)
            .ok_or_else(|| NarouError::Platform("S3 stage lease response omitted ETag".into()))?;
        let value: Reservation = serde_json::from_slice(&response.body)
            .map_err(|error| NarouError::Platform(format!("invalid stage lease: {error}")))?;
        if value.key != key.as_ref() { return Err(NarouError::Platform("stage lease key mismatch".into())); }
        Ok(Some((value, etag)))
    }

    async fn put_reservation(&self, key: &ObjectKey, value: &Reservation, expected: Option<&str>) -> Result<bool> {
        let bytes = serde_json::to_vec(value).map_err(|error| NarouError::Platform(error.to_string()))?;
        if bytes.len() > 64 * 1024 { return Err(NarouError::Platform("S3 stage lease exceeds metadata bound".into())); }
        let condition = match expected { Some(etag) => ("if-match", etag), None => ("if-none-match", "*") };
        let (response, _) = self.send(HttpMethod::Put, &self.location.object_url(&journal_key(key)?),
            &[("content-type", "application/json"), condition], Some(bytes), Some(XML_LIMIT)).await?;
        if matches!(response.status, 409 | 412) { return Ok(false); }
        require_status(&response, &[200, 201, 204], "update stage lease")?;
        Ok(true)
    }

    pub(super) async fn reserve_stage(&self, key: &ObjectKey) -> Result<()> {
        let now = self.clock.now_utc().timestamp();
        let value = Reservation { key: key.to_string(), upload_id: None, expires_at: now + STAGE_TTL_SECONDS, reclaiming: false };
        if !self.put_reservation(key, &value, None).await? {
            return Err(NarouError::Platform("stage key is already reserved".into()));
        }
        self.stage_leases.renewed(key, now);
        Ok(())
    }

    async fn update_stage_lease(&self, key: &ObjectKey, upload_id: Option<Option<&str>>) -> Result<()> {
        for _ in 0..3 {
            let Some((mut value, etag)) = self.load_reservation(key).await? else {
                return Err(NarouError::Platform("stage lease was deleted".into()));
            };
            if value.reclaiming { return Err(NarouError::Platform("stage lease has expired and is being reclaimed".into())); }
            let now = self.clock.now_utc().timestamp();
            value.expires_at = now + STAGE_TTL_SECONDS;
            if let Some(upload_id) = upload_id { value.upload_id = upload_id.map(str::to_owned); }
            if self.put_reservation(key, &value, Some(&etag)).await? {
                self.stage_leases.renewed(key, now);
                return Ok(());
            }
        }
        Err(NarouError::Platform("stage lease changed concurrently".into()))
    }

    pub(super) async fn save_reservation(&self, key: &ObjectKey, upload_id: Option<&str>) -> Result<()> {
        self.update_stage_lease(key, Some(upload_id)).await
    }

    pub(in crate::platform::s3_store) async fn renew_stage_progress(&self, key: &ObjectKey) -> Result<()> {
        if is_staging_key(key) && self.stage_leases.needs_renewal(key, self.clock.now_utc().timestamp()) {
            self.update_stage_lease(key, None).await?;
        }
        Ok(())
    }

    pub(super) async fn claim_stage(&self, key: &ObjectKey, expired_only: bool) -> Result<bool> {
        let Some((mut value, etag)) = self.load_reservation(key).await? else { return Ok(false); };
        if value.reclaiming { return Ok(true); }
        if expired_only && value.expires_at > self.clock.now_utc().timestamp() { return Ok(false); }
        value.reclaiming = true;
        self.put_reservation(key, &value, Some(&etag)).await
    }
}
