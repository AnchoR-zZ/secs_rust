//! Strict SECS-II binary codec boundary.
//!
//! The Wave 1 SECS-II agent owns this module. It will expose exact decoding
//! that rejects trailing bytes and respects [`super::DecodeLimits`].
