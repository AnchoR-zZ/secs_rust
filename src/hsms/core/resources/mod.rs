//! Use-case-level transaction boundary across private Core resource owners.
//!
//! The assembled reducer will expose only atomic orchestration methods here;
//! callers will not receive independent mutable access to the contained ledgers.

pub(super) mod authority;
mod ids;
mod publication;

use self::authority::PeerRejectMutationAuthority;

#[allow(unused_imports)]
pub(super) use publication::{
    NormalSecondaryUnavailable, PreparedReplyUse, PublicationAdmissionError, PublicationCloseError,
    PublicationCloseSummary, PublicationDeliveryTerminal, PublicationDisposition,
    PublicationFinishError, PublicationInvariantViolation, PublicationReplyUseCommitFailure,
    PublicationReplyUsePrepareFailure, PublicationResetError, PublicationResetSummary,
    PublicationResourceKind, PublicationResources, PublicationResourcesBuildError,
    ReplyCapabilityMode, ReplyContract, ReplyContractError, ReplyUseCommitError, ReplyUseKind,
    ReplyUseTerminal, ReplyUseUnavailable,
};

use crate::hsms::{
    contracts::{PeerRejectDisposition, RejectReference},
    core::{
        operation::{
            CompletionTarget, OperationLedger, OperationLedgerBuildError, OperationRetention,
            RejectDiscoveryDecision, RejectTokenInvalidity, RejectValidationDecision,
            TerminalCorrelationRetention,
        },
        transaction::{
            OperationClass, PeerRejectFinishDecision, RegistryBuildError, RegistryOperationState,
            TombstoneCategory, TransactionRegistry,
        },
    },
    model::{
        ids::{ConnectionGeneration, OperationId},
        runtime::TimerToken,
    },
};

/// Failure while constructing the currently assembled Core resource owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoreResourcesBuildError {
    /// Operation ledger rejected one of its logical capacities.
    Operation(
        /// Exact Operation-ledger construction failure.
        OperationLedgerBuildError,
    ),
    /// Transaction registry rejected one of its logical capacities.
    Transaction(
        /// Exact Registry construction failure.
        RegistryBuildError,
    ),
}

/// Internal invariant failure detected before any unsafe peer-Reject attribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerRejectInvariantViolation {
    /// Correlation selected an Operation absent from semantic ownership.
    CorrelationOwnershipMismatch,
    /// A discovered live token became invalid before any resource mutation.
    OperationValidation(
        /// Exact Operation token validation failure.
        RejectTokenInvalidity,
    ),
    /// A discovered Operation became terminal before the atomic use case began.
    OperationAlreadyTerminal,
    /// A discovered Operation disappeared before the atomic use case began.
    OperationDisappeared,
    /// Registry no longer contains the independently attributed Operation.
    RegistryOperationMissing,
    /// Registry retained a terminal record instead of the attributed live owner.
    RegistryAlreadyTerminal,
    /// Registry says Core never authorized this outbound Operation to be visible.
    RegistryNotVisible,
    /// Operation and Registry disagree about the exact ownership class.
    RegistryClassMismatch,
    /// Operation declared no Registry binding but a live Registry owner exists.
    UnexpectedRegistryLiveOwner {
        /// Exact unexpected live Registry ownership class.
        class: OperationClass,
    },
    /// Operation declared no Registry binding but a terminal tombstone exists.
    UnexpectedRegistryTerminalOwner {
        /// Exact unexpected terminal Registry category.
        category: TombstoneCategory,
    },
}

/// Atomic CoreResources result for one received peer `Reject.req`.
#[must_use = "peer-Reject completion, timer cancellation, and diagnostics must be routed"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PeerRejectResourcesDecision {
    /// One globally unique live Operation was terminated across both ledgers.
    Applied {
        /// Exact semantic Operation terminated by the peer Reject.
        operation_id: OperationId,
        /// Original command completion authority or autonomous marker.
        target: CompletionTarget,
        /// Operation resource-shell retention after Registry release.
        retention: OperationRetention,
        /// Terminal Reject-correlation history action.
        correlation: TerminalCorrelationRetention,
        /// Exact T3 or T6 token removed from Registry, if armed.
        cancel_timer: Option<TimerToken>,
    },
    /// Reject was safely classified without mutating any live Operation.
    Notice {
        /// Public-safe attribution category for the eventual protocol notice.
        disposition: PeerRejectDisposition,
    },
    /// Reject belonged to an obsolete or future TCP generation.
    WrongGeneration {
        /// Generation owned by these resources.
        expected: ConnectionGeneration,
        /// Generation stamped on the received Reject.
        actual: ConnectionGeneration,
    },
    /// Independent resource owners disagreed; no Operation mutation occurred.
    InvariantViolation {
        /// Exact cross-resource inconsistency detected by preflight.
        violation: PeerRejectInvariantViolation,
    },
}

/// Private assembly of resource owners that must change as one Core use case.
pub(crate) struct CoreResources {
    /// Semantic Operation ownership and outbound Reject correlation.
    operations: OperationLedger,
    /// System Bytes, transaction, visibility, timer, and tombstone ownership.
    transactions: TransactionRegistry,
    /// Non-forgeable capability gating peer-Reject mutations in both ledgers.
    peer_reject_authority: PeerRejectMutationAuthority,
}

impl CoreResources {
    /// Builds the minimal Operation/Transaction assembly for one TCP generation.
    ///
    /// Each capacity is logical and lazily backed by its child owner. The
    /// returned resources expose peer Reject only as one atomic use case.
    pub(crate) fn new(
        generation: ConnectionGeneration,
        operation_capacity: usize,
        correlation_history_capacity: usize,
        request_capacity: usize,
        transaction_tombstone_capacity: usize,
    ) -> Result<Self, CoreResourcesBuildError> {
        let operations =
            OperationLedger::new(generation, operation_capacity, correlation_history_capacity)
                .map_err(CoreResourcesBuildError::Operation)?;
        let transactions =
            TransactionRegistry::new(generation, request_capacity, transaction_tombstone_capacity)
                .map_err(CoreResourcesBuildError::Transaction)?;
        Ok(Self {
            operations,
            transactions,
            peer_reject_authority: PeerRejectMutationAuthority::new(),
        })
    }

    /// Attributes and atomically commits one peer Reject across Operation and Registry.
    ///
    /// All Operation facts are revalidated before Registry mutation. Registry
    /// mutation is capability-gated and synchronous; its successful exact
    /// class becomes an unforgeable release proof consumed immediately by the
    /// Operation commit. No caller can obtain either intermediate capability.
    pub(crate) fn apply_peer_reject(
        &mut self,
        reference: RejectReference,
    ) -> PeerRejectResourcesDecision {
        let token = match self.operations.discover_peer_reject(reference) {
            RejectDiscoveryDecision::Live(token) => token,
            RejectDiscoveryDecision::Unknown => {
                return Self::notice(PeerRejectDisposition::Unknown);
            }
            RejectDiscoveryDecision::Ambiguous { .. } => {
                return Self::notice(PeerRejectDisposition::Ambiguous);
            }
            RejectDiscoveryDecision::Late => {
                return Self::notice(PeerRejectDisposition::Late);
            }
            RejectDiscoveryDecision::Duplicate => {
                return Self::notice(PeerRejectDisposition::Duplicate);
            }
            RejectDiscoveryDecision::Conflicting => {
                return Self::notice(PeerRejectDisposition::Conflicting);
            }
            RejectDiscoveryDecision::UnsupportedExtension => {
                return Self::notice(PeerRejectDisposition::UnsupportedExtension);
            }
            RejectDiscoveryDecision::WrongGeneration { expected, actual } => {
                return PeerRejectResourcesDecision::WrongGeneration { expected, actual };
            }
            RejectDiscoveryDecision::InvariantViolation => {
                return Self::invariant(PeerRejectInvariantViolation::CorrelationOwnershipMismatch);
            }
        };

        let validated = match self
            .operations
            .validate_peer_reject_commit(&self.peer_reject_authority, token)
        {
            RejectValidationDecision::Validated(validated) => validated,
            RejectValidationDecision::AlreadyTerminal { .. } => {
                return Self::invariant(PeerRejectInvariantViolation::OperationAlreadyTerminal);
            }
            RejectValidationDecision::StaleToken => {
                return Self::invariant(PeerRejectInvariantViolation::OperationDisappeared);
            }
            RejectValidationDecision::InvariantViolation { validation } => {
                return Self::invariant(PeerRejectInvariantViolation::OperationValidation(
                    validation,
                ));
            }
        };

        let operation_id = validated.operation_id();
        let (registry_release, cancel_timer) = match validated.expected_registry_binding() {
            None => match self.transactions.operation_state(operation_id) {
                RegistryOperationState::Absent => (None, None),
                RegistryOperationState::Live { class, .. } => {
                    return Self::invariant(
                        PeerRejectInvariantViolation::UnexpectedRegistryLiveOwner { class },
                    );
                }
                RegistryOperationState::Terminal { category } => {
                    return Self::invariant(
                        PeerRejectInvariantViolation::UnexpectedRegistryTerminalOwner { category },
                    );
                }
            },
            Some(expected_class) => match self.transactions.finish_peer_rejected(
                &mut self.peer_reject_authority,
                operation_id,
                expected_class,
            ) {
                PeerRejectFinishDecision::Finished {
                    release,
                    cancel_timer,
                    ..
                } => (Some(release), cancel_timer),
                PeerRejectFinishDecision::AlreadyTerminal { .. } => {
                    return Self::invariant(PeerRejectInvariantViolation::RegistryAlreadyTerminal);
                }
                PeerRejectFinishDecision::NotVisible { .. } => {
                    return Self::invariant(PeerRejectInvariantViolation::RegistryNotVisible);
                }
                PeerRejectFinishDecision::ClassMismatch { .. } => {
                    return Self::invariant(PeerRejectInvariantViolation::RegistryClassMismatch);
                }
                PeerRejectFinishDecision::UnknownOperation => {
                    return Self::invariant(PeerRejectInvariantViolation::RegistryOperationMissing);
                }
            },
        };

        let committed = self.operations.commit_peer_reject(
            &mut self.peer_reject_authority,
            validated,
            registry_release,
        );
        let (target, retention, correlation) = committed.into_parts();
        PeerRejectResourcesDecision::Applied {
            operation_id,
            target,
            retention,
            correlation,
            cancel_timer,
        }
    }

    /// Constructs one non-mutating public-safe peer-Reject classification.
    const fn notice(disposition: PeerRejectDisposition) -> PeerRejectResourcesDecision {
        PeerRejectResourcesDecision::Notice { disposition }
    }

    /// Constructs one fail-closed cross-resource invariant result.
    const fn invariant(violation: PeerRejectInvariantViolation) -> PeerRejectResourcesDecision {
        PeerRejectResourcesDecision::InvariantViolation { violation }
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::core::operation::OperationClaimCause;

    use crate::hsms::{
        contracts::{
            CommandCompletionAuthority, OperationOwner, OutboundHeaderIdentity,
            PeerRejectDisposition, RejectReference,
        },
        core::{
            operation::{
                ActiveOperationWrite, CompletionTarget, OperationPurpose, OperationScope,
                OperationSpec, OperationTerminalCause, OperationVisibilityDecision,
                TerminalClaimDecision,
            },
            resources::{CoreResources, PeerRejectInvariantViolation, PeerRejectResourcesDecision},
            transaction::{MarkVisibleDecision, OperationClass, RegistryOperationState},
        },
        error::OperationError,
        model::ids::{
            CommandId, ConnectionGeneration, Function, OperationId, SessionId, Stream, SystemBytes,
            WriteId,
        },
        protocol::{
            header::{DataHeader, RejectReason},
            message::{DataMessage, ProtocolMessage},
        },
    };

    /// Deterministic generation shared by the atomic peer-Reject tests.
    const GENERATION: ConnectionGeneration = ConnectionGeneration::new(7);

    /// Creates a minimal resource assembly with non-zero lazy capacities.
    fn resources() -> CoreResources {
        CoreResources::new(GENERATION, 4, 4, 4, 4).expect("all resource capacities are non-zero")
    }

    /// Builds the exact W=1 Data identity for one reserved transaction.
    fn request_identity(system_bytes: SystemBytes) -> OutboundHeaderIdentity {
        let message = ProtocolMessage::Data(DataMessage::new(
            DataHeader::new(
                SessionId::new(3).expect("ordinary Data Session ID"),
                Stream::new(1).expect("seven-bit stream"),
                Function::new(1),
                true,
                system_bytes,
            ),
            None,
        ));
        OutboundHeaderIdentity::from_protocol_message(&message)
            .expect("valid outbound message shape")
    }

    /// Builds the exact W=false Data Secondary identity for one reply Operation.
    fn reply_identity(system_bytes: SystemBytes) -> OutboundHeaderIdentity {
        let message = ProtocolMessage::Data(DataMessage::new(
            DataHeader::new(
                SessionId::new(3).expect("ordinary Data Session ID"),
                Stream::new(1).expect("seven-bit stream"),
                Function::new(2),
                false,
                system_bytes,
            ),
            None,
        ));
        OutboundHeaderIdentity::from_protocol_message(&message)
            .expect("valid outbound message shape")
    }

    /// Registers matching Operation and Registry owners and marks both visible.
    fn register_visible_request(resources: &mut CoreResources) -> SystemBytes {
        let operation_id = OperationId::new(1);
        let reserved = resources
            .transactions
            .reserve_request(
                operation_id,
                SessionId::new(3).expect("ordinary Data Session ID"),
                Stream::new(1).expect("seven-bit stream"),
                Function::new(1),
            )
            .expect("request reservation");
        let spec = OperationSpec::new(
            operation_id,
            OperationOwner::Command(CommandCompletionAuthority::for_test(CommandId::new(10))),
            OperationPurpose::Request,
            OperationScope::SelectedSessionData,
            Some(OperationClass::Request),
            Some(ActiveOperationWrite::new(
                WriteId::new(100),
                request_identity(reserved.system_bytes()),
            )),
        );
        let token = resources
            .operations
            .prepare_registration(&spec)
            .expect("operation preflight");
        resources
            .operations
            .commit_registration(token, spec)
            .expect("operation commit");
        assert!(matches!(
            resources.transactions.mark_visible(operation_id),
            MarkVisibleDecision::Marked {
                class: OperationClass::Request
            }
        ));
        assert_eq!(
            resources
                .operations
                .mark_may_be_visible(operation_id, WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );
        reserved.system_bytes()
    }

    /// Builds the exact unsupported-PType Reject for the registered request.
    fn request_reject(system_bytes: SystemBytes) -> RejectReference {
        RejectReference::new(
            GENERATION,
            3,
            0,
            RejectReason::UNSUPPORTED_PTYPE,
            system_bytes,
        )
    }

    /// Confirms the single CoreResources use case removes Registry ownership,
    /// terminalizes Operation ownership, and releases one completion authority.
    #[test]
    fn peer_reject_commits_both_ledgers_and_one_completion_atomically() {
        let mut resources = resources();
        let system_bytes = register_visible_request(&mut resources);
        let reference = request_reject(system_bytes);

        let decision = resources.apply_peer_reject(reference);
        let (target, retention) = match decision {
            PeerRejectResourcesDecision::Applied {
                operation_id,
                target,
                retention,
                cancel_timer,
                ..
            } => {
                assert_eq!(operation_id, OperationId::new(1));
                assert_eq!(cancel_timer, None);
                (target, retention)
            }
            other => panic!("expected applied peer Reject, got {other:?}"),
        };
        assert!(matches!(
            retention,
            crate::hsms::core::operation::OperationRetention::Retained {
                terminal: true,
                registry_binding: None,
                active_write: Some(write_id),
            } if write_id == WriteId::new(100)
        ));
        match target {
            CompletionTarget::Command(authority) => {
                let completion = authority.failed(OperationError::PeerRejected {
                    reason: RejectReason::UNSUPPORTED_PTYPE,
                });
                assert_eq!(completion.command_id(), CommandId::new(10));
            }
            CompletionTarget::Autonomous => panic!("request must retain command ownership"),
        }
        let snapshot = resources
            .operations
            .operation_snapshot(OperationId::new(1))
            .expect("active write keeps one terminal resource shell");
        assert_eq!(snapshot.registry_binding(), None);
        assert!(matches!(
            snapshot.terminal_cause(),
            Some(OperationTerminalCause::PeerRejected(terminal))
                if terminal.reference() == reference
        ));
        assert!(matches!(
            resources.apply_peer_reject(reference),
            PeerRejectResourcesDecision::Notice {
                disposition: PeerRejectDisposition::Duplicate
            }
        ));
    }

    /// Confirms an Operation/Registry ownership mismatch fails before any
    /// semantic mutation or move-only completion release can occur.
    #[test]
    fn peer_reject_registry_mismatch_is_fail_closed_and_non_mutating() {
        let mut resources = resources();
        let operation_id = OperationId::new(1);
        let system_bytes = SystemBytes::new(1);
        let spec = OperationSpec::new(
            operation_id,
            OperationOwner::Command(CommandCompletionAuthority::for_test(CommandId::new(10))),
            OperationPurpose::Request,
            OperationScope::SelectedSessionData,
            Some(OperationClass::Request),
            Some(ActiveOperationWrite::new(
                WriteId::new(100),
                request_identity(system_bytes),
            )),
        );
        let token = resources
            .operations
            .prepare_registration(&spec)
            .expect("operation preflight");
        resources
            .operations
            .commit_registration(token, spec)
            .expect("operation commit");
        assert_eq!(
            resources
                .operations
                .mark_may_be_visible(operation_id, WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );

        assert!(matches!(
            resources.apply_peer_reject(request_reject(system_bytes)),
            PeerRejectResourcesDecision::InvariantViolation {
                violation: PeerRejectInvariantViolation::RegistryOperationMissing
            }
        ));
        let snapshot = resources
            .operations
            .operation_snapshot(operation_id)
            .expect("failed atomic use case must retain the operation");
        assert_eq!(snapshot.registry_binding(), Some(OperationClass::Request));
        assert_eq!(snapshot.terminal_cause(), None);
        match resources
            .operations
            .claim_terminal(operation_id, OperationClaimCause::InternalInvariantFailure)
        {
            TerminalClaimDecision::Claimed {
                target: CompletionTarget::Command(authority),
                ..
            } => {
                assert_eq!(authority.command_id(), CommandId::new(10));
                drop(authority.failed(OperationError::RuntimeStopped));
            }
            other => {
                panic!("peer Reject mismatch must not consume completion authority: {other:?}")
            }
        }
    }

    /// Confirms a nominally no-Registry reply cannot hide a colliding live
    /// Registry owner; both ledgers remain unchanged when preflight finds it.
    #[test]
    fn peer_reject_detects_unexpected_registry_owner_before_mutation() {
        let mut resources = resources();
        let operation_id = OperationId::new(1);
        let reserved = resources
            .transactions
            .reserve_request(
                operation_id,
                SessionId::new(3).expect("ordinary Data Session ID"),
                Stream::new(1).expect("seven-bit stream"),
                Function::new(1),
            )
            .expect("deliberately colliding Registry request");
        assert!(matches!(
            resources.transactions.mark_visible(operation_id),
            MarkVisibleDecision::Marked {
                class: OperationClass::Request
            }
        ));
        let spec = OperationSpec::new(
            operation_id,
            OperationOwner::Command(CommandCompletionAuthority::for_test(CommandId::new(10))),
            OperationPurpose::Reply,
            OperationScope::SelectedSessionData,
            None,
            Some(ActiveOperationWrite::new(
                WriteId::new(100),
                reply_identity(reserved.system_bytes()),
            )),
        );
        let token = resources
            .operations
            .prepare_registration(&spec)
            .expect("reply Operation preflight");
        resources
            .operations
            .commit_registration(token, spec)
            .expect("reply Operation commit");
        assert_eq!(
            resources
                .operations
                .mark_may_be_visible(operation_id, WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );
        let reference = RejectReference::new(
            GENERATION,
            3,
            0,
            RejectReason::TRANSACTION_NOT_OPEN,
            reserved.system_bytes(),
        );

        assert!(matches!(
            resources.apply_peer_reject(reference),
            PeerRejectResourcesDecision::InvariantViolation {
                violation: PeerRejectInvariantViolation::UnexpectedRegistryLiveOwner {
                    class: OperationClass::Request
                }
            }
        ));
        let snapshot = resources
            .operations
            .operation_snapshot(operation_id)
            .expect("reply Operation must remain live");
        assert_eq!(snapshot.registry_binding(), None);
        assert_eq!(snapshot.terminal_cause(), None);
        assert!(matches!(
            resources.transactions.operation_state(operation_id),
            RegistryOperationState::Live {
                class: OperationClass::Request,
                ..
            }
        ));
    }
}
