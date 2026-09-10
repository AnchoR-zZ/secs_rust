//! Public completion receipts produced after outbound frames commit locally.
//!
//! Writer assigns wire ordering. Driver combines its actual committed outcome
//! with the connection generation to construct the public receipt.

use crate::hsms::model::ids::ConnectionGeneration;
#[cfg(any(feature = "runtime-tokio", test))]
use crate::hsms::model::ids::WireSequence;

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
    #[cfg(any(feature = "runtime-tokio", test))]
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
