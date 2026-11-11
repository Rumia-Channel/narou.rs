//! Internal heap-allocation accounting. The caller's reader and output slice are not owned here.

use std::cell::Cell;
use std::mem::size_of;
use std::ops::{Deref, DerefMut};

use crate::DecodingError;

#[derive(Debug)]
pub(crate) struct MemoryBudget {
    limit: usize,
    used: Cell<usize>,
}

impl MemoryBudget {
    pub(crate) const fn new(limit: usize) -> Self {
        Self { limit, used: Cell::new(0) }
    }

    pub(crate) fn reserve(&self, bytes: usize) -> Result<Allocation<'_>, DecodingError> {
        let used = self.used.get().checked_add(bytes)
            .filter(|&used| used <= self.limit)
            .ok_or(DecodingError::MemoryLimitExceeded)?;
        self.used.set(used);
        Ok(Allocation { budget: self, bytes })
    }

    pub(crate) fn string(&self, text: &str) -> Result<String, DecodingError> {
        let mut data = BudgetedVec::filled(self, text.len(), 0)?;
        data.copy_from_slice(text.as_bytes());
        // UTF-8 is unchanged, and conversion transfers the allocation without copying.
        Ok(String::from_utf8(data.into_vec()).unwrap())
    }
}

/// Charges a set of allocations with the same lifetime (not a cumulative decode quota).
#[derive(Debug)]
pub(crate) struct Allocation<'a> {
    budget: &'a MemoryBudget,
    bytes: usize,
}

impl Allocation<'_> {
    pub(crate) fn resize(&mut self, bytes: usize) -> Result<(), DecodingError> {
        if bytes > self.bytes {
            let extra = self.budget.reserve(bytes - self.bytes)?;
            // Transfer the charge to this reservation, without releasing it.
            std::mem::forget(extra);
        } else {
            self.budget.used.set(self.budget.used.get() - (self.bytes - bytes));
        }
        self.bytes = bytes;
        Ok(())
    }

    pub(crate) fn reserve_vec<T>(
        &mut self,
        data: &mut Vec<T>,
        additional: usize,
    ) -> Result<(), DecodingError> {
        let required = data.len().checked_add(additional)
            .ok_or(DecodingError::MemoryLimitExceeded)?;
        if required <= data.capacity() {
            return Ok(());
        }
        let old_bytes = allocation_size::<T>(data.capacity())?;
        let new_bytes = allocation_size::<T>(required)?;
        // A realloc may hold the old and new allocations simultaneously.
        let mut replacement = self.budget.reserve(new_bytes)?;
        data.try_reserve_exact(additional).map_err(|_| DecodingError::MemoryLimitExceeded)?;
        replacement.resize(allocation_size::<T>(data.capacity())?)?;
        let retained = self.bytes.checked_sub(old_bytes)
            .and_then(|bytes| bytes.checked_add(replacement.bytes))
            .ok_or(DecodingError::MemoryLimitExceeded)?;
        self.budget.used.set(self.budget.used.get() - old_bytes);
        self.bytes = retained;
        std::mem::forget(replacement);
        Ok(())
    }

    /// Geometric growth, capped by the remaining budget including a possible realloc copy.
    pub(crate) fn grow_vec<T>(&mut self, data: &mut Vec<T>, required: usize) -> Result<(), DecodingError> {
        if required <= data.capacity() {
            return Ok(());
        }
        let available = (self.budget.limit - self.budget.used.get()) / size_of::<T>().max(1);
        let target = required.max(data.capacity().saturating_mul(2).min(available));
        self.reserve_vec(data, target - data.len())
    }

    pub(crate) fn vec<T: Clone>(&mut self, len: usize, value: T) -> Result<Vec<T>, DecodingError> {
        let mut data = Vec::new();
        self.reserve_vec(&mut data, len)?;
        data.resize(len, value);
        Ok(data)
    }

    pub(crate) fn string(&mut self, text: &str) -> Result<String, DecodingError> {
        let mut data = self.vec(text.len(), 0)?;
        data.copy_from_slice(text.as_bytes());
        Ok(String::from_utf8(data).unwrap())
    }
}

impl Drop for Allocation<'_> {
    fn drop(&mut self) {
        self.budget.used.set(self.budget.used.get() - self.bytes);
    }
}

#[derive(Debug)]
pub(crate) struct BudgetedVec<'a, T> {
    // Drop the storage before releasing its charge.
    data: Vec<T>,
    allocation: Allocation<'a>,
}

impl<'a, T> BudgetedVec<'a, T> {
    pub(crate) fn new(budget: &'a MemoryBudget) -> Self {
        Self { data: Vec::new(), allocation: Allocation { budget, bytes: 0 } }
    }

    pub(crate) fn with_capacity(budget: &'a MemoryBudget, len: usize) -> Result<Self, DecodingError> {
        let mut data = Self::new(budget);
        data.allocation.reserve_vec(&mut data.data, len)?;
        Ok(data)
    }

    pub(crate) fn filled(budget: &'a MemoryBudget, len: usize, value: T) -> Result<Self, DecodingError>
    where T: Clone,
    {
        let mut data = Self::with_capacity(budget, len)?;
        data.data.resize(len, value);
        Ok(data)
    }

    pub(crate) fn push(&mut self, value: T) -> Result<(), DecodingError> {
        self.allocation.reserve_vec(&mut self.data, 1)?;
        self.data.push(value);
        Ok(())
    }

    pub(crate) fn as_slice(&self) -> &[T] { &self.data }

    /// Transfers storage to the caller. Retained decoder storage must be charged again next call.
    pub(crate) fn into_vec(self) -> Vec<T> { self.data }
}

impl<T> Deref for BudgetedVec<'_, T> {
    type Target = [T];
    fn deref(&self) -> &[T] { &self.data }
}

impl<T> DerefMut for BudgetedVec<'_, T> {
    fn deref_mut(&mut self) -> &mut [T] { &mut self.data }
}

pub(crate) fn allocation_size<T>(len: usize) -> Result<usize, DecodingError> {
    len.checked_mul(size_of::<T>()).filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or(DecodingError::MemoryLimitExceeded)
}

pub(crate) fn image_size(width: u32, height: u32, channels: usize) -> Result<usize, DecodingError> {
    usize::try_from(width).ok()
        .and_then(|width| usize::try_from(height).ok().and_then(|height| width.checked_mul(height)))
        .and_then(|pixels| pixels.checked_mul(channels))
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or(DecodingError::MemoryLimitExceeded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_simultaneous_buffers_and_releases_on_drop() {
        let budget = MemoryBudget::new(16);
        let a = BudgetedVec::filled(&budget, 8, 0u8).unwrap();
        assert!(matches!(BudgetedVec::filled(&budget, 9, 0u8), Err(DecodingError::MemoryLimitExceeded)));
        let b = BudgetedVec::filled(&budget, 8, 0u8).unwrap();
        drop(a);
        drop(b);
        assert!(BudgetedVec::filled(&budget, 16, 0u8).is_ok());
        assert_eq!(budget.used.get(), 0);
    }

    #[test]
    fn rejects_overflow_before_allocating() {
        let budget = MemoryBudget::new(usize::MAX);
        assert!(matches!(BudgetedVec::filled(&budget, usize::MAX, 0u32), Err(DecodingError::MemoryLimitExceeded)));
        assert!(matches!(image_size(u32::MAX, u32::MAX, 4), Err(DecodingError::MemoryLimitExceeded)));
        assert_eq!(budget.used.get(), 0);
    }

    #[test]
    fn growth_accounts_for_reallocation_peak() {
        let budget = MemoryBudget::new(8);
        let mut data = BudgetedVec::filled(&budget, 4, 0u8).unwrap();
        assert!(matches!(data.push(1), Err(DecodingError::MemoryLimitExceeded)));
        assert_eq!(data.len(), 4);
        assert_eq!(budget.used.get(), 4);
    }
}
