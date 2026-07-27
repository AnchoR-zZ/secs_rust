//! Owns bounded, generation-scoped reply capabilities inside the HSMS Core.
//!
//! The ledger stores only immutable reply contracts and their live publication
//! state. It does not own application payloads, delivery identities, commands,
//! writes, or runtime effects. A read-only use plan lets `CoreResources`
//! preflight other ledgers before the exact capability is consumed atomically.

use std::collections::BTreeMap;

use crate::hsms::{
    contracts::{ReplyToken, ReplyTokenClaim, ReplyTokenIssuer, ValidatedReplyTokenClaim},
    core::reply::{ReplyCapabilityMode, ReplyContract},
    model::ids::{ConnectionGeneration, ReplyCapabilityId, ReplyCapabilityIncarnation},
};

/// Failure constructing a logically bounded reply-capability ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyLedgerConfigError {
    /// A zero bound could never admit an inbound W=1 Primary.
    ZeroCapacity,
}

/// Live publication state retained for one reply capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyCapabilityState {
    /// The token is inside a publication attempt and cannot yet be consumed.
    PendingPublication,
    /// Publication completed successfully and the application may use the token.
    Available,
}

/// One live, Core-authoritative reply capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplyCapabilityEntry {
    /// Never-reused identity of this exact reservation.
    incarnation: ReplyCapabilityIncarnation,
    /// Immutable response-header authority compiled from the inbound Primary.
    contract: ReplyContract,
    /// Whether the application has successfully received the corresponding token.
    state: ReplyCapabilityState,
}

/// Failure reserving a new pending-publication capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyReserveError {
    /// The TCP generation ended and this ledger is permanently closed.
    Closing,
    /// The contract belongs to a different TCP generation.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation embedded in the supplied reply contract.
        actual: ConnectionGeneration,
    },
    /// The capability identity is already live in this ledger.
    DuplicateId {
        /// Active identity that cannot be registered again.
        capability_id: ReplyCapabilityId,
    },
    /// Pending and available capabilities already occupy the configured bound.
    CapacityExhausted {
        /// Maximum number of simultaneously live capabilities.
        capacity: usize,
    },
    /// Every representable reservation incarnation has already been issued.
    IncarnationExhausted,
}

/// Exact reservation ticket retained across application publication.
///
/// The ticket is intentionally opaque and move-only. CoreResources may borrow
/// it for duplicate publication completion, or turn the current live entry
/// into an exact revocation plan when publication fails.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a reply publication ticket must be published, revoked, or cleared by reset"]
pub(crate) struct ReplyPublicationTicket {
    /// Exact pending reservation captured at insertion.
    snapshot: ReplyEntrySnapshot,
}

impl ReplyPublicationTicket {
    /// Returns the generation that owns this pending reservation.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.snapshot.generation
    }

    /// Returns the public capability identity assigned to this reservation.
    pub(crate) const fn capability_id(&self) -> ReplyCapabilityId {
        self.snapshot.capability_id
    }
}

/// Exact artifacts created by one successful pending reservation.
///
/// CoreResources retains the publication ticket with its delivery state and
/// transfers the unique opaque token to the application only after publication
/// succeeds. Both artifacts carry the same private incarnation.
#[derive(Debug)]
#[must_use = "split the reservation into its publication ticket and application token"]
pub(crate) struct ReplyReservation {
    /// Exact ticket used to publish or explicitly revoke this reservation.
    publication_ticket: ReplyPublicationTicket,
    /// Unique opaque authority eventually transferred to the application.
    reply_token: ReplyToken,
}

impl ReplyReservation {
    /// Separates this reservation into its exact publication and application artifacts.
    pub(crate) fn into_parts(self) -> (ReplyPublicationTicket, ReplyToken) {
        (self.publication_ticket, self.reply_token)
    }
}

/// Result of completing the application-publication half of a capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "publication decisions determine whether reply authority became available"]
pub(crate) enum ReplyPublicationDecision {
    /// The exact pending capability became available.
    MadeAvailable,
    /// The exact capability was already available, so no state changed.
    AlreadyAvailable,
    /// No live capability owns the supplied identity.
    UnknownOrTerminal,
    /// The ID now names a later reservation, so no state changed.
    IncarnationChanged {
        /// Reused identity whose current incarnation differs from the ticket.
        capability_id: ReplyCapabilityId,
    },
    /// The ID and incarnation matched but the immutable contract did not.
    ContractChanged {
        /// Identity whose contract failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The completion belongs to a different TCP generation.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation supplied with the publication completion.
        actual: ConnectionGeneration,
    },
}

/// Reason an exact explicit-revocation plan could not be prepared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyRevocationUnavailable {
    /// The ticket belongs to a different TCP generation.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation captured by the stale ticket.
        actual: ConnectionGeneration,
    },
    /// The reservation was already consumed, revoked, reset, or never existed.
    UnknownOrTerminal,
    /// The ID now names a later reservation.
    IncarnationChanged {
        /// Reused identity whose incarnation failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The ID and incarnation matched but the immutable contract did not.
    ContractChanged {
        /// Identity whose contract failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
}

/// Exact, move-only plan for revoking one live capability.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "an explicit revocation plan must be committed or deliberately discarded"]
pub(crate) struct ReplyRevocationPlan {
    /// Exact live entry and state observed during revocation preparation.
    snapshot: ReplyRevocationSnapshot,
}

/// Successful explicit revocation of one exact live capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a revocation terminal releases capacity and reply authority"]
pub(crate) struct ReplyRevocationTerminal {
    /// Authoritative contract released by the revocation.
    contract: ReplyContract,
    /// Live state from which the capability was revoked.
    previous_state: ReplyCapabilityState,
}

impl ReplyRevocationTerminal {
    /// Returns the contract whose authority was removed.
    pub(crate) const fn contract(self) -> ReplyContract {
        self.contract
    }

    /// Returns the publication state observed immediately before removal.
    pub(crate) const fn previous_state(self) -> ReplyCapabilityState {
        self.previous_state
    }
}

/// Failure committing a revocation plan whose exact entry has changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyRevocationCommitError {
    /// The plan belongs to a different TCP generation.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation captured by the stale plan.
        actual: ConnectionGeneration,
    },
    /// The reservation was already consumed, revoked, reset, or never existed.
    UnknownOrTerminal {
        /// Identity that no longer names a live entry.
        capability_id: ReplyCapabilityId,
    },
    /// The ID now names a later reservation.
    IncarnationChanged {
        /// Reused identity whose incarnation failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The ID and incarnation matched but the immutable contract did not.
    ContractChanged {
        /// Identity whose contract failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The exact reservation changed publication state after preparation.
    StateChanged {
        /// Identity whose state failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
}

/// Application-selected use of one available reply capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyUseKind {
    /// Send the exact normal F+1 Secondary authorized by the contract.
    Normal,
    /// Send a header-only SxF0 transaction abort.
    Abort,
    /// Release the capability locally without writing a frame.
    Abandon,
}

/// Exact immutable identity shared by every cross-call reply plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplyEntrySnapshot {
    /// TCP generation whose ledger produced the plan.
    generation: ConnectionGeneration,
    /// Exact capability intended for the terminal transition.
    capability_id: ReplyCapabilityId,
    /// Never-reused identity of the reservation observed by the plan.
    incarnation: ReplyCapabilityIncarnation,
    /// Authoritative contract observed while the capability was available.
    contract: ReplyContract,
}

impl ReplyEntrySnapshot {
    /// Captures the exact reservation that every later mutation must revalidate.
    const fn new(
        generation: ConnectionGeneration,
        capability_id: ReplyCapabilityId,
        incarnation: ReplyCapabilityIncarnation,
        contract: ReplyContract,
    ) -> Self {
        Self {
            generation,
            capability_id,
            incarnation,
            contract,
        }
    }
}

/// Exact live entry and publication state captured by a revocation plan.
#[derive(Debug, PartialEq, Eq)]
struct ReplyRevocationSnapshot {
    /// Immutable identity of the reservation to revoke.
    entry: ReplyEntrySnapshot,
    /// Publication state that must remain unchanged until commit.
    state: ReplyCapabilityState,
}

/// Authorized, move-only plan for a normal F+1 Secondary.
///
/// Only a contract with a real F+1 function can construct this type. It is the
/// sole plan variant that exposes a normal-response contract to downstream
/// Operation and Write preflight.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "an authorized normal reply plan must be committed or explicitly discarded"]
pub(crate) struct AuthorizedNormalReplyPlan {
    /// Exact available capability captured for later commit.
    snapshot: ReplyEntrySnapshot,
}

impl AuthorizedNormalReplyPlan {
    /// Returns the generation whose live capability produced this plan.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.snapshot.generation
    }

    /// Returns the exact capability that must still be available at commit.
    pub(crate) const fn capability_id(&self) -> ReplyCapabilityId {
        self.snapshot.capability_id
    }

    /// Returns the Core-authoritative contract proven to support normal F+1.
    pub(crate) const fn contract(&self) -> ReplyContract {
        self.snapshot.contract
    }
}

/// Authorized, move-only plan for a header-only SxF0 abort.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "an authorized abort-reply plan must be committed or explicitly discarded"]
pub(crate) struct AuthorizedAbortReplyPlan {
    /// Exact available capability captured for later commit.
    snapshot: ReplyEntrySnapshot,
}

impl AuthorizedAbortReplyPlan {
    /// Returns the generation whose live capability produced this plan.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.snapshot.generation
    }

    /// Returns the exact capability that must still be available at commit.
    pub(crate) const fn capability_id(&self) -> ReplyCapabilityId {
        self.snapshot.capability_id
    }

    /// Returns the Core-authoritative contract used to build header-only SxF0.
    pub(crate) const fn contract(&self) -> ReplyContract {
        self.snapshot.contract
    }
}

/// Authorized, move-only plan for local reply-capability abandonment.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "an authorized abandon-reply plan must be committed or explicitly discarded"]
pub(crate) struct AuthorizedAbandonReplyPlan {
    /// Exact available capability captured for later commit.
    snapshot: ReplyEntrySnapshot,
}

impl AuthorizedAbandonReplyPlan {
    /// Returns the generation whose live capability produced this plan.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.snapshot.generation
    }

    /// Returns the exact capability that must still be available at commit.
    pub(crate) const fn capability_id(&self) -> ReplyCapabilityId {
        self.snapshot.capability_id
    }
}

/// Exhaustive, move-only authorized use plan for an available capability.
///
/// Callers must match the semantic variant before performing downstream
/// resource preflight. Only `Normal` and `Abort` need a protocol write, while
/// `Abandon` is a local terminal action.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "match the reply-use plan and commit its exact semantic variant"]
pub(crate) enum ReplyUsePlan {
    /// Normal F+1 response authority proven available by the stored contract.
    Normal(
        /// Opaque authorized normal-response plan.
        AuthorizedNormalReplyPlan,
    ),
    /// Header-only SxF0 authority valid for normal and abort-only contracts.
    Abort(
        /// Opaque authorized abort-response plan.
        AuthorizedAbortReplyPlan,
    ),
    /// Local capability release requiring no protocol write.
    Abandon(
        /// Opaque authorized abandonment plan.
        AuthorizedAbandonReplyPlan,
    ),
}

/// Result of authorizing one requested use against the current capability.
///
/// F255 normal use is terminal here: the ledger removes the capability inside
/// the same mutable call and returns only evidence of the completed revocation.
/// It can therefore never leak a discardable revoke plan to CoreResources.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "authorization either carries an exact use plan or an already-applied terminal"]
pub(crate) enum ReplyUseAuthorization {
    /// A normal, abort, or abandon use may proceed to downstream preflight.
    Authorized(
        /// Exact move-only plan that must be committed after preflight succeeds.
        ReplyUsePlan,
    ),
    /// F255 normal use was rejected and its abort-only authority already revoked.
    NormalRequiresAbortRevoked {
        /// Authoritative abort-only contract removed atomically.
        contract: ReplyContract,
    },
}

/// Reason an available-capability use could not be prepared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyUseUnavailable {
    /// The token was minted by a different reply-ledger instance.
    ForeignIssuer,
    /// The token targets a different TCP generation.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation supplied with the token.
        actual: ConnectionGeneration,
    },
    /// The token has not yet been transferred successfully to the application.
    PendingPublication,
    /// The token names an earlier reservation that reused the same public ID.
    IncarnationChanged {
        /// Reused identity whose private incarnation did not match.
        capability_id: ReplyCapabilityId,
    },
    /// The identity is unknown, already consumed, or already revoked.
    UnknownOrTerminal,
}

/// Successful terminal transition produced by committing an exact use plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "reply-use terminals determine completion and outbound-write behavior"]
pub(crate) enum ReplyUseTerminal {
    /// A valid normal, abort, or abandon action consumed the capability.
    Consumed {
        /// Authoritative contract removed from the ledger.
        contract: ReplyContract,
        /// Exact action that consumed the authority.
        use_kind: ReplyUseKind,
    },
}

/// Failure committing a plan that no longer names the exact available entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyUseCommitError {
    /// The plan was produced by a different generation-scoped ledger.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation captured by the stale plan.
        actual: ConnectionGeneration,
    },
    /// The capability was consumed, revoked, reset, or never registered.
    UnknownOrTerminal {
        /// Identity that no longer names a live entry.
        capability_id: ReplyCapabilityId,
    },
    /// The capability still exists but publication has not made it available.
    PendingPublication {
        /// Identity whose use cannot commit before publication.
        capability_id: ReplyCapabilityId,
    },
    /// The immutable contract no longer matches the one captured by the plan.
    ContractChanged {
        /// Identity whose authoritative contract failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The ID now names a later reservation.
    IncarnationChanged {
        /// Reused identity whose incarnation failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The requested use or derived outcome differs from the prepared plan.
    PlanChanged {
        /// Identity whose prepared decision failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
}

/// Counts live capabilities removed by a session or generation reset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[must_use = "reset summaries report how much live reply authority was revoked"]
pub(crate) struct ReplyResetSummary {
    /// Capabilities removed before their token publication completed.
    pending_publication: usize,
    /// Capabilities removed after their tokens reached the application.
    available: usize,
}

impl ReplyResetSummary {
    /// Returns how many pending-publication capabilities were revoked.
    pub(crate) const fn pending_publication(self) -> usize {
        self.pending_publication
    }

    /// Returns how many application-available capabilities were revoked.
    pub(crate) const fn available(self) -> usize {
        self.available
    }

    /// Returns the total number of live capabilities removed by the reset.
    pub(crate) const fn total(self) -> usize {
        self.pending_publication + self.available
    }
}

/// Bounded owner of all live reply authority for one TCP generation.
pub(crate) struct ReplyCapabilityLedger {
    /// TCP generation whose inbound W=1 Primaries may create entries.
    generation: ConnectionGeneration,
    /// Logical maximum across pending-publication and available entries.
    capacity: usize,
    /// Unforgeable mint whose brand authenticates application reply tokens.
    token_issuer: ReplyTokenIssuer,
    /// Permanent fence set when the owning TCP generation ends.
    closing: bool,
    /// Incarnation to issue next, or `None` after the complete domain is spent.
    next_incarnation: Option<ReplyCapabilityIncarnation>,
    /// Lazily allocated live entries keyed by their externally assigned IDs.
    entries: BTreeMap<ReplyCapabilityId, ReplyCapabilityEntry>,
}

impl ReplyCapabilityLedger {
    /// Creates an empty generation-scoped ledger with logical `capacity`.
    ///
    /// The backing tree allocates lazily, so even `usize::MAX` does not request
    /// a proportional allocation. Zero is rejected because it cannot support
    /// reliable publication of any inbound W=1 Primary.
    pub(crate) fn new(
        generation: ConnectionGeneration,
        capacity: usize,
    ) -> Result<Self, ReplyLedgerConfigError> {
        if capacity == 0 {
            return Err(ReplyLedgerConfigError::ZeroCapacity);
        }
        Ok(Self {
            generation,
            capacity,
            token_issuer: ReplyTokenIssuer::new(),
            closing: false,
            next_incarnation: Some(ReplyCapabilityIncarnation::new(1)),
            entries: BTreeMap::new(),
        })
    }

    /// Returns the TCP generation exclusively owned by this ledger.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the logical maximum number of simultaneously live capabilities.
    pub(crate) const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the combined number of pending-publication and available entries.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the ledger currently owns no live capability.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns whether the generation-end closing fence has been raised.
    pub(crate) const fn is_closing(&self) -> bool {
        self.closing
    }

    /// Returns whether another pending capability can be reserved.
    pub(crate) fn has_capacity(&self) -> bool {
        !self.closing && self.next_incarnation.is_some() && self.entries.len() < self.capacity
    }

    /// Registers `capability_id` as pending publication under `contract`.
    ///
    /// The returned ticket names this exact reservation rather than only its
    /// externally assigned ID. Wrong generation, duplicate identity, exhausted
    /// capacity, and exhausted incarnation space leave the ledger unchanged.
    pub(crate) fn reserve_pending(
        &mut self,
        capability_id: ReplyCapabilityId,
        contract: ReplyContract,
    ) -> Result<ReplyReservation, ReplyReserveError> {
        if self.closing {
            return Err(ReplyReserveError::Closing);
        }
        if contract.generation() != self.generation {
            return Err(ReplyReserveError::WrongGeneration {
                expected: self.generation,
                actual: contract.generation(),
            });
        }

        if self.entries.contains_key(&capability_id) {
            return Err(ReplyReserveError::DuplicateId { capability_id });
        }
        if self.entries.len() >= self.capacity {
            return Err(ReplyReserveError::CapacityExhausted {
                capacity: self.capacity,
            });
        }
        let Some(incarnation) = self.next_incarnation else {
            return Err(ReplyReserveError::IncarnationExhausted);
        };

        let snapshot =
            ReplyEntrySnapshot::new(self.generation, capability_id, incarnation, contract);
        self.entries.insert(
            capability_id,
            ReplyCapabilityEntry {
                incarnation,
                contract,
                state: ReplyCapabilityState::PendingPublication,
            },
        );
        self.next_incarnation = incarnation
            .get()
            .checked_add(1)
            .map(ReplyCapabilityIncarnation::new);
        Ok(ReplyReservation {
            publication_ticket: ReplyPublicationTicket { snapshot },
            reply_token: self.token_issuer.issue(
                capability_id,
                self.generation,
                incarnation,
                contract.supports_normal_secondary(),
            ),
        })
    }

    /// Makes the exact pending capability available after successful publication.
    ///
    /// The borrowed reservation ticket lets duplicate completion remain
    /// idempotent while preventing a delayed completion from publishing a later
    /// reservation that reused the same external ID and immutable contract.
    pub(crate) fn mark_available(
        &mut self,
        ticket: &ReplyPublicationTicket,
    ) -> ReplyPublicationDecision {
        let snapshot = ticket.snapshot;
        if snapshot.generation != self.generation {
            return ReplyPublicationDecision::WrongGeneration {
                expected: self.generation,
                actual: snapshot.generation,
            };
        }
        let Some(entry) = self.entries.get_mut(&snapshot.capability_id) else {
            return ReplyPublicationDecision::UnknownOrTerminal;
        };
        if entry.incarnation != snapshot.incarnation {
            return ReplyPublicationDecision::IncarnationChanged {
                capability_id: snapshot.capability_id,
            };
        }
        if entry.contract != snapshot.contract {
            return ReplyPublicationDecision::ContractChanged {
                capability_id: snapshot.capability_id,
            };
        }
        match entry.state {
            ReplyCapabilityState::PendingPublication => {
                entry.state = ReplyCapabilityState::Available;
                ReplyPublicationDecision::MadeAvailable
            }
            ReplyCapabilityState::Available => ReplyPublicationDecision::AlreadyAvailable,
        }
    }

    /// Prepares a move-only revocation plan from an exact reservation ticket.
    ///
    /// Preparation captures the current publication state as well as the
    /// reservation incarnation. Any stale ticket leaves the ledger unchanged.
    pub(crate) fn prepare_revocation(
        &self,
        ticket: &ReplyPublicationTicket,
    ) -> Result<ReplyRevocationPlan, ReplyRevocationUnavailable> {
        let snapshot = ticket.snapshot;
        if snapshot.generation != self.generation {
            return Err(ReplyRevocationUnavailable::WrongGeneration {
                expected: self.generation,
                actual: snapshot.generation,
            });
        }
        let Some(entry) = self.entries.get(&snapshot.capability_id) else {
            return Err(ReplyRevocationUnavailable::UnknownOrTerminal);
        };
        if entry.incarnation != snapshot.incarnation {
            return Err(ReplyRevocationUnavailable::IncarnationChanged {
                capability_id: snapshot.capability_id,
            });
        }
        if entry.contract != snapshot.contract {
            return Err(ReplyRevocationUnavailable::ContractChanged {
                capability_id: snapshot.capability_id,
            });
        }
        Ok(ReplyRevocationPlan {
            snapshot: ReplyRevocationSnapshot {
                entry: snapshot,
                state: entry.state,
            },
        })
    }

    /// Commits one exact explicit-revocation plan and releases its capacity.
    ///
    /// Generation, ID, incarnation, contract, and publication state must all
    /// still match. Every failure leaves all current entries unchanged.
    pub(crate) fn commit_revocation(
        &mut self,
        plan: ReplyRevocationPlan,
    ) -> Result<ReplyRevocationTerminal, ReplyRevocationCommitError> {
        let snapshot = plan.snapshot;
        if snapshot.entry.generation != self.generation {
            return Err(ReplyRevocationCommitError::WrongGeneration {
                expected: self.generation,
                actual: snapshot.entry.generation,
            });
        }
        let Some(entry) = self.entries.get(&snapshot.entry.capability_id) else {
            return Err(ReplyRevocationCommitError::UnknownOrTerminal {
                capability_id: snapshot.entry.capability_id,
            });
        };
        if entry.incarnation != snapshot.entry.incarnation {
            return Err(ReplyRevocationCommitError::IncarnationChanged {
                capability_id: snapshot.entry.capability_id,
            });
        }
        if entry.contract != snapshot.entry.contract {
            return Err(ReplyRevocationCommitError::ContractChanged {
                capability_id: snapshot.entry.capability_id,
            });
        }
        if entry.state != snapshot.state {
            return Err(ReplyRevocationCommitError::StateChanged {
                capability_id: snapshot.entry.capability_id,
            });
        }

        let Some(entry) = self.entries.remove(&snapshot.entry.capability_id) else {
            return Err(ReplyRevocationCommitError::UnknownOrTerminal {
                capability_id: snapshot.entry.capability_id,
            });
        };
        Ok(ReplyRevocationTerminal {
            contract: entry.contract,
            previous_state: entry.state,
        })
    }

    /// Authorizes an exact available capability use.
    ///
    /// This operation trusts only the stored [`ReplyContract`] and never reads
    /// the non-authoritative public token hint. Normal use of an F255 contract
    /// removes the capability inside this same mutable call; every other valid
    /// use returns a move-only plan for downstream preflight and exact commit.
    pub(crate) fn prepare_use(
        &mut self,
        token: ReplyToken,
        use_kind: ReplyUseKind,
    ) -> Result<ReplyUseAuthorization, ReplyUseUnavailable> {
        let claim: ReplyTokenClaim = token.into_claim();
        let validated: ValidatedReplyTokenClaim = self
            .token_issuer
            .validate_claim(claim)
            .map_err(|_foreign| ReplyUseUnavailable::ForeignIssuer)?;
        let (generation, capability_id, incarnation) = validated.into_parts();
        if generation != self.generation {
            return Err(ReplyUseUnavailable::WrongGeneration {
                expected: self.generation,
                actual: generation,
            });
        }
        let Some(entry) = self.entries.get(&capability_id).copied() else {
            return Err(ReplyUseUnavailable::UnknownOrTerminal);
        };
        if entry.incarnation != incarnation {
            return Err(ReplyUseUnavailable::IncarnationChanged { capability_id });
        }
        if entry.state == ReplyCapabilityState::PendingPublication {
            return Err(ReplyUseUnavailable::PendingPublication);
        }

        let snapshot =
            ReplyEntrySnapshot::new(generation, capability_id, incarnation, entry.contract);
        let authorization =
            match (use_kind, entry.contract.mode()) {
                (ReplyUseKind::Normal, ReplyCapabilityMode::NormalSecondary { .. }) => {
                    ReplyUseAuthorization::Authorized(ReplyUsePlan::Normal(
                        AuthorizedNormalReplyPlan { snapshot },
                    ))
                }
                (ReplyUseKind::Normal, ReplyCapabilityMode::AbortOnly) => {
                    let Some(removed) = self.entries.remove(&capability_id) else {
                        return Err(ReplyUseUnavailable::UnknownOrTerminal);
                    };
                    ReplyUseAuthorization::NormalRequiresAbortRevoked {
                        contract: removed.contract,
                    }
                }
                (ReplyUseKind::Abort, _) => ReplyUseAuthorization::Authorized(ReplyUsePlan::Abort(
                    AuthorizedAbortReplyPlan { snapshot },
                )),
                (ReplyUseKind::Abandon, _) => ReplyUseAuthorization::Authorized(
                    ReplyUsePlan::Abandon(AuthorizedAbandonReplyPlan { snapshot }),
                ),
            };
        Ok(authorization)
    }

    /// Commits one prepared use after revalidating every captured field.
    ///
    /// Success removes exactly one available entry. Generation, ID,
    /// incarnation, contract, publication state, and authorized use kind are
    /// all revalidated. Every stale-plan error leaves all entries unchanged.
    pub(crate) fn commit_use(
        &mut self,
        plan: ReplyUsePlan,
    ) -> Result<ReplyUseTerminal, ReplyUseCommitError> {
        let (snapshot, use_kind) = match plan {
            ReplyUsePlan::Normal(plan) => (plan.snapshot, ReplyUseKind::Normal),
            ReplyUsePlan::Abort(plan) => (plan.snapshot, ReplyUseKind::Abort),
            ReplyUsePlan::Abandon(plan) => (plan.snapshot, ReplyUseKind::Abandon),
        };

        if snapshot.generation != self.generation {
            return Err(ReplyUseCommitError::WrongGeneration {
                expected: self.generation,
                actual: snapshot.generation,
            });
        }
        let Some(entry) = self.entries.get(&snapshot.capability_id) else {
            return Err(ReplyUseCommitError::UnknownOrTerminal {
                capability_id: snapshot.capability_id,
            });
        };
        if entry.incarnation != snapshot.incarnation {
            return Err(ReplyUseCommitError::IncarnationChanged {
                capability_id: snapshot.capability_id,
            });
        }
        if entry.contract != snapshot.contract {
            return Err(ReplyUseCommitError::ContractChanged {
                capability_id: snapshot.capability_id,
            });
        }
        if entry.state == ReplyCapabilityState::PendingPublication {
            return Err(ReplyUseCommitError::PendingPublication {
                capability_id: snapshot.capability_id,
            });
        }
        if !Self::use_kind_matches_contract(entry.contract, use_kind) {
            return Err(ReplyUseCommitError::PlanChanged {
                capability_id: snapshot.capability_id,
            });
        }

        let Some(entry) = self.entries.remove(&snapshot.capability_id) else {
            return Err(ReplyUseCommitError::UnknownOrTerminal {
                capability_id: snapshot.capability_id,
            });
        };
        Ok(ReplyUseTerminal::Consumed {
            contract: entry.contract,
            use_kind,
        })
    }

    /// Revokes every live capability when the TCP generation ends.
    ///
    /// No completion or effect is produced; the owning reducer separately
    /// decides generation-close effects and application command outcomes.
    pub(crate) fn clear_for_generation_end(&mut self) -> ReplyResetSummary {
        self.closing = true;
        self.clear_live_entries()
    }

    /// Revokes every live capability when the session leaves `Selected`.
    ///
    /// This prevents authority from an earlier Selected tenure becoming usable
    /// after a later Select procedure on the same TCP generation.
    pub(crate) fn clear_for_selected_session_reset(&mut self) -> ReplyResetSummary {
        self.clear_live_entries()
    }

    /// Revalidates that the exact opaque use variant agrees with Core authority.
    const fn use_kind_matches_contract(contract: ReplyContract, use_kind: ReplyUseKind) -> bool {
        match use_kind {
            ReplyUseKind::Normal => {
                matches!(contract.mode(), ReplyCapabilityMode::NormalSecondary { .. })
            }
            ReplyUseKind::Abort | ReplyUseKind::Abandon => true,
        }
    }

    /// Counts and removes every pending or available entry without allocating.
    fn clear_live_entries(&mut self) -> ReplyResetSummary {
        let mut summary = ReplyResetSummary::default();
        for entry in self.entries.values() {
            match entry.state {
                ReplyCapabilityState::PendingPublication => {
                    summary.pending_publication += 1;
                }
                ReplyCapabilityState::Available => {
                    summary.available += 1;
                }
            }
        }
        self.entries.clear();
        // Never reset the incarnation sequence: delayed plans must remain stale
        // when the same external ID and contract are reserved after a reset.
        summary
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        contracts::{ReplyToken, ReplyTokenIssuer},
        core::reply::{
            ReplyCapabilityLedger, ReplyCapabilityMode, ReplyCapabilityState,
            ReplyLedgerConfigError, ReplyPublicationDecision, ReplyPublicationTicket,
            ReplyReserveError, ReplyRevocationCommitError, ReplyRevocationUnavailable,
            ReplyUseAuthorization, ReplyUseCommitError, ReplyUseKind, ReplyUsePlan,
            ReplyUseTerminal, ReplyUseUnavailable,
        },
        model::ids::{ConnectionGeneration, ReplyCapabilityId, SystemBytes},
        Function, SessionId, Stream,
    };

    use super::{ReplyCapabilityIncarnation, ReplyContract};

    /// Creates a deterministic reply contract for the supplied generation and function.
    fn contract(generation: u64, function: u8) -> ReplyContract {
        ReplyContract::from_primary_parts(
            ConnectionGeneration::new(generation),
            SessionId::new(7).expect("valid Data Session ID"),
            Stream::new(3).expect("valid stream"),
            Function::new(function),
            true,
            SystemBytes::new(0x0102_0304),
        )
        .expect("odd W=1 Primary")
    }

    /// Creates a non-zero-capacity ledger owned by generation seven.
    fn ledger(capacity: usize) -> ReplyCapabilityLedger {
        ReplyCapabilityLedger::new(ConnectionGeneration::new(7), capacity)
            .expect("non-zero logical capacity")
    }

    /// Splits one pending reservation into its exact ticket and opaque token.
    fn reserve_parts(
        ledger: &mut ReplyCapabilityLedger,
        id: u64,
        function: u8,
    ) -> (ReplyPublicationTicket, ReplyToken) {
        let capability_id = ReplyCapabilityId::new(id);
        ledger
            .reserve_pending(capability_id, contract(7, function))
            .expect("capacity available")
            .into_parts()
    }

    /// Reserves and publishes one capability, returning all exact artifacts.
    fn available(
        ledger: &mut ReplyCapabilityLedger,
        id: u64,
        function: u8,
    ) -> (ReplyCapabilityId, ReplyPublicationTicket, ReplyToken) {
        let capability_id = ReplyCapabilityId::new(id);
        let (ticket, token) = reserve_parts(ledger, id, function);
        assert_eq!(
            ledger.mark_available(&ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        (capability_id, ticket, token)
    }

    /// Exhaustively matches one authorized semantic plan, verifies its
    /// read-only fields, and returns the same move-only variant for commit.
    fn verify_authorized_plan(
        authorization: ReplyUseAuthorization,
        expected_kind: ReplyUseKind,
        expected_id: ReplyCapabilityId,
        expected_contract: ReplyContract,
    ) -> ReplyUsePlan {
        let ReplyUseAuthorization::Authorized(plan) = authorization else {
            panic!("expected an authorized plan, got {authorization:?}");
        };
        match (plan, expected_kind) {
            (ReplyUsePlan::Normal(plan), ReplyUseKind::Normal) => {
                assert_eq!(plan.generation(), ConnectionGeneration::new(7));
                assert_eq!(plan.capability_id(), expected_id);
                assert_eq!(plan.contract(), expected_contract);
                assert!(matches!(
                    plan.contract().mode(),
                    ReplyCapabilityMode::NormalSecondary { .. }
                ));
                ReplyUsePlan::Normal(plan)
            }
            (ReplyUsePlan::Abort(plan), ReplyUseKind::Abort) => {
                assert_eq!(plan.generation(), ConnectionGeneration::new(7));
                assert_eq!(plan.capability_id(), expected_id);
                assert_eq!(plan.contract(), expected_contract);
                ReplyUsePlan::Abort(plan)
            }
            (ReplyUsePlan::Abandon(plan), ReplyUseKind::Abandon) => {
                assert_eq!(plan.generation(), ConnectionGeneration::new(7));
                assert_eq!(plan.capability_id(), expected_id);
                ReplyUsePlan::Abandon(plan)
            }
            (unexpected, expected_kind) => {
                panic!("unexpected plan {unexpected:?} for {expected_kind:?}")
            }
        }
    }

    /// Confirms zero capacity is rejected while an extreme logical capacity
    /// does not eagerly allocate proportional memory.
    #[test]
    fn construction_is_structured_and_lazily_bounded() {
        assert!(matches!(
            ReplyCapabilityLedger::new(ConnectionGeneration::new(7), 0),
            Err(ReplyLedgerConfigError::ZeroCapacity)
        ));

        let mut ledger = ledger(usize::MAX);
        assert_eq!(ledger.capacity(), usize::MAX);
        assert!(ledger.is_empty());
        let _reservation = ledger
            .reserve_pending(ReplyCapabilityId::new(1), contract(7, 1))
            .expect("one lazy tree node fits");
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms pending and available entries share one logical capacity and
    /// every failed reservation leaves existing state intact.
    #[test]
    fn capacity_counts_both_live_states_and_reservation_is_atomic() {
        let mut ledger = ledger(2);
        let first = ReplyCapabilityId::new(1);
        let second = ReplyCapabilityId::new(2);
        let _first_reservation = ledger
            .reserve_pending(first, contract(7, 1))
            .expect("first entry");
        let (second_ticket, _second_token) = ledger
            .reserve_pending(second, contract(7, 3))
            .expect("second entry")
            .into_parts();
        assert_eq!(
            ledger.mark_available(&second_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        assert!(!ledger.has_capacity());

        assert!(matches!(
            ledger.reserve_pending(ReplyCapabilityId::new(3), contract(7, 5)),
            Err(ReplyReserveError::CapacityExhausted { capacity: 2 })
        ));
        assert!(matches!(
            ledger.reserve_pending(first, contract(7, 1)),
            Err(ReplyReserveError::DuplicateId { capability_id }) if capability_id == first
        ));
        assert!(matches!(
            ledger.reserve_pending(ReplyCapabilityId::new(4), contract(8, 1)),
            Err(ReplyReserveError::WrongGeneration {
                expected,
                actual,
            }) if expected == ConnectionGeneration::new(7)
                && actual == ConnectionGeneration::new(8)
        ));
        assert_eq!(ledger.len(), 2);
    }

    /// Confirms publication permits only the exact Pending-to-Available
    /// transition and treats repeats and unknown IDs without mutation.
    #[test]
    fn publication_transition_is_exact_and_idempotent() {
        let mut ledger = ledger(2);
        let capability_id = ReplyCapabilityId::new(1);
        let (ticket, _token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("pending capability")
            .into_parts();
        let mut foreign =
            ReplyCapabilityLedger::new(ConnectionGeneration::new(8), 1).expect("foreign ledger");
        let (foreign_ticket, _foreign_token) = foreign
            .reserve_pending(capability_id, contract(8, 1))
            .expect("foreign pending capability")
            .into_parts();

        assert_eq!(
            ledger.mark_available(&foreign_ticket),
            ReplyPublicationDecision::WrongGeneration {
                expected: ConnectionGeneration::new(7),
                actual: ConnectionGeneration::new(8),
            }
        );
        assert_eq!(
            ledger.mark_available(&ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        assert_eq!(
            ledger.mark_available(&ticket),
            ReplyPublicationDecision::AlreadyAvailable
        );
        let revoke = ledger
            .prepare_revocation(&ticket)
            .expect("available capability can be revoked");
        let _terminal = ledger
            .commit_revocation(revoke)
            .expect("exact revocation succeeds");
        assert_eq!(
            ledger.mark_available(&ticket),
            ReplyPublicationDecision::UnknownOrTerminal
        );
        assert!(ledger.is_empty());
    }

    /// Confirms pending publication cannot be planned or accidentally consumed.
    #[test]
    fn pending_capability_cannot_be_used() {
        let mut ledger = ledger(1);
        let capability_id = ReplyCapabilityId::new(1);
        let (_ticket, token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("pending capability")
            .into_parts();

        assert_eq!(
            ledger.prepare_use(token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::PendingPublication)
        );
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms normal, abort, and abandon plans consume available authority
    /// once and return the exact stored contract and selected use.
    #[test]
    fn authorized_use_kinds_consume_exactly_once() {
        for (index, use_kind) in [
            ReplyUseKind::Normal,
            ReplyUseKind::Abort,
            ReplyUseKind::Abandon,
        ]
        .into_iter()
        .enumerate()
        {
            let mut ledger = ledger(1);
            let (capability_id, _ticket, token) = available(&mut ledger, index as u64 + 1, 1);
            let plan = ledger
                .prepare_use(token, use_kind)
                .expect("available capability");
            let plan = verify_authorized_plan(plan, use_kind, capability_id, contract(7, 1));

            assert_eq!(
                ledger.commit_use(plan),
                Ok(ReplyUseTerminal::Consumed {
                    contract: contract(7, 1),
                    use_kind,
                })
            );
            assert!(ledger.is_empty());
            assert!(ledger.has_capacity());
        }
    }

    /// Confirms F255 normal use revokes inside the authorization call and
    /// returns no discardable plan or authorized normal-contract path.
    #[test]
    fn function_255_normal_has_only_reject_and_revoke_path() {
        let mut ledger = ledger(1);
        let (_capability_id, _ticket, token) = available(&mut ledger, 1, 255);
        let authorization = ledger
            .prepare_use(token, ReplyUseKind::Normal)
            .expect("published F255 capability");

        match authorization {
            ReplyUseAuthorization::NormalRequiresAbortRevoked { contract: removed } => {
                assert_eq!(removed, contract(7, 255));
            }
            ReplyUseAuthorization::Authorized(ReplyUsePlan::Normal(_)) => {
                panic!("F255 must never expose an authorized normal-response plan")
            }
            unexpected => panic!("unexpected F255 normal authorization: {unexpected:?}"),
        }
        assert!(ledger.is_empty());
        assert!(ledger.has_capacity());
    }

    /// Confirms F255 still authorizes both header-only abort and local abandon.
    #[test]
    fn function_255_allows_abort_and_abandon_once() {
        for use_kind in [ReplyUseKind::Abort, ReplyUseKind::Abandon] {
            let mut ledger = ledger(1);
            let (capability_id, _ticket, token) = available(&mut ledger, 1, 255);
            let plan = ledger
                .prepare_use(token, use_kind)
                .expect("abort-only capability supports this use");
            let plan = verify_authorized_plan(plan, use_kind, capability_id, contract(7, 255));
            assert_eq!(
                ledger.commit_use(plan),
                Ok(ReplyUseTerminal::Consumed {
                    contract: contract(7, 255),
                    use_kind,
                })
            );
            assert!(ledger.is_empty());
        }
    }

    /// Confirms a wrong-generation token and unknown identity cannot produce a
    /// plan or consume a numerically matching live capability.
    #[test]
    fn wrong_generation_and_unknown_use_are_non_mutating() {
        let mut ledger = ledger(1);
        let (capability_id, ticket, _current_token) = available(&mut ledger, 1, 1);
        let wrong_generation_token = ledger.token_issuer.issue(
            capability_id,
            ConnectionGeneration::new(8),
            ticket.snapshot.incarnation,
            true,
        );
        let unknown_token = ledger.token_issuer.issue(
            ReplyCapabilityId::new(99),
            ConnectionGeneration::new(7),
            ticket.snapshot.incarnation,
            true,
        );

        assert_eq!(
            ledger.prepare_use(wrong_generation_token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::WrongGeneration {
                expected: ConnectionGeneration::new(7),
                actual: ConnectionGeneration::new(8),
            })
        );
        assert_eq!(
            ledger.prepare_use(unknown_token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::UnknownOrTerminal)
        );
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms a foreign issuer cannot authorize a token even when every
    /// visible and private numeric identity field equals the live reservation.
    #[test]
    fn foreign_issuer_token_with_exact_numeric_identity_is_non_mutating() {
        let mut ledger = ledger(1);
        let (capability_id, ticket, current_token) = available(&mut ledger, 1, 1);
        let foreign_issuer = ReplyTokenIssuer::new();
        let foreign_token = foreign_issuer.issue(
            capability_id,
            ConnectionGeneration::new(7),
            ticket.snapshot.incarnation,
            true,
        );

        assert_eq!(
            ledger.prepare_use(foreign_token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::ForeignIssuer)
        );
        assert_eq!(ledger.len(), 1);

        let current = ledger
            .prepare_use(current_token, ReplyUseKind::Normal)
            .expect("owning issuer token remains usable");
        let current =
            verify_authorized_plan(current, ReplyUseKind::Normal, capability_id, contract(7, 1));
        assert!(matches!(
            ledger.commit_use(current),
            Ok(ReplyUseTerminal::Consumed { .. })
        ));
    }

    /// Confirms an exact use plan becomes stale after explicit revocation.
    #[test]
    fn use_plan_becomes_stale_after_explicit_revocation() {
        let mut ledger = ledger(1);
        let (capability_id, ticket, token) = available(&mut ledger, 1, 1);
        let authorization = ledger
            .prepare_use(token, ReplyUseKind::Normal)
            .expect("exact use plan");
        let stale_plan = verify_authorized_plan(
            authorization,
            ReplyUseKind::Normal,
            capability_id,
            contract(7, 1),
        );
        let revocation = ledger
            .prepare_revocation(&ticket)
            .expect("exact revocation plan");

        let _terminal = ledger
            .commit_revocation(revocation)
            .expect("revocation wins");
        assert_eq!(
            ledger.commit_use(stale_plan),
            Err(ReplyUseCommitError::UnknownOrTerminal { capability_id })
        );
        assert!(ledger.is_empty());
    }

    /// Confirms revocation removes either live state, restores capacity, and
    /// rejects wrong-generation or duplicate requests without side effects.
    #[test]
    fn revocation_is_exact_and_terminal() {
        let mut ledger = ledger(2);
        let pending = ReplyCapabilityId::new(1);
        let published = ReplyCapabilityId::new(2);
        let (pending_ticket, _pending_token) = ledger
            .reserve_pending(pending, contract(7, 1))
            .expect("pending entry")
            .into_parts();
        let (published_ticket, _published_token) = ledger
            .reserve_pending(published, contract(7, 3))
            .expect("published entry")
            .into_parts();
        assert_eq!(
            ledger.mark_available(&published_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        let mut foreign =
            ReplyCapabilityLedger::new(ConnectionGeneration::new(8), 1).expect("foreign ledger");
        let (foreign_ticket, _foreign_token) = foreign
            .reserve_pending(pending, contract(8, 1))
            .expect("foreign pending entry")
            .into_parts();

        assert_eq!(
            ledger.prepare_revocation(&foreign_ticket),
            Err(ReplyRevocationUnavailable::WrongGeneration {
                expected: ConnectionGeneration::new(7),
                actual: ConnectionGeneration::new(8),
            })
        );
        let pending_plan = ledger
            .prepare_revocation(&pending_ticket)
            .expect("pending revocation plan");
        let pending_terminal = ledger
            .commit_revocation(pending_plan)
            .expect("pending revocation commits");
        assert_eq!(pending_terminal.contract(), contract(7, 1));
        assert_eq!(
            pending_terminal.previous_state(),
            ReplyCapabilityState::PendingPublication
        );
        let published_plan = ledger
            .prepare_revocation(&published_ticket)
            .expect("available revocation plan");
        let published_terminal = ledger
            .commit_revocation(published_plan)
            .expect("available revocation commits");
        assert_eq!(published_terminal.contract(), contract(7, 3));
        assert_eq!(
            published_terminal.previous_state(),
            ReplyCapabilityState::Available
        );
        assert_eq!(
            ledger.prepare_revocation(&published_ticket),
            Err(ReplyRevocationUnavailable::UnknownOrTerminal)
        );
        assert!(ledger.is_empty());
        assert!(ledger.has_capacity());
    }

    /// Confirms generation end invalidates plans, reports both live states,
    /// raises a permanent closing fence, and rejects every later reservation.
    #[test]
    fn generation_end_reports_states_and_permanently_closes_ledger() {
        let mut ledger = ledger(2);
        let pending = ReplyCapabilityId::new(1);
        let _pending_reservation = ledger
            .reserve_pending(pending, contract(7, 1))
            .expect("pending entry");
        let (published, _published_ticket, published_token) = available(&mut ledger, 2, 3);
        let stale_authorization = ledger
            .prepare_use(published_token, ReplyUseKind::Abort)
            .expect("available entry");
        let stale_plan = verify_authorized_plan(
            stale_authorization,
            ReplyUseKind::Abort,
            published,
            contract(7, 3),
        );

        let summary = ledger.clear_for_generation_end();
        assert_eq!(summary.pending_publication(), 1);
        assert_eq!(summary.available(), 1);
        assert_eq!(summary.total(), 2);
        assert!(ledger.is_empty());
        assert!(ledger.is_closing());
        assert!(!ledger.has_capacity());
        assert_eq!(
            ledger.commit_use(stale_plan),
            Err(ReplyUseCommitError::UnknownOrTerminal {
                capability_id: published
            })
        );

        assert!(matches!(
            ledger.reserve_pending(ReplyCapabilityId::new(3), contract(7, 5)),
            Err(ReplyReserveError::Closing)
        ));
        assert!(ledger.is_empty());
    }

    /// Confirms stale application tokens, publication tickets, use plans, and
    /// revocation plans cannot mutate same-ID, same-contract reconstructions.
    #[test]
    fn incarnation_closes_same_id_same_contract_aba_for_every_cross_call_artifact() {
        let mut ledger = ledger(1);
        let capability_id = ReplyCapabilityId::new(41);
        let (original_ticket, stale_token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("original reservation")
            .into_parts();
        assert_eq!(
            ledger.mark_available(&original_ticket),
            ReplyPublicationDecision::MadeAvailable
        );

        let stale_revocation = ledger
            .prepare_revocation(&original_ticket)
            .expect("original stale revocation plan");
        let removal = ledger
            .prepare_revocation(&original_ticket)
            .expect("original removal plan");
        let _terminal = ledger
            .commit_revocation(removal)
            .expect("remove original reservation");

        let (second_ticket, second_token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("first same-ID reconstruction")
            .into_parts();
        assert_eq!(
            ledger.mark_available(&original_ticket),
            ReplyPublicationDecision::IncarnationChanged { capability_id }
        );
        assert_eq!(
            ledger.prepare_use(stale_token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::IncarnationChanged { capability_id })
        );
        assert_eq!(
            ledger.mark_available(&second_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        assert_eq!(
            ledger.commit_revocation(stale_revocation),
            Err(ReplyRevocationCommitError::IncarnationChanged { capability_id })
        );

        let second_authorization = ledger
            .prepare_use(second_token, ReplyUseKind::Normal)
            .expect("second reservation authorization");
        let stale_use = verify_authorized_plan(
            second_authorization,
            ReplyUseKind::Normal,
            capability_id,
            contract(7, 1),
        );
        let second_removal = ledger
            .prepare_revocation(&second_ticket)
            .expect("second reservation removal plan");
        let _terminal = ledger
            .commit_revocation(second_removal)
            .expect("remove second reservation");

        let (current_ticket, current_token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("second same-ID reconstruction")
            .into_parts();
        assert_eq!(
            ledger.mark_available(&current_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        assert_eq!(
            ledger.commit_use(stale_use),
            Err(ReplyUseCommitError::IncarnationChanged { capability_id })
        );
        assert_eq!(ledger.len(), 1);

        let current_authorization = ledger
            .prepare_use(current_token, ReplyUseKind::Normal)
            .expect("current reservation remains available");
        let current_use = verify_authorized_plan(
            current_authorization,
            ReplyUseKind::Normal,
            capability_id,
            contract(7, 1),
        );
        assert!(matches!(
            ledger.commit_use(current_use),
            Ok(ReplyUseTerminal::Consumed { .. })
        ));
        assert!(ledger.is_empty());
    }

    /// Confirms a revocation plan also binds the publication state observed at
    /// preparation and cannot remove an entry after that state changes.
    #[test]
    fn revocation_plan_revalidates_publication_state() {
        let mut ledger = ledger(1);
        let capability_id = ReplyCapabilityId::new(51);
        let (ticket, _token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("pending reservation")
            .into_parts();
        let stale_pending_plan = ledger
            .prepare_revocation(&ticket)
            .expect("pending revocation plan");
        assert_eq!(
            ledger.mark_available(&ticket),
            ReplyPublicationDecision::MadeAvailable
        );

        assert_eq!(
            ledger.commit_revocation(stale_pending_plan),
            Err(ReplyRevocationCommitError::StateChanged { capability_id })
        );
        let current_plan = ledger
            .prepare_revocation(&ticket)
            .expect("current available revocation plan");
        let terminal = ledger
            .commit_revocation(current_plan)
            .expect("current state commits");
        assert_eq!(terminal.previous_state(), ReplyCapabilityState::Available);
        assert!(ledger.is_empty());
    }

    /// Confirms the final representable incarnation is issued once and the
    /// following reservation fails structurally without wrapping to an old ID.
    #[test]
    fn incarnation_exhaustion_is_structured_and_never_wraps() {
        let mut ledger = ledger(1);
        ledger.next_incarnation = Some(ReplyCapabilityIncarnation::new(u64::MAX));
        let (final_ticket, _final_token) = ledger
            .reserve_pending(ReplyCapabilityId::new(61), contract(7, 1))
            .expect("final representable incarnation is usable")
            .into_parts();
        assert_eq!(final_ticket.snapshot.incarnation.get(), u64::MAX);
        assert!(ledger.next_incarnation.is_none());
        let removal = ledger
            .prepare_revocation(&final_ticket)
            .expect("final reservation revocation");
        let _terminal = ledger
            .commit_revocation(removal)
            .expect("remove final reservation");

        assert!(!ledger.has_capacity());
        assert!(matches!(
            ledger.reserve_pending(ReplyCapabilityId::new(62), contract(7, 1)),
            Err(ReplyReserveError::IncarnationExhausted)
        ));
        assert!(ledger.is_empty());
        assert!(!ledger.is_closing());
        assert!(ledger.next_incarnation.is_none());
    }

    /// Confirms a Selected-session reset clears authority without closing the
    /// ledger, and a reused ID receives a new exact token incarnation.
    #[test]
    fn selected_session_reset_revokes_authority_but_allows_new_incarnation() {
        let mut ledger = ledger(2);
        let (_old_ticket, stale_token) = ledger
            .reserve_pending(ReplyCapabilityId::new(1), contract(7, 1))
            .expect("pending entry")
            .into_parts();
        let (_second_id, _second_ticket, _second_token) = available(&mut ledger, 2, 255);

        let summary = ledger.clear_for_selected_session_reset();
        assert_eq!(summary.pending_publication(), 1);
        assert_eq!(summary.available(), 1);
        assert_eq!(ledger.generation(), ConnectionGeneration::new(7));
        assert!(ledger.is_empty());
        assert!(!ledger.is_closing());
        assert!(ledger.has_capacity());

        let capability_id = ReplyCapabilityId::new(1);
        let (current_ticket, current_token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("selected reset keeps ledger reusable")
            .into_parts();
        assert_eq!(
            ledger.prepare_use(stale_token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::IncarnationChanged { capability_id })
        );
        assert_eq!(
            ledger.mark_available(&current_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        let current = ledger
            .prepare_use(current_token, ReplyUseKind::Normal)
            .expect("new incarnation is usable");
        let current =
            verify_authorized_plan(current, ReplyUseKind::Normal, capability_id, contract(7, 1));
        assert!(matches!(
            ledger.commit_use(current),
            Ok(ReplyUseTerminal::Consumed { .. })
        ));
    }
}
