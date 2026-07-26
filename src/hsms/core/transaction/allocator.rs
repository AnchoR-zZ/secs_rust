//! Provides the non-zero, wrapping System Bytes cursor used by the transaction
//! registry. Candidate discovery is deliberately non-mutating so the registry
//! can commit allocation only after every reservation precondition succeeds.

use crate::hsms::model::ids::SystemBytes;

/// Number of locally allocatable, non-zero System Bytes values.
const SYSTEM_BYTES_SPACE: u64 = u32::MAX as u64;

/// Failure to find an unoccupied System Bytes candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AllocationError {
    /// Every value in the non-zero System Bytes domain is occupied.
    Exhausted,
    /// The supplied occupancy count disagreed with the occupancy predicate.
    InconsistentOccupancy,
}

/// Non-zero System Bytes cursor whose mutation is separated from discovery.
#[derive(Clone, Copy, Debug)]
pub(super) struct SystemBytesAllocator {
    /// First candidate considered by the next successful reservation.
    next: u32,
}

impl SystemBytesAllocator {
    /// Creates an allocator whose first candidate is System Bytes value one.
    pub(super) const fn new() -> Self {
        Self { next: 1 }
    }

    /// Finds an available value without advancing the allocator.
    ///
    /// `occupied_count` is the number of distinct occupied non-zero values and
    /// `is_occupied` tests each candidate. The method examines at most one more
    /// value than the reported occupancy count, which is sufficient by the
    /// pigeonhole principle and avoids scanning the complete `u32` domain in
    /// normal bounded configurations.
    pub(super) fn find_available(
        &self,
        occupied_count: u64,
        mut is_occupied: impl FnMut(SystemBytes) -> bool,
    ) -> Result<SystemBytes, AllocationError> {
        if occupied_count >= SYSTEM_BYTES_SPACE {
            return Err(AllocationError::Exhausted);
        }

        let mut candidate = self.next;
        for _ in 0..=occupied_count {
            let system_bytes = SystemBytes::new(candidate);
            if !is_occupied(system_bytes) {
                return Ok(system_bytes);
            }
            candidate = successor(candidate);
        }

        Err(AllocationError::InconsistentOccupancy)
    }

    /// Commits `reserved` and advances the next cursor to its wrapping successor.
    pub(super) fn commit(&mut self, reserved: SystemBytes) {
        self.next = successor(reserved.get());
    }

    /// Creates an allocator at `next` for boundary-focused unit tests.
    #[cfg(test)]
    pub(super) const fn with_next(next: u32) -> Self {
        assert!(next != 0, "allocator candidates must be non-zero");
        Self { next }
    }

    /// Returns the current candidate without modifying the allocator.
    #[cfg(test)]
    pub(super) const fn next_candidate(self) -> SystemBytes {
        SystemBytes::new(self.next)
    }
}

/// Returns the next non-zero System Bytes value, wrapping `u32::MAX` to one.
const fn successor(value: u32) -> u32 {
    if value == u32::MAX {
        1
    } else {
        value + 1
    }
}

#[cfg(test)]
mod tests {
    //! Allocation tests cover the initial cursor, wrap, collision skipping, and
    //! the non-mutating failure half of atomic registry reservation.

    use super::*;

    /// Confirms the allocator starts at the first non-zero System Bytes value.
    #[test]
    fn initial_candidate_is_one() {
        let allocator = SystemBytesAllocator::new();

        assert_eq!(
            allocator
                .find_available(0, |_| false)
                .expect("one is free")
                .get(),
            1
        );
        assert_eq!(allocator.next_candidate().get(), 1);
    }

    /// Confirms a committed maximum value wraps the cursor back to one.
    #[test]
    fn maximum_value_wraps_to_one() {
        let mut allocator = SystemBytesAllocator::with_next(u32::MAX);
        let reserved = allocator
            .find_available(0, |_| false)
            .expect("maximum is free");

        allocator.commit(reserved);

        assert_eq!(reserved.get(), u32::MAX);
        assert_eq!(allocator.next_candidate().get(), 1);
    }

    /// Confirms candidate discovery skips consecutive occupied values.
    #[test]
    fn occupied_candidates_are_skipped() {
        let allocator = SystemBytesAllocator::with_next(7);

        let reserved = allocator
            .find_available(2, |candidate| matches!(candidate.get(), 7 | 8))
            .expect("nine is free");

        assert_eq!(reserved.get(), 9);
    }

    /// Confirms failed discovery leaves the cursor unchanged for a later retry.
    #[test]
    fn failed_discovery_does_not_advance_cursor() {
        let allocator = SystemBytesAllocator::with_next(23);

        assert_eq!(
            allocator.find_available(0, |_| true),
            Err(AllocationError::InconsistentOccupancy)
        );
        assert_eq!(allocator.next_candidate().get(), 23);
    }

    /// Confirms a complete logical occupancy reports exhaustion immediately.
    #[test]
    fn complete_domain_reports_exhaustion_without_scanning() {
        let allocator = SystemBytesAllocator::new();

        assert_eq!(
            allocator.find_available(SYSTEM_BYTES_SPACE, |_| false),
            Err(AllocationError::Exhausted)
        );
        assert_eq!(allocator.next_candidate().get(), 1);
    }
}
