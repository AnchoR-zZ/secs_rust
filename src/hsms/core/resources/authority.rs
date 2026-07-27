//! Sealed mutation authority shared by the HSMS resource aggregate and its ledgers.
//!
//! The resource coordinator is the sole production issuer. Operation and transaction
//! ledgers may name and consume the gate, but cannot construct it.

/// Sealed authority allowing the transaction ledger to apply a validated peer Reject.
#[derive(Debug)]
pub(crate) struct PeerRejectMutationAuthority {
    /// Private field preventing construction outside this module.
    private: (),
}

impl PeerRejectMutationAuthority {
    /// Issues the transaction-mutation authority to the parent resource coordinator.
    ///
    /// Returns the sole authority value owned by one `CoreResources` aggregate.
    pub(super) const fn new() -> Self {
        Self { private: () }
    }

    /// Issues an authority value for focused ledger unit tests.
    ///
    /// Returns a proof that is constructible only while compiling crate tests.
    #[cfg(test)]
    pub(crate) const fn for_test() -> Self {
        Self::new()
    }
}
