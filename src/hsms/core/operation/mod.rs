//! Bounded operation ownership and exactly-once terminal-claim boundary.
//!
//! The implementation is introduced in the next task; this module reserves a
//! conflict-free path after shared contracts have been frozen.

mod correlation;
mod ledger;
