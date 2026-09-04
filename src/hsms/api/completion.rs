//! Public completion receipts produced after outbound frames commit locally.
//!
//! The future writer will own wire ordering; the protocol core will combine
//! its terminal outcome with the current generation to construct receipts.

// Construction becomes production-reachable with the future endpoint runtime.
#![allow(dead_code)]

use crate::hsms::model::ids::{ConnectionGeneration, WireSequence};

/// Proof that one complete frame reached the local writer commit point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendReceipt {
    /// TCP incarnation whose writer committed the frame.
    generation: ConnectionGeneration,
    /// Generation-local ordered position of the committed frame.
    wire_sequence: u64,
}

impl SendReceipt {
    /// Creates a receipt after Driver combines Core's committed-write identity
    /// with the Writer-owned `wire_sequence` for `generation`.
    pub(crate) const fn new(generation: ConnectionGeneration, wire_sequence: WireSequence) -> Self {
        Self {
            generation,
            wire_sequence: wire_sequence.get(),
        }
    }

    /// Returns the TCP generation that committed the frame.
    #[must_use]
    pub const fn generation(self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the frame's generation-local wire sequence.
    #[must_use]
    pub const fn wire_sequence(self) -> u64 {
        self.wire_sequence
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::model::ids::{ConnectionGeneration, WireSequence};

    use super::SendReceipt;

    /// Confirms the crate-private Driver construction boundary preserves the
    /// exact generation and Writer-assigned total-order position.
    #[test]
    fn send_receipt_preserves_driver_and_writer_facts() {
        let generation = ConnectionGeneration::new(17);
        let receipt = SendReceipt::new(generation, WireSequence::new(41));

        assert_eq!(receipt.generation(), generation);
        assert_eq!(receipt.wire_sequence(), 41);
    }
}
