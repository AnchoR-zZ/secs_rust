//! Sealed authority and nonce identities private to `PublicationResources`.
//!
//! The publication coordinator is the sole production issuer. Reply and Delivery
//! ledgers retain the same opaque publication-aggregate identity, so an
//! authority or clear receipt from another aggregate cannot mutate them even
//! when every generation and resource identifier has the same numeric value.

use std::sync::Arc;

use crate::hsms::model::ids::ConnectionGeneration;

/// Private allocation whose pointer identity names one publication aggregate.
#[derive(Debug)]
struct PublicationAggregateBrand {
    /// Private field preventing construction outside this module.
    private: (),
}

/// Opaque observation of one exact publication-aggregate instance.
///
/// Ledgers and receipts retain this non-`Copy` identity. It is not mutation
/// authority and exposes only exact pointer-identity comparisons.
#[derive(Debug)]
pub(super) struct PublicationAggregateIdentity {
    /// Private pointer brand shared by one aggregate's publication resources.
    brand: Arc<PublicationAggregateBrand>,
}

impl PublicationAggregateIdentity {
    /// Duplicates this non-authorizing observation for another owned proof.
    pub(super) fn duplicate(&self) -> Self {
        Self {
            brand: Arc::clone(&self.brand),
        }
    }

    /// Returns whether `authority` controls this exact publication aggregate.
    pub(super) fn matches_authority(&self, authority: &PublicationMutationAuthority) -> bool {
        Arc::ptr_eq(&self.brand, &authority.brand)
    }

    /// Returns whether `other` observes this exact publication aggregate.
    pub(super) fn exact_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.brand, &other.brand)
    }
}

/// Move-only authority for one aggregate's Reply and Delivery mutations.
///
/// Unlike a namespace-only zero-sized gate, this value carries an exact
/// pointer brand. Passing another aggregate's otherwise identical authority
/// therefore produces a structured, non-mutating rejection.
#[derive(Debug)]
pub(super) struct PublicationMutationAuthority {
    /// Private pointer brand shared with this aggregate's publication ledgers.
    brand: Arc<PublicationAggregateBrand>,
}

impl PublicationMutationAuthority {
    /// Issues publication-mutation authority to the parent publication subaggregate.
    ///
    /// Returns the sole authority value owned by one `PublicationResources`.
    pub(super) fn new() -> Self {
        Self {
            brand: Arc::new(PublicationAggregateBrand { private: () }),
        }
    }

    /// Captures a non-authorizing identity for ledger and receipt binding.
    pub(super) fn identity(&self) -> PublicationAggregateIdentity {
        PublicationAggregateIdentity {
            brand: Arc::clone(&self.brand),
        }
    }

    /// Issues publication-mutation authority for focused ledger unit tests.
    ///
    /// Returns a proof that is constructible only while compiling crate tests.
    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        Self::new()
    }
}

/// Private allocation naming one exact Delivery-to-Reply clear handshake.
#[derive(Debug)]
struct PublicationClearRequestBrand {
    /// Private field preventing structural construction outside this module.
    private: (),
}

/// Semantic scope bound into one cross-ledger publication-clear request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicationClearScope {
    /// End of one Selected-session tenure.
    SelectedSessionReset,
    /// Permanent end of one TCP connection generation.
    GenerationEnd,
}

/// Delivery-retained identity of one exact global-clear request.
#[derive(Debug)]
pub(super) struct DeliveryClearRequestIdentity {
    /// Pointer nonce unique to one Delivery reset or close preparation.
    brand: Arc<PublicationClearRequestBrand>,
    /// Publication aggregate that issued this request.
    aggregate: PublicationAggregateIdentity,
    /// TCP generation whose resources must be cleared.
    generation: ConnectionGeneration,
    /// Selected-reset or generation-end semantics required by the request.
    scope: PublicationClearScope,
}

impl DeliveryClearRequestIdentity {
    /// Issues one split request: Delivery retains identity and Reply consumes work.
    pub(super) fn issue(
        aggregate: &PublicationAggregateIdentity,
        generation: ConnectionGeneration,
        scope: PublicationClearScope,
    ) -> (Self, Self) {
        let brand = Arc::new(PublicationClearRequestBrand { private: () });
        (
            Self {
                brand: Arc::clone(&brand),
                aggregate: aggregate.duplicate(),
                generation,
                scope,
            },
            Self {
                brand,
                aggregate: aggregate.duplicate(),
                generation,
                scope,
            },
        )
    }

    /// Returns whether `other` names this exact clear handshake.
    pub(super) fn exact_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.brand, &other.brand)
    }

    /// Returns whether this request belongs to `aggregate`.
    pub(super) fn matches_aggregate(&self, aggregate: &PublicationAggregateIdentity) -> bool {
        self.aggregate.exact_eq(aggregate)
    }

    /// Returns the TCP generation bound to this request.
    pub(super) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the semantic clear scope bound to this request.
    pub(super) const fn scope(&self) -> PublicationClearScope {
        self.scope
    }
}
