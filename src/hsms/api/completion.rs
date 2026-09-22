//! Public completion receipts produced after outbound frames commit locally.
//!
//! Driver combines the Writer's actual committed outcome
//! with the connection generation to construct the public receipt.

use crate::hsms::model::ids::ConnectionGeneration;

/// Proof that one complete frame reached the local writer commit point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendReceipt {
    /// TCP incarnation whose writer committed the frame.
    generation: ConnectionGeneration,
}

impl SendReceipt {
    /// Creates a receipt for `generation` after Driver validates local write completion.
    #[cfg(any(feature = "runtime-tokio", test))]
    pub(crate) const fn new(generation: ConnectionGeneration) -> Self {
        Self { generation }
    }

    /// Returns the TCP generation that committed the frame.
    #[must_use]
    pub const fn generation(self) -> ConnectionGeneration {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::model::ids::ConnectionGeneration;

    use super::SendReceipt;

    /// Confirms the crate-private Driver construction boundary preserves the
    /// connection generation of the completed local write.
    #[test]
    fn send_receipt_preserves_connection_generation() {
        let generation = ConnectionGeneration::new(17);
        let receipt = SendReceipt::new(generation);

        assert_eq!(receipt.generation(), generation);
    }
}
