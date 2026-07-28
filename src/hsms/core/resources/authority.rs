//! Sealed mutation authority shared only by non-publication Core resources.
//!
//! Publication authority lives under the private `publication` subtree so
//! other `CoreResources` children cannot construct or coordinate its ledgers.

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
