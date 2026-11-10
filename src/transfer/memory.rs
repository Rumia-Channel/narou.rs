//! Working-set admission shared by manifest decoding and asynchronous export.
use super::{ArchiveManifest, ArchiveNovel, Result, TransferError, MAX_MANIFEST_METADATA_BYTES};

pub(super) fn default_limit() -> usize {
    if cfg!(target_arch = "wasm32") { MAX_MANIFEST_METADATA_BYTES } else { usize::MAX }
}

pub(super) fn allocation_limit() -> TransferError {
    TransferError::TooLarge("transfer metadata allocation exceeds resource budget".into())
}

pub(super) struct MetadataBudget { pub(super) used: usize, pub(super) limit: usize }
impl MetadataBudget {
    pub(super) fn allocation<T>(capacity: usize) -> Result<usize> {
        Self::allocation_bytes(capacity, std::mem::size_of::<T>())
    }
    pub(super) fn allocation_bytes(capacity: usize, item_size: usize) -> Result<usize> {
        if capacity == 0 { return Ok(0); }
        capacity.checked_mul(item_size).and_then(|bytes| bytes.checked_add(15))
            .map(|bytes| bytes & !15).and_then(|bytes| bytes.checked_add(16))
            .ok_or_else(allocation_limit)
    }
    pub(super) fn check(&self, additional: usize) -> Result<()> {
        if additional > self.limit.saturating_sub(self.used) { return Err(allocation_limit()); }
        Ok(())
    }
    pub(super) fn charge(&mut self, additional: usize) -> Result<()> {
        self.check(additional)?;
        self.used += additional;
        Ok(())
    }
    pub(super) fn release_charge(&mut self, charge: usize) { self.used -= charge; }

    /// A replacement allocation coexists with the old storage until relocation
    /// finishes. Choose the largest useful growth that fits that peak.
    pub(super) fn growth(&self, length: usize, capacity: usize, additional: usize, item_size: usize) -> Result<usize> {
        let required = length.checked_add(additional).ok_or_else(allocation_limit)?;
        if required <= capacity { return Ok(capacity); }
        let preferred = capacity.saturating_mul(2).max(4).max(required);
        let available = self.limit.saturating_sub(self.used);
        let fitting = (available.saturating_sub(16) & !15) / item_size.max(1);
        let target = preferred.min(fitting);
        if target < required { return Err(allocation_limit()); }
        self.check(Self::allocation_bytes(target, item_size)?)?;
        Ok(target)
    }
    pub(super) fn replace_allocation(&mut self, old_capacity: usize, new_capacity: usize, item_size: usize) -> Result<()> {
        let previous = Self::allocation_bytes(old_capacity, item_size)?;
        let next = Self::allocation_bytes(new_capacity, item_size)?;
        self.used = self.used - previous + next;
        if self.used > self.limit { return Err(allocation_limit()); }
        Ok(())
    }
    pub(super) fn reserve<T>(&mut self, values: &mut Vec<T>, additional: usize) -> Result<()> {
        let capacity = self.growth(values.len(), values.capacity(), additional, std::mem::size_of::<T>())?;
        if capacity == values.capacity() { return Ok(()); }
        let previous = values.capacity();
        values.try_reserve_exact(capacity - values.len()).map_err(|_| allocation_limit())?;
        self.replace_allocation(previous, values.capacity(), std::mem::size_of::<T>())
    }
    pub(super) fn release<T>(&mut self, values: &Vec<T>) {
        self.release_charge(Self::allocation::<T>(values.capacity()).expect("previously charged allocation"));
    }
    pub(super) fn string(&mut self, capacity: usize) -> Result<String> {
        let charge = Self::allocation::<u8>(capacity)?;
        self.charge(charge)?;
        let mut value = String::new();
        value.try_reserve_exact(capacity).map_err(|_| allocation_limit())?;
        self.replace_allocation(capacity, value.capacity(), 1)?;
        Ok(value)
    }
    pub(super) fn adopt_manifest(&mut self, manifest: &ArchiveManifest) -> Result<()> {
        self.charge(Self::allocation::<ArchiveNovel>(manifest.novels.capacity())?)?;
        for value in [&manifest.app_version, &manifest.exported_at].into_iter().flatten() {
            self.charge(Self::allocation::<u8>(value.capacity())?)?;
        }
        for novel in &manifest.novels { self.adopt_novel(novel)?; }
        Ok(())
    }
    pub(super) fn adopt_novel(&mut self, novel: &ArchiveNovel) -> Result<()> {
        for value in [&novel.toc_url, &novel.title, &novel.author, &novel.sitename] {
            self.charge(Self::allocation::<u8>(value.capacity())?)?;
        }
        for value in [&novel.domain, &novel.attribute_tag, &novel.ncode, &novel.subtitle,
            &novel.general_lastup, &novel.last_update].into_iter().flatten() {
            self.charge(Self::allocation::<u8>(value.capacity())?)?;
        }
        self.charge(Self::allocation::<String>(novel.tags.capacity())?)?;
        for tag in &novel.tags { self.charge(Self::allocation::<u8>(tag.capacity())?)?; }
        if let Some(members) = &novel.virtual_members {
            self.charge(Self::allocation::<String>(members.capacity())?)?;
            for url in members { self.charge(Self::allocation::<u8>(url.capacity())?)?; }
        }
        self.charge(Self::allocation::<super::ArchiveFile>(novel.files.capacity())?)?;
        for file in &novel.files {
            self.charge(Self::allocation::<u8>(file.path.capacity())?)?;
            self.charge(Self::allocation::<u8>(file.bytes.capacity())?)?;
            if let Some(hash) = &file.sha256 { self.charge(Self::allocation::<u8>(hash.capacity())?)?; }
        }
        Ok(())
    }
}
