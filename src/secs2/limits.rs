//! Resource policy for strict, bounded SECS-II decoding.
//!
//! E5 defines the representable per-item length. The remaining limits are
//! configurable engineering guardrails against excessive nesting, allocation,
//! and item counts; they are not SEMI-recommended operating values.

use super::SecsItemError;

/// Maximum value representable by the one-to-three byte E5 length field.
pub const MAX_ENCODED_ITEM_LENGTH: usize = 0x00FF_FFFF;

/// Hard ceiling for List nesting accepted by the decoder.
///
/// [`crate::secs2::SecsItem`] is a recursively owned public enum, so the
/// recursive destruction depth of a decoded tree or an already completed
/// subtree during error cleanup grows in proportion to List nesting. Keeping
/// that nesting at or below 256 gives every decoder-created cleanup path a
/// small, fixed upper bound while remaining well above the depth used by
/// practical SECS-II message schemas. Applications may configure a lower
/// limit but cannot raise it beyond this safety boundary.
pub const MAX_DECODE_NESTING_DEPTH: usize = 256;

/// Resource limits applied by the strict SECS-II decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Maximum number of nested List levels accepted in one item tree.
    max_depth: usize,
    /// Maximum number of nodes accepted across the complete decoded tree.
    max_total_items: usize,
    /// Maximum encoded payload bytes accepted for one non-List item.
    max_item_bytes: usize,
    /// Maximum number of direct children accepted in one List item.
    max_list_items: usize,
}

impl DecodeLimits {
    /// Creates decoder limits from the supplied nesting, total-node,
    /// per-item-byte, and per-List-child bounds.
    ///
    /// Returns the validated limits or a [`SecsItemError`] when any value is
    /// zero, `max_depth` exceeds [`MAX_DECODE_NESTING_DEPTH`], or
    /// `max_item_bytes` exceeds the E5 three-byte length field.
    pub fn new(
        max_depth: usize,
        max_total_items: usize,
        max_item_bytes: usize,
        max_list_items: usize,
    ) -> Result<Self, SecsItemError> {
        let limits = Self {
            max_depth,
            max_total_items,
            max_item_bytes,
            max_list_items,
        };
        limits.validate()?;
        Ok(limits)
    }

    /// Verifies that every configured limit is non-zero, the nesting limit is
    /// bounded for safe recursive tree cleanup, and the byte limit is
    /// representable by the E5 length field.
    ///
    /// Returns `Ok(())` when the limits are usable, otherwise the first
    /// validation error in field order.
    pub fn validate(&self) -> Result<(), SecsItemError> {
        for (field, value) in [
            ("max_depth", self.max_depth),
            ("max_total_items", self.max_total_items),
            ("max_item_bytes", self.max_item_bytes),
            ("max_list_items", self.max_list_items),
        ] {
            if value == 0 {
                return Err(SecsItemError::ZeroLimit { field });
            }
        }

        if self.max_depth > MAX_DECODE_NESTING_DEPTH {
            return Err(SecsItemError::DepthLimitTooLarge {
                value: self.max_depth,
                maximum: MAX_DECODE_NESTING_DEPTH,
            });
        }

        if self.max_item_bytes > MAX_ENCODED_ITEM_LENGTH {
            return Err(SecsItemError::ItemLengthTooLarge {
                value: self.max_item_bytes,
                maximum: MAX_ENCODED_ITEM_LENGTH,
            });
        }

        Ok(())
    }

    #[must_use]
    /// Returns the maximum permitted List nesting depth.
    pub fn max_depth(self) -> usize {
        self.max_depth
    }

    #[must_use]
    /// Returns the maximum number of nodes in one decoded item tree.
    pub fn max_total_items(self) -> usize {
        self.max_total_items
    }

    #[must_use]
    /// Returns the maximum payload bytes permitted for one non-List item.
    pub fn max_item_bytes(self) -> usize {
        self.max_item_bytes
    }

    #[must_use]
    /// Returns the maximum number of direct children permitted in one List.
    pub fn max_list_items(self) -> usize {
        self.max_list_items
    }
}

impl Default for DecodeLimits {
    /// Returns the standard SECS-II decoder resource guardrails.
    ///
    /// The item-byte value follows the E5 protocol maximum. Depth and item
    /// counts are engineering defaults that applications should tune to their
    /// message schemas and memory budget.
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_total_items: 1_000_000,
            max_item_bytes: MAX_ENCODED_ITEM_LENGTH,
            max_list_items: 1_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for construction-time resource-limit validation.

    use super::*;

    /// Builds limits with the requested depth and otherwise valid default
    /// values, returning the constructor result for boundary assertions.
    fn limits_with_depth(max_depth: usize) -> Result<DecodeLimits, SecsItemError> {
        let defaults = DecodeLimits::default();
        DecodeLimits::new(
            max_depth,
            defaults.max_total_items(),
            defaults.max_item_bytes(),
            defaults.max_list_items(),
        )
    }

    /// Confirms that the public hard nesting ceiling itself remains accepted.
    #[test]
    fn nesting_safety_ceiling_is_accepted() {
        let limits = limits_with_depth(MAX_DECODE_NESTING_DEPTH).expect("safety ceiling");
        assert_eq!(limits.max_depth(), MAX_DECODE_NESTING_DEPTH);
    }

    /// Confirms that a requested depth immediately above the safety ceiling
    /// is rejected with a unit-specific construction error.
    #[test]
    fn nesting_depth_above_safety_ceiling_is_rejected() {
        let requested = MAX_DECODE_NESTING_DEPTH + 1;
        assert_eq!(
            limits_with_depth(requested),
            Err(SecsItemError::DepthLimitTooLarge {
                value: requested,
                maximum: MAX_DECODE_NESTING_DEPTH,
            })
        );
    }
}
