//! Maintains bounded outbound-header correlation for peer `Reject.req`.
//!
//! The index is a private child of `OperationLedger`: it owns no command
//! completion authority and never mutates transaction or write state. Live
//! entries are scanned together with a bounded terminal FIFO so Reject
//! attribution is globally unique across both sets.

use std::collections::{HashMap, VecDeque};

use crate::hsms::{
    contracts::{
        OutboundCorrelationState, OutboundHeaderIdentity, RejectCorrelationEligibility,
        RejectReference,
    },
    model::ids::{ConnectionGeneration, OperationId},
};

/// Correlation-index construction failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CorrelationBuildError {
    /// Terminal diagnostic history must retain at least one entry.
    ZeroTerminalHistoryCapacity,
}

/// Failure to register a live outbound identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CorrelationRegisterError {
    /// The operation already owns a live outbound identity.
    DuplicateOperation {
        /// Operation that already exists in the live correlation map.
        operation_id: OperationId,
    },
}

/// Result of advancing one live correlation to possible peer visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CorrelationVisibilityDecision {
    /// Visibility advanced from `BeforeProceed` to `MayBeVisible`.
    Marked,
    /// The exact live identity was already possibly peer-visible.
    AlreadyVisible,
    /// No live correlation exists for the supplied operation.
    UnknownOperation,
}

/// Reason recorded when a possibly visible identity becomes terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CorrelationTerminalCause {
    /// The operation ended for a reason other than peer Reject.
    Other,
    /// The operation was uniquely terminated by this exact peer Reject.
    PeerRejected(
        /// Full Reject reference retained for duplicate/conflict diagnostics.
        RejectReference,
    ),
}

/// Result of removing a live identity at its semantic terminal point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CorrelationTerminalDecision {
    /// The operation had no outbound identity, as for local abandonment.
    NoCorrelation,
    /// A definitely invisible identity was discarded without terminal history.
    DiscardedBeforeProceed,
    /// A possibly visible identity entered bounded terminal history.
    Retained {
        /// Oldest operation evicted from diagnostic history, if the FIFO was full.
        evicted_operation_id: Option<OperationId>,
    },
}

/// Move-only proof that discovery found exactly one live Reject candidate.
#[derive(Debug)]
pub(super) struct CorrelationRejectToken {
    /// Exact live operation discovered by the global scan.
    operation_id: OperationId,
    /// Immutable outbound identity observed during discovery.
    identity: OutboundHeaderIdentity,
    /// Exact peer Reject reference used by discovery.
    reference: RejectReference,
}

impl CorrelationRejectToken {
    /// Returns the uniquely discovered live operation.
    pub(super) const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Returns the immutable outbound identity observed during discovery.
    pub(super) const fn identity(&self) -> OutboundHeaderIdentity {
        self.identity
    }

    /// Returns the exact peer Reject reference used during discovery.
    pub(super) const fn reference(&self) -> RejectReference {
        self.reference
    }
}

/// Result of globally scanning live and terminal outbound identities.
#[derive(Debug)]
pub(super) enum CorrelationRejectDiscovery {
    /// Exactly one possibly visible live operation matched.
    Live(
        /// Move-only token required to commit the attribution.
        CorrelationRejectToken,
    ),
    /// No retained outbound identity matched.
    Unknown,
    /// More than one live/history identity matched, so mutation is unsafe.
    Ambiguous {
        /// Number of matching possibly visible live operations.
        live_matches: usize,
        /// Number of matching terminal-history records.
        terminal_matches: usize,
    },
    /// One terminal identity matched an operation that ended another way.
    Late,
    /// One terminal identity retained this exact prior Reject reference.
    Duplicate,
    /// One terminal identity retained a different prior Reject reference.
    Conflicting,
    /// The extension reason has no configured Header Byte 2 semantics.
    UnsupportedExtension,
    /// The Reject belongs to another TCP connection generation.
    WrongGeneration {
        /// Generation owned by this correlation index.
        expected: ConnectionGeneration,
        /// Generation stamped onto the rejected inbound event.
        actual: ConnectionGeneration,
    },
}

/// Result of revalidating a move-only Reject discovery token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CorrelationTokenValidation {
    /// The token still names the exact unique live candidate.
    Valid,
    /// The operation no longer has a live correlation identity.
    UnknownOperation,
    /// The operation still exists, but its immutable identity disagrees.
    IdentityChanged,
    /// The operation has not reached, or no longer retains, live eligibility.
    NotLiveEligible,
    /// The token's Reject reference belongs to another generation.
    WrongGeneration,
    /// The reference no longer semantically matches the live identity.
    ReferenceMismatch,
    /// Another live or terminal candidate appeared after discovery.
    NoLongerUnique,
}

/// One outbound identity that may later become peer-visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LiveCorrelation {
    /// Header identity derived from the typed outbound protocol message.
    identity: OutboundHeaderIdentity,
    /// Conservative visibility authority used during Reject discovery.
    state: OutboundCorrelationState,
}

/// Minimal terminal memory needed to distinguish Reject diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalRejectMemory {
    /// The operation ended for any reason other than peer Reject.
    OtherTerminal,
    /// The operation ended because of this exact Reject reference.
    PeerRejected(
        /// Exact prior Reject retained for duplicate/conflict comparison.
        RejectReference,
    ),
}

/// One possibly visible identity retained after its operation became terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalCorrelation {
    /// Operation that originally emitted this outbound identity.
    operation_id: OperationId,
    /// Immutable header identity retained for later global scans.
    identity: OutboundHeaderIdentity,
    /// Minimal terminal reason used for diagnostic classification.
    memory: TerminalRejectMemory,
}

/// Bounded live and terminal outbound-header correlation store.
pub(super) struct OutboundCorrelationIndex {
    /// TCP generation whose outbound messages populate this index.
    generation: ConnectionGeneration,
    /// Maximum number of independent terminal diagnostic records.
    terminal_capacity: usize,
    /// Live identities keyed only by their unique operation owner.
    live: HashMap<OperationId, LiveCorrelation>,
    /// Oldest-first bounded terminal diagnostic history.
    terminal: VecDeque<TerminalCorrelation>,
}

impl OutboundCorrelationIndex {
    /// Creates a lazy-allocation correlation index for one generation.
    ///
    /// `terminal_capacity` is a logical bound. The constructor deliberately
    /// performs no capacity-sized allocation, so even extreme validated
    /// values cannot cause a construction-time allocation failure.
    pub(super) fn new(
        generation: ConnectionGeneration,
        terminal_capacity: usize,
    ) -> Result<Self, CorrelationBuildError> {
        if terminal_capacity == 0 {
            return Err(CorrelationBuildError::ZeroTerminalHistoryCapacity);
        }
        Ok(Self {
            generation,
            terminal_capacity,
            live: HashMap::new(),
            terminal: VecDeque::new(),
        })
    }

    /// Returns the generation owned by this index.
    pub(super) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the number of live outbound identities.
    pub(super) fn live_len(&self) -> usize {
        self.live.len()
    }

    /// Returns the number of retained terminal diagnostic records.
    pub(super) fn terminal_len(&self) -> usize {
        self.terminal.len()
    }

    /// Returns whether both live and terminal correlation stores are empty.
    pub(super) fn is_empty(&self) -> bool {
        self.live.is_empty() && self.terminal.is_empty()
    }

    /// Registers one immutable outbound identity as definitely invisible.
    ///
    /// Failure leaves every live and terminal correlation record unchanged.
    pub(super) fn register(
        &mut self,
        operation_id: OperationId,
        identity: OutboundHeaderIdentity,
    ) -> Result<(), CorrelationRegisterError> {
        if self.live.contains_key(&operation_id) {
            return Err(CorrelationRegisterError::DuplicateOperation { operation_id });
        }
        self.live.insert(
            operation_id,
            LiveCorrelation {
                identity,
                state: OutboundCorrelationState::BeforeProceed,
            },
        );
        Ok(())
    }

    /// Advances an exact live operation to conservative peer visibility.
    pub(super) fn mark_may_be_visible(
        &mut self,
        operation_id: OperationId,
    ) -> CorrelationVisibilityDecision {
        let Some(correlation) = self.live.get_mut(&operation_id) else {
            return CorrelationVisibilityDecision::UnknownOperation;
        };
        match correlation.state {
            OutboundCorrelationState::BeforeProceed => {
                correlation.state = OutboundCorrelationState::MayBeVisible;
                CorrelationVisibilityDecision::Marked
            }
            OutboundCorrelationState::MayBeVisible => CorrelationVisibilityDecision::AlreadyVisible,
            OutboundCorrelationState::TerminalHistory => {
                unreachable!("terminal history is never retained in the live map")
            }
        }
    }

    /// Removes a live identity and applies its terminal-history policy.
    ///
    /// `BeforeProceed` identities are discarded because the peer could not
    /// have observed them. `MayBeVisible` identities move to the independent
    /// bounded FIFO and may evict only the oldest diagnostic record.
    pub(super) fn terminalize(
        &mut self,
        operation_id: OperationId,
        cause: CorrelationTerminalCause,
    ) -> CorrelationTerminalDecision {
        let Some(correlation) = self.live.remove(&operation_id) else {
            return CorrelationTerminalDecision::NoCorrelation;
        };
        if correlation.state == OutboundCorrelationState::BeforeProceed {
            return CorrelationTerminalDecision::DiscardedBeforeProceed;
        }

        let evicted_operation_id = if self.terminal.len() == self.terminal_capacity {
            self.terminal.pop_front().map(|record| record.operation_id)
        } else {
            None
        };
        let memory = match cause {
            CorrelationTerminalCause::Other => TerminalRejectMemory::OtherTerminal,
            CorrelationTerminalCause::PeerRejected(reference) => {
                TerminalRejectMemory::PeerRejected(reference)
            }
        };
        self.terminal.push_back(TerminalCorrelation {
            operation_id,
            identity: correlation.identity,
            memory,
        });
        CorrelationTerminalDecision::Retained {
            evicted_operation_id,
        }
    }

    /// Discovers one globally unique Reject attribution without mutation.
    ///
    /// Live and terminal candidates share one uniqueness domain. A live match
    /// plus a terminal match is therefore ambiguous rather than preferring the
    /// newer live operation.
    pub(super) fn discover_peer_reject(
        &self,
        reference: RejectReference,
    ) -> CorrelationRejectDiscovery {
        if reference.generation() != self.generation {
            return CorrelationRejectDiscovery::WrongGeneration {
                expected: self.generation,
                actual: reference.generation(),
            };
        }
        if !reference.reason().is_base_standard() {
            return CorrelationRejectDiscovery::UnsupportedExtension;
        }

        let mut live_match = None;
        let mut live_matches = 0usize;
        for (operation_id, correlation) in &self.live {
            if reference.candidate_eligibility(correlation.identity, correlation.state)
                == RejectCorrelationEligibility::Live
            {
                live_matches = live_matches.saturating_add(1);
                if live_match.is_none() {
                    live_match = Some((*operation_id, correlation.identity));
                }
            }
        }

        let mut terminal_match = None;
        let mut terminal_matches = 0usize;
        for correlation in &self.terminal {
            if reference.candidate_eligibility(
                correlation.identity,
                OutboundCorrelationState::TerminalHistory,
            ) == RejectCorrelationEligibility::TerminalDiagnostic
            {
                terminal_matches = terminal_matches.saturating_add(1);
                if terminal_match.is_none() {
                    terminal_match = Some(*correlation);
                }
            }
        }

        let total_matches = live_matches.saturating_add(terminal_matches);
        if total_matches == 0 {
            return CorrelationRejectDiscovery::Unknown;
        }
        if total_matches > 1 {
            return CorrelationRejectDiscovery::Ambiguous {
                live_matches,
                terminal_matches,
            };
        }
        if let Some((operation_id, identity)) = live_match {
            return CorrelationRejectDiscovery::Live(CorrelationRejectToken {
                operation_id,
                identity,
                reference,
            });
        }

        match terminal_match
            .expect("one total terminal match must retain its correlation record")
            .memory
        {
            TerminalRejectMemory::OtherTerminal => CorrelationRejectDiscovery::Late,
            TerminalRejectMemory::PeerRejected(previous) if previous == reference => {
                CorrelationRejectDiscovery::Duplicate
            }
            TerminalRejectMemory::PeerRejected(_) => CorrelationRejectDiscovery::Conflicting,
        }
    }

    /// Revalidates a discovery token immediately before semantic commit.
    ///
    /// This method does not mutate the index. `OperationLedger` performs the
    /// terminal transition only after this validation succeeds.
    pub(super) fn validate_reject_token(
        &self,
        token: &CorrelationRejectToken,
    ) -> CorrelationTokenValidation {
        if token.reference.generation() != self.generation {
            return CorrelationTokenValidation::WrongGeneration;
        }
        let Some(correlation) = self.live.get(&token.operation_id) else {
            return CorrelationTokenValidation::UnknownOperation;
        };
        if correlation.identity != token.identity {
            return CorrelationTokenValidation::IdentityChanged;
        }
        if correlation.state != OutboundCorrelationState::MayBeVisible {
            return CorrelationTokenValidation::NotLiveEligible;
        }
        if token
            .reference
            .candidate_eligibility(correlation.identity, correlation.state)
            != RejectCorrelationEligibility::Live
        {
            return CorrelationTokenValidation::ReferenceMismatch;
        }
        match self.discover_peer_reject(token.reference) {
            CorrelationRejectDiscovery::Live(current)
                if current.operation_id == token.operation_id
                    && current.identity == token.identity =>
            {
                CorrelationTokenValidation::Valid
            }
            CorrelationRejectDiscovery::Live(_) => CorrelationTokenValidation::IdentityChanged,
            CorrelationRejectDiscovery::Ambiguous { .. } => {
                CorrelationTokenValidation::NoLongerUnique
            }
            CorrelationRejectDiscovery::WrongGeneration { .. } => {
                CorrelationTokenValidation::WrongGeneration
            }
            CorrelationRejectDiscovery::Unknown
            | CorrelationRejectDiscovery::Late
            | CorrelationRejectDiscovery::Duplicate
            | CorrelationRejectDiscovery::Conflicting
            | CorrelationRejectDiscovery::UnsupportedExtension => {
                CorrelationTokenValidation::ReferenceMismatch
            }
        }
    }
}
