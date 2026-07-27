//! Owns bounded semantic operations and their exactly-once terminal claims.
//!
//! Semantic completion is independent from resource cleanup: a command may
//! complete while its exact write or transaction binding remains as a bounded
//! resource shell. The ledger owns neither transaction internals nor write
//! phases and communicates with future `CoreResources` only through typed,
//! move-only decisions.

use std::collections::HashMap;

use crate::hsms::{
    contracts::{
        CommandCompletionAuthority, OperationOwner, OutboundHeaderIdentity, OutboundOperationKind,
        RejectReference,
    },
    core::{
        operation::correlation::{
            CorrelationBuildError, CorrelationRegisterError, CorrelationRejectDiscovery,
            CorrelationRejectToken, CorrelationTerminalCause, CorrelationTerminalDecision,
            CorrelationTokenValidation, CorrelationVisibilityDecision, OutboundCorrelationIndex,
        },
        resources::authority::PeerRejectMutationAuthority,
        transaction::PeerRejectRegistryRelease,
        transaction::{ControlKind, OneWayKind, OperationClass},
    },
    model::ids::{CommandId, ConnectionGeneration, OperationId, WriteId},
    TimeoutKind,
};

/// Construction failure for one bounded operation ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationLedgerBuildError {
    /// The logical live/resource-shell capacity must be non-zero.
    ZeroOperationCapacity,
    /// The independent terminal correlation history must be non-zero.
    ZeroCorrelationHistoryCapacity,
}

/// Stable semantic purpose of one operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationPurpose {
    /// Application W=0 Data Primary completed by local write commitment.
    Send,
    /// Application W=1 Data Primary completed by response or failure.
    Request,
    /// Application F+1 Secondary completed by local write commitment.
    Reply,
    /// Application SxF0 response completed by local write commitment.
    AbortReply,
    /// Local reply-capability release that performs no outbound write.
    AbandonReply,
    /// Locally initiated transactional control procedure.
    Control(
        /// Select, Deselect, or Linktest transaction class.
        ControlKind,
    ),
    /// One-way `Separate.req` operation.
    Separate,
    /// Mandatory autonomous response to a peer control request.
    PeerControlResponse(
        /// Select, Deselect, or Linktest response form being emitted.
        ControlKind,
    ),
    /// Autonomous locally generated `Reject.req`.
    ProtocolReject,
}

impl OperationPurpose {
    /// Returns the only valid Registry binding for this operation purpose.
    const fn expected_registry_binding(self) -> Option<OperationClass> {
        match self {
            Self::Send => Some(OperationClass::OneWay(OneWayKind::Data)),
            Self::Request => Some(OperationClass::Request),
            Self::Control(kind) => Some(OperationClass::Control(kind)),
            Self::Separate => Some(OperationClass::OneWay(OneWayKind::Separate)),
            Self::Reply
            | Self::AbortReply
            | Self::AbandonReply
            | Self::PeerControlResponse(_)
            | Self::ProtocolReject => None,
        }
    }

    /// Returns the session-lifecycle scope required by this purpose.
    const fn expected_scope(self) -> OperationScope {
        match self {
            Self::Send | Self::Request | Self::Reply | Self::AbortReply | Self::AbandonReply => {
                OperationScope::SelectedSessionData
            }
            Self::Control(_)
            | Self::Separate
            | Self::PeerControlResponse(_)
            | Self::ProtocolReject => OperationScope::GenerationControl,
        }
    }

    /// Returns the exact typed outbound message kind required by this purpose.
    const fn expected_outbound_kind(self) -> Option<OutboundOperationKind> {
        match self {
            Self::Send => Some(OutboundOperationKind::DataPrimaryW0),
            Self::Request => Some(OutboundOperationKind::DataPrimaryW1),
            Self::Reply => Some(OutboundOperationKind::DataSecondaryW0),
            Self::AbortReply => Some(OutboundOperationKind::DataAbortW0),
            Self::Control(ControlKind::Select) => Some(OutboundOperationKind::SelectRequest),
            Self::Control(ControlKind::Deselect) => Some(OutboundOperationKind::DeselectRequest),
            Self::Control(ControlKind::Linktest) => Some(OutboundOperationKind::LinktestRequest),
            Self::Separate => Some(OutboundOperationKind::SeparateRequest),
            Self::PeerControlResponse(ControlKind::Select) => {
                Some(OutboundOperationKind::SelectResponse)
            }
            Self::PeerControlResponse(ControlKind::Deselect) => {
                Some(OutboundOperationKind::DeselectResponse)
            }
            Self::PeerControlResponse(ControlKind::Linktest) => {
                Some(OutboundOperationKind::LinktestResponse)
            }
            Self::ProtocolReject => Some(OutboundOperationKind::RejectRequest),
            Self::AbandonReply => None,
        }
    }

    /// Returns whether this purpose may be owned by `owner`.
    const fn supports_owner(self, owner: &OperationOwner) -> bool {
        match owner {
            OperationOwner::Command(_) => {
                !matches!(self, Self::PeerControlResponse(_) | Self::ProtocolReject)
            }
            OperationOwner::Autonomous => matches!(
                self,
                Self::Control(_)
                    | Self::Separate
                    | Self::PeerControlResponse(_)
                    | Self::ProtocolReject
            ),
        }
    }
}

/// Lifecycle domain affected by a re-selectable session reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationScope {
    /// Data work that must end when the selected session is reset.
    SelectedSessionData,
    /// Control work that survives a Data-session reset until generation close.
    GenerationControl,
}

/// Exact outbound write and identity initially attached to one operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActiveOperationWrite {
    /// Core-assigned frame write identity.
    write_id: WriteId,
    /// Header identity derived from the typed outbound protocol message.
    identity: OutboundHeaderIdentity,
}

impl ActiveOperationWrite {
    /// Creates an exact operation-to-write binding.
    pub(crate) const fn new(write_id: WriteId, identity: OutboundHeaderIdentity) -> Self {
        Self { write_id, identity }
    }

    /// Returns the exact active write identity.
    pub(crate) const fn write_id(self) -> WriteId {
        self.write_id
    }

    /// Returns the immutable outbound header identity.
    pub(crate) const fn identity(self) -> OutboundHeaderIdentity {
        self.identity
    }
}

/// Complete immutable description committed during operation admission.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "an operation specification must be registered or explicitly discarded"]
pub(crate) struct OperationSpec {
    /// Generation-local semantic operation identity.
    operation_id: OperationId,
    /// Command or autonomous owner of the unique terminal result.
    owner: OperationOwner,
    /// Stable semantic purpose used to reject incoherent resource bindings.
    purpose: OperationPurpose,
    /// Lifecycle domain affected by re-selectable session reset.
    scope: OperationScope,
    /// Registry ownership class already reserved for this operation, if any.
    registry_binding: Option<OperationClass>,
    /// Exact outbound write and identity, or `None` for local abandonment.
    active_write: Option<ActiveOperationWrite>,
}

impl OperationSpec {
    /// Creates a complete operation admission specification.
    pub(crate) const fn new(
        operation_id: OperationId,
        owner: OperationOwner,
        purpose: OperationPurpose,
        scope: OperationScope,
        registry_binding: Option<OperationClass>,
        active_write: Option<ActiveOperationWrite>,
    ) -> Self {
        Self {
            operation_id,
            owner,
            purpose,
            scope,
            registry_binding,
            active_write,
        }
    }

    /// Returns the generation-local operation identity.
    pub(crate) const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Borrows the command or autonomous owner without duplicating authority.
    pub(crate) const fn owner(&self) -> &OperationOwner {
        &self.owner
    }

    /// Returns the semantic operation purpose.
    pub(crate) const fn purpose(&self) -> OperationPurpose {
        self.purpose
    }

    /// Returns the operation's lifecycle scope.
    pub(crate) const fn scope(&self) -> OperationScope {
        self.scope
    }

    /// Returns the expected Registry ownership class.
    pub(crate) const fn registry_binding(&self) -> Option<OperationClass> {
        self.registry_binding
    }

    /// Returns the exact initial write and outbound identity.
    pub(crate) const fn active_write(&self) -> Option<ActiveOperationWrite> {
        self.active_write
    }

    /// Consumes a rejected specification and returns its operation owner.
    ///
    /// The returned owner retains any linear command-completion authority that
    /// must be completed after registration fails.
    pub(crate) fn into_owner(self) -> OperationOwner {
        self.owner
    }

    /// Validates owner, scope, Registry class, write presence, and exact outbound kind.
    fn validate(&self) -> Result<(), OperationSpecError> {
        if !self.purpose.supports_owner(&self.owner) {
            return Err(OperationSpecError::OwnerPurposeMismatch);
        }
        let expected_scope = self.purpose.expected_scope();
        if self.scope != expected_scope {
            return Err(OperationSpecError::ScopeMismatch {
                expected: expected_scope,
                actual: self.scope,
            });
        }
        let expected_binding = self.purpose.expected_registry_binding();
        if self.registry_binding != expected_binding {
            return Err(OperationSpecError::RegistryBindingMismatch {
                expected: expected_binding,
                actual: self.registry_binding,
            });
        }
        let expected_kind = self.purpose.expected_outbound_kind();
        match (expected_kind, self.active_write) {
            (None, None) => Ok(()),
            (None, Some(_)) => Err(OperationSpecError::UnexpectedWrite),
            (Some(_), None) => Err(OperationSpecError::MissingWrite),
            (Some(expected), Some(write)) if write.identity().kind() != expected => {
                Err(OperationSpecError::OutboundKindMismatch {
                    expected,
                    actual: write.identity().kind(),
                })
            }
            (Some(_), Some(_)) => Ok(()),
        }
    }
}

/// Incoherent fields supplied in an operation specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationSpecError {
    /// The semantic purpose cannot be owned by the supplied owner class.
    OwnerPurposeMismatch,
    /// The purpose requires another lifecycle scope.
    ScopeMismatch {
        /// Scope derived from the semantic purpose.
        expected: OperationScope,
        /// Scope supplied by the caller.
        actual: OperationScope,
    },
    /// The purpose requires a different Registry ownership class.
    RegistryBindingMismatch {
        /// Registry binding derived from the semantic purpose.
        expected: Option<OperationClass>,
        /// Registry binding supplied by the caller.
        actual: Option<OperationClass>,
    },
    /// A local no-write purpose unexpectedly carried an active write.
    UnexpectedWrite,
    /// A frame-producing purpose omitted its exact write binding.
    MissingWrite,
    /// The typed outbound message kind disagrees with its semantic purpose.
    OutboundKindMismatch {
        /// Exact kind derived from the semantic purpose.
        expected: OutboundOperationKind,
        /// Exact kind derived from the typed protocol message.
        actual: OutboundOperationKind,
    },
}

/// Failure returned by registration preflight or commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationRegisterError {
    /// Permanent generation close has fenced new operations.
    Closing,
    /// The bounded operation ledger has no free logical slot.
    CapacityExhausted {
        /// Configured maximum number of live entries and terminal shells.
        capacity: usize,
    },
    /// The generation-local operation identity is already registered.
    DuplicateOperation {
        /// Existing operation identity that prevented admission.
        operation_id: OperationId,
    },
    /// Another live operation or terminal resource shell owns the command ID.
    DuplicateCommand {
        /// Command identity that cannot be admitted twice.
        command_id: CommandId,
        /// Existing operation that currently owns the command identity.
        existing_operation_id: OperationId,
    },
    /// Move-only preflight token names another operation specification.
    TokenOperationMismatch {
        /// Operation identity admitted by preflight.
        expected: OperationId,
        /// Operation identity carried by the rejected specification.
        actual: OperationId,
    },
    /// Move-only preflight token names another command owner.
    TokenCommandMismatch {
        /// Command identity admitted by preflight.
        expected: Option<CommandId>,
        /// Command identity carried by the rejected specification.
        actual: Option<CommandId>,
    },
    /// The complete operation specification is internally incoherent.
    InvalidSpec(
        /// Exact specification validation failure.
        OperationSpecError,
    ),
}

/// Move-only rejection returned when a prepared registration cannot be committed.
///
/// The rejected specification is retained so command completion authority and
/// every other owned registration resource remain recoverable by the caller.
#[must_use = "a rejected registration contains owned resources that must be recovered"]
#[derive(Debug)]
pub(crate) struct OperationRegistrationRejection {
    /// Structured reason why the registration commit was rejected.
    error: OperationRegisterError,
    /// Original, unmodified operation specification supplied to the commit.
    spec: OperationSpec,
}

impl OperationRegistrationRejection {
    /// Creates a rejection that preserves the complete supplied specification.
    ///
    /// `error` describes the failed revalidation and `spec` retains every
    /// ownership-bearing input. Returns the move-only rejection wrapper.
    const fn new(error: OperationRegisterError, spec: OperationSpec) -> Self {
        Self { error, spec }
    }

    /// Returns the structured registration error without consuming the rejection.
    pub(crate) const fn error(&self) -> OperationRegisterError {
        self.error
    }

    /// Borrows the exact operation specification rejected by the ledger.
    pub(crate) const fn spec(&self) -> &OperationSpec {
        &self.spec
    }

    /// Splits the rejection into its error and original owned specification.
    ///
    /// Returns `(error, spec)` so callers can complete or retry the recovered
    /// operation owner without losing its linear completion authority.
    pub(crate) fn into_parts(self) -> (OperationRegisterError, OperationSpec) {
        (self.error, self.spec)
    }
}

/// Move-only proof that registration preflight admitted one operation identity.
#[derive(Debug)]
#[must_use = "a prepared registration token must be committed or explicitly discarded"]
pub(crate) struct OperationRegistrationToken {
    /// Operation identity checked for capacity and uniqueness.
    operation_id: OperationId,
    /// Optional command identity checked for independent uniqueness.
    command_id: Option<CommandId>,
}

impl OperationRegistrationToken {
    /// Returns the operation identity admitted by preflight.
    pub(crate) const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Returns the command identity bound by preflight, if this is command work.
    pub(crate) const fn command_id(&self) -> Option<CommandId> {
        self.command_id
    }
}

/// Generic first-terminal source accepted from ordinary Core use cases.
///
/// This mutation input deliberately has no peer-Reject variant. A copied
/// [`OperationTerminalCause`] diagnostic can therefore never be fed back into
/// the generic terminal API to bypass `CoreResources`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationClaimCause {
    /// The operation reached its typed successful outcome.
    Completed,
    /// A W=1 request received its matching Secondary.
    ResponseMatched,
    /// An exact same-transaction SxF0 aborted the request.
    TransactionAborted,
    /// A protocol or runtime timer expired first.
    TimedOut(
        /// Exact timeout kind that won the serialized terminal race.
        TimeoutKind,
    ),
    /// The selected Data session ended before completion.
    SessionDeselected,
    /// Permanent generation close ended the operation.
    GenerationClosing,
    /// A deterministic writer failure ended the operation.
    WriteFailed,
    /// Writer failure left peer visibility indeterminate.
    DeliveryIndeterminate,
    /// Cross-resource state disagreed and forced fail-closed handling.
    InternalInvariantFailure,
}

impl OperationClaimCause {
    /// Converts one permitted generic trigger into its stored diagnostic form.
    const fn into_terminal(self) -> OperationTerminalCause {
        match self {
            Self::Completed => OperationTerminalCause::Completed,
            Self::ResponseMatched => OperationTerminalCause::ResponseMatched,
            Self::TransactionAborted => OperationTerminalCause::TransactionAborted,
            Self::TimedOut(timeout) => OperationTerminalCause::TimedOut(timeout),
            Self::SessionDeselected => OperationTerminalCause::SessionDeselected,
            Self::GenerationClosing => OperationTerminalCause::GenerationClosing,
            Self::WriteFailed => OperationTerminalCause::WriteFailed,
            Self::DeliveryIndeterminate => OperationTerminalCause::DeliveryIndeterminate,
            Self::InternalInvariantFailure => OperationTerminalCause::InternalInvariantFailure,
        }
    }
}

/// Semantic terminal source retained after exactly-once claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationTerminalCause {
    /// The operation reached its typed successful outcome.
    Completed,
    /// A W=1 request received its matching Secondary.
    ResponseMatched,
    /// An exact same-transaction SxF0 aborted the request.
    TransactionAborted,
    /// A protocol or runtime timer expired first.
    TimedOut(
        /// Exact timeout kind that won the serialized terminal race.
        TimeoutKind,
    ),
    /// A globally unique peer Reject terminated the operation.
    PeerRejected(
        /// Opaque Reject terminal fact constructible only by the gated commit path.
        PeerRejectTerminal,
    ),
    /// The selected Data session ended before completion.
    SessionDeselected,
    /// Permanent generation close ended the operation.
    GenerationClosing,
    /// A deterministic writer failure ended the operation.
    WriteFailed,
    /// Writer failure left peer visibility indeterminate.
    DeliveryIndeterminate,
    /// Cross-resource state disagreed and forced fail-closed handling.
    InternalInvariantFailure,
}

/// Opaque peer-Reject terminal fact unavailable to generic terminal callers.
///
/// The private field has no public constructor. Only `OperationLedger` creates
/// this value after `CoreResources` presents its private mutation authority,
/// so generic callers cannot synthesize a peer-Reject terminal cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PeerRejectTerminal {
    /// Exact peer Reject that won the semantic terminal race.
    reference: RejectReference,
}

impl PeerRejectTerminal {
    /// Returns the exact peer Reject retained for completion and diagnostics.
    pub(crate) const fn reference(self) -> RejectReference {
        self.reference
    }
}

/// Semantic first-terminal-wins state stored independently of resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SemanticState {
    /// No terminal source has yet claimed the operation.
    Live,
    /// One exact source already claimed the sole terminal result.
    TerminalClaimed(
        /// First terminal source, retained for duplicate suppression.
        OperationTerminalCause,
    ),
}

/// Owner-specific result of the first semantic terminal claim.
#[must_use = "terminal ownership must be routed or explicitly handled"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CompletionTarget {
    /// One accepted command must receive a terminal completion.
    Command(
        /// Original move-only authority admitted with that command.
        CommandCompletionAuthority,
    ),
    /// Autonomous protocol work has no application command completion.
    Autonomous,
}

/// Whether an operation remains solely to clean exact resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationRetention {
    /// Terminal operation had no remaining resources and was removed.
    Retired,
    /// Operation remains live or as a bounded terminal resource shell.
    Retained {
        /// Whether the semantic result has already been claimed.
        terminal: bool,
        /// Exact Registry class still awaiting resource release.
        registry_binding: Option<OperationClass>,
        /// Exact writer identity still awaiting terminal cleanup.
        active_write: Option<WriteId>,
    },
}

/// Read-only operation facts needed for cross-ledger orchestration.
///
/// The snapshot deliberately omits `OperationOwner` and `CommandId`, so only
/// a first terminal claim can release completion authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OperationSnapshot {
    /// Exact operation described by this snapshot.
    operation_id: OperationId,
    /// Semantic purpose used to interpret write and Registry outcomes.
    purpose: OperationPurpose,
    /// Lifecycle domain used by selected-session reset.
    scope: OperationScope,
    /// Registry class still owned by this operation.
    registry_binding: Option<OperationClass>,
    /// Exact active write still awaiting terminal cleanup.
    active_write: Option<WriteId>,
    /// First terminal cause, or `None` while semantic work remains live.
    terminal_cause: Option<OperationTerminalCause>,
}

impl OperationSnapshot {
    /// Returns the exact operation identity.
    pub(crate) const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Returns the semantic operation purpose.
    pub(crate) const fn purpose(self) -> OperationPurpose {
        self.purpose
    }

    /// Returns the operation's lifecycle domain.
    pub(crate) const fn scope(self) -> OperationScope {
        self.scope
    }

    /// Returns the exact Registry class still awaiting release.
    pub(crate) const fn registry_binding(self) -> Option<OperationClass> {
        self.registry_binding
    }

    /// Returns the exact active write still awaiting cleanup.
    pub(crate) const fn active_write(self) -> Option<WriteId> {
        self.active_write
    }

    /// Returns the first terminal cause, or `None` for a live operation.
    pub(crate) const fn terminal_cause(self) -> Option<OperationTerminalCause> {
        self.terminal_cause
    }
}

/// Correlation-history effect of one first terminal claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalCorrelationRetention {
    /// Operation had no outbound correlation identity.
    None,
    /// Definitely invisible identity was discarded.
    DiscardedBeforeProceed,
    /// Possibly visible identity entered bounded terminal history.
    RetainedHistory {
        /// Oldest diagnostic operation evicted by the FIFO, if any.
        evicted_operation_id: Option<OperationId>,
    },
}

/// Result of attempting a generic semantic terminal claim.
#[must_use = "terminal decisions may contain an exactly-once completion permit"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TerminalClaimDecision {
    /// This source won and received the sole owner-specific completion target.
    Claimed {
        /// Command permit or autonomous marker released exactly once.
        target: CompletionTarget,
        /// Whether exact resources keep a bounded shell alive.
        retention: OperationRetention,
        /// Correlation history action committed with the semantic terminal.
        correlation: TerminalCorrelationRetention,
    },
    /// A prior terminal source already won; no completion target was reissued.
    AlreadyTerminal {
        /// First source retained by the operation shell.
        cause: OperationTerminalCause,
    },
    /// No live operation or resource shell uses the supplied identity.
    UnknownOperation,
}

/// Result of advancing an operation's exact write to peer visibility.
#[must_use = "visibility decisions report stale writes and invariant failures"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationVisibilityDecision {
    /// Both operation and correlation state advanced to possible visibility.
    Marked,
    /// The exact operation/write was already possibly visible.
    AlreadyVisible,
    /// A prior source already claimed semantic termination.
    AlreadyTerminal {
        /// First terminal source retained by the resource shell.
        cause: OperationTerminalCause,
    },
    /// The operation currently owns another active write.
    WrongWrite {
        /// Exact write still owned by the operation.
        expected: WriteId,
        /// Stale or unrelated write supplied by the caller.
        actual: WriteId,
    },
    /// Operation has no active write to authorize.
    NoActiveWrite,
    /// Correlation state was unexpectedly absent from an outbound operation.
    CorrelationInvariantViolation,
    /// No operation or resource shell uses the supplied identity.
    UnknownOperation,
}

/// Result of releasing an exact Registry ownership binding.
#[must_use = "registry release decisions must be reconciled with the registry owner"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegistryReleaseDecision {
    /// Exact class was released and any now-empty terminal shell was retired.
    Released {
        /// Operation retention after the resource release.
        retention: OperationRetention,
    },
    /// Operation currently has no Registry resource binding.
    NoBinding,
    /// Supplied class did not equal the operation's exact Registry binding.
    ClassMismatch {
        /// Registry class still owned by the operation.
        expected: OperationClass,
        /// Class supplied by the caller.
        actual: OperationClass,
    },
    /// No operation or resource shell uses the supplied identity.
    UnknownOperation,
}

/// Result of releasing one exact active write.
#[must_use = "write release decisions report stale and mismatched terminal events"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActiveWriteReleaseDecision {
    /// Exact write was cleared and any now-empty terminal shell was retired.
    Released {
        /// Operation retention after the write release.
        retention: OperationRetention,
    },
    /// Operation already has no active write.
    NoActiveWrite,
    /// Supplied write did not equal the operation's exact active write.
    WrongWrite {
        /// Exact write still owned by the operation.
        expected: WriteId,
        /// Stale or unrelated write supplied by the caller.
        actual: WriteId,
    },
    /// No operation or resource shell uses the supplied identity.
    UnknownOperation,
}

/// Move-only operation-level token returned by global Reject discovery.
#[derive(Debug)]
#[must_use = "a prepared reject commit token must be committed or explicitly discarded"]
pub(crate) struct RejectCommitToken {
    /// Private correlation proof revalidated during semantic commit.
    correlation: CorrelationRejectToken,
    /// Registry class that future `CoreResources` must release first.
    expected_registry_binding: Option<OperationClass>,
}

impl RejectCommitToken {
    /// Returns the uniquely attributed operation identity.
    pub(crate) const fn operation_id(&self) -> OperationId {
        self.correlation.operation_id()
    }

    /// Returns the Registry class that must be removed before commit.
    pub(crate) const fn expected_registry_binding(&self) -> Option<OperationClass> {
        self.expected_registry_binding
    }

    /// Returns the exact peer Reject reference.
    pub(crate) const fn reference(&self) -> RejectReference {
        self.correlation.reference()
    }
}

/// Public-to-Core result of globally discovering peer Reject attribution.
#[must_use = "a live Reject token must be committed or explicitly abandoned"]
#[derive(Debug)]
pub(crate) enum RejectDiscoveryDecision {
    /// Exactly one possibly visible live operation matched.
    Live(
        /// Move-only proof required for semantic commit.
        RejectCommitToken,
    ),
    /// No retained outbound identity matched.
    Unknown,
    /// Multiple candidates matched across the global live/history domain.
    Ambiguous {
        /// Number of matching live operations.
        live_matches: usize,
        /// Number of matching terminal records.
        terminal_matches: usize,
    },
    /// One terminal identity matched work completed another way.
    Late,
    /// The exact same Reject had already terminated the operation.
    Duplicate,
    /// A different Reject had already terminated the matching operation.
    Conflicting,
    /// Extension reason lacks configured attribution semantics.
    UnsupportedExtension,
    /// Reject event belongs to another TCP generation.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation stamped on the peer Reject event.
        actual: ConnectionGeneration,
    },
    /// Correlation and semantic operation ownership disagreed.
    InvariantViolation,
}

/// Move-only proof that a discovered Reject remained globally unique and live.
///
/// `CoreResources` obtains this proof before touching the Registry, performs
/// any required Registry removal synchronously, and then consumes the proof in
/// the infallible gated Operation commit.
#[must_use = "validated Reject authority must be committed or explicitly abandoned"]
#[derive(Debug)]
pub(crate) struct ValidatedRejectCommitToken {
    /// Original correlation token revalidated without mutating Operation state.
    token: RejectCommitToken,
}

impl ValidatedRejectCommitToken {
    /// Returns the exact operation whose peer Reject was validated.
    pub(crate) const fn operation_id(&self) -> OperationId {
        self.token.operation_id()
    }

    /// Returns the exact Registry class that must be removed first, if any.
    pub(crate) const fn expected_registry_binding(&self) -> Option<OperationClass> {
        self.token.expected_registry_binding()
    }

    /// Returns the exact peer Reject retained by this proof.
    pub(crate) const fn reference(&self) -> RejectReference {
        self.token.reference()
    }
}

/// Result of revalidating a unique live Reject before any cross-ledger mutation.
#[must_use = "validated Reject authority or stale diagnostics must be handled"]
#[derive(Debug)]
pub(crate) enum RejectValidationDecision {
    /// Discovery facts remain unique, live, and internally coherent.
    Validated(
        /// Move-only proof consumed only by the gated commit path.
        ValidatedRejectCommitToken,
    ),
    /// A prior terminal source won after discovery.
    AlreadyTerminal {
        /// First semantic terminal source.
        cause: OperationTerminalCause,
    },
    /// The operation or its live correlation disappeared after discovery.
    StaleToken,
    /// Immutable discovery facts no longer match the live correlation.
    InvariantViolation {
        /// Exact failed token-revalidation classification.
        validation: RejectTokenInvalidity,
    },
}

/// Operation-side result of the gated peer-Reject commit.
#[must_use = "peer-Reject completion authority and cleanup facts must be routed"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PeerRejectOperationCommit {
    /// Command completion authority or autonomous marker released exactly once.
    target: CompletionTarget,
    /// Whether an exact write keeps the terminal resource shell alive.
    retention: OperationRetention,
    /// Correlation-history mutation committed with the terminal transition.
    correlation: TerminalCorrelationRetention,
}

impl PeerRejectOperationCommit {
    /// Consumes the result into completion, retention, and correlation facts.
    pub(crate) fn into_parts(
        self,
    ) -> (
        CompletionTarget,
        OperationRetention,
        TerminalCorrelationRetention,
    ) {
        (self.target, self.retention, self.correlation)
    }
}

/// Stable operation-level diagnostic for stale Reject discovery facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RejectTokenInvalidity {
    /// The operation no longer has a live correlation identity.
    UnknownOperation,
    /// The operation's immutable outbound identity unexpectedly changed.
    IdentityChanged,
    /// The operation is no longer eligible for live Reject mutation.
    NotLiveEligible,
    /// The token belongs to another TCP generation.
    WrongGeneration,
    /// The Reject reference no longer matches the outbound identity.
    ReferenceMismatch,
    /// Another candidate appeared, so the discovery is no longer unique.
    NoLongerUnique,
    /// Operation Registry ownership changed after Reject discovery.
    RegistryBindingChanged,
}

/// Terminal outcome reported for one lifecycle-selected operation.
#[must_use = "newly claimed lifecycle commands require completion"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LifecycleTerminalStatus {
    /// Lifecycle transition won and released the sole completion target.
    Claimed(
        /// Command permit or autonomous marker.
        CompletionTarget,
    ),
    /// Operation was already terminal and released no completion target.
    AlreadyTerminal(
        /// First terminal cause retained by the resource shell.
        OperationTerminalCause,
    ),
}

/// Stable lifecycle disposition for one exact operation.
#[must_use = "lifecycle resources and completions must be handled"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LifecycleOperationDecision {
    /// Exact operation selected by close or session reset.
    operation_id: OperationId,
    /// Active write to cancel or await, if one remains.
    active_write: Option<WriteId>,
    /// Registry resource class to release, if one remains.
    registry_binding: Option<OperationClass>,
    /// Newly claimed or previously retained terminal status.
    terminal: LifecycleTerminalStatus,
}

impl LifecycleOperationDecision {
    /// Returns the lifecycle-selected operation identity.
    pub(crate) const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Returns the exact active write still requiring cleanup.
    pub(crate) const fn active_write(&self) -> Option<WriteId> {
        self.active_write
    }

    /// Returns the exact Registry class still requiring release.
    pub(crate) const fn registry_binding(&self) -> Option<OperationClass> {
        self.registry_binding
    }

    /// Borrows the owner-specific terminal claim status.
    pub(crate) const fn terminal(&self) -> &LifecycleTerminalStatus {
        &self.terminal
    }

    /// Consumes the disposition into its operation, resource, and terminal fields.
    pub(crate) fn into_parts(
        self,
    ) -> (
        OperationId,
        Option<WriteId>,
        Option<OperationClass>,
        LifecycleTerminalStatus,
    ) {
        (
            self.operation_id,
            self.active_write,
            self.registry_binding,
            self.terminal,
        )
    }
}

/// Idempotent semantic close decision for one generation.
#[must_use = "close decisions contain cleanup work and command completions"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OperationCloseDecision {
    /// Whether this invocation fenced admission and performed first close.
    began_close: bool,
    /// Stable operation-id-ordered lifecycle dispositions.
    operations: Vec<LifecycleOperationDecision>,
}

impl OperationCloseDecision {
    /// Returns whether this call performed the first close transition.
    pub(crate) const fn began_close(&self) -> bool {
        self.began_close
    }

    /// Borrows the stable operation-id-ordered close dispositions.
    pub(crate) fn operations(&self) -> &[LifecycleOperationDecision] {
        &self.operations
    }

    /// Consumes the decision and returns all close dispositions.
    pub(crate) fn into_operations(self) -> Vec<LifecycleOperationDecision> {
        self.operations
    }
}

/// Data-session reset decision that leaves generation control work intact.
#[must_use = "reset decisions contain cleanup work and command completions"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OperationSessionResetDecision {
    /// Stable operation-id-ordered selected-session dispositions.
    operations: Vec<LifecycleOperationDecision>,
}

impl OperationSessionResetDecision {
    /// Returns whether this reset found no new selected-session cleanup work.
    pub(crate) fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Borrows the stable operation-id-ordered reset dispositions.
    pub(crate) fn operations(&self) -> &[LifecycleOperationDecision] {
        &self.operations
    }

    /// Consumes the decision and returns all reset dispositions.
    pub(crate) fn into_operations(self) -> Vec<LifecycleOperationDecision> {
        self.operations
    }
}

/// One bounded semantic operation and any resources not yet released.
#[derive(Debug)]
struct OperationEntry {
    /// Live command or autonomous authority; taken exactly once at terminal claim.
    owner: Option<OperationOwner>,
    /// Command identity retained for uniqueness until the resource shell retires.
    command_id: Option<CommandId>,
    /// Semantic purpose retained for diagnostics and integration validation.
    purpose: OperationPurpose,
    /// Lifecycle domain used by re-selectable session reset.
    scope: OperationScope,
    /// Transaction Registry class still owned by this operation.
    registry_binding: Option<OperationClass>,
    /// Exact write still awaiting terminal cleanup.
    active_write: Option<WriteId>,
    /// First-terminal-wins semantic state.
    semantic: SemanticState,
    /// Whether a prior session reset already returned this entry's resources.
    reset_notified: bool,
}

/// Bounded generation-local owner of semantic operation completion.
pub(crate) struct OperationLedger {
    /// TCP generation whose operations this ledger owns.
    generation: ConnectionGeneration,
    /// Logical maximum including live entries and terminal resource shells.
    capacity: usize,
    /// Whether permanent close has fenced all further registration.
    closing: bool,
    /// Operation entries keyed by generation-unique identity.
    entries: HashMap<OperationId, OperationEntry>,
    /// Command identity to exact owning operation, including terminal shells.
    command_owners: HashMap<CommandId, OperationId>,
    /// Private live and bounded terminal outbound-header index.
    correlations: OutboundCorrelationIndex,
}

impl OperationLedger {
    /// Creates a lazy-allocation bounded ledger for one TCP generation.
    ///
    /// Neither logical capacity is used for eager allocation, so even
    /// `usize::MAX` construction remains allocation-free and non-panicking.
    pub(crate) fn new(
        generation: ConnectionGeneration,
        capacity: usize,
        correlation_history_capacity: usize,
    ) -> Result<Self, OperationLedgerBuildError> {
        if capacity == 0 {
            return Err(OperationLedgerBuildError::ZeroOperationCapacity);
        }
        let correlations = OutboundCorrelationIndex::new(generation, correlation_history_capacity)
            .map_err(|error| match error {
                CorrelationBuildError::ZeroTerminalHistoryCapacity => {
                    OperationLedgerBuildError::ZeroCorrelationHistoryCapacity
                }
            })?;
        Ok(Self {
            generation,
            capacity,
            closing: false,
            entries: HashMap::new(),
            command_owners: HashMap::new(),
            correlations,
        })
    }

    /// Returns the TCP generation owned by this ledger.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns whether permanent close has fenced registration.
    pub(crate) const fn is_closing(&self) -> bool {
        self.closing
    }

    /// Returns the number of live entries and terminal resource shells.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns the number of command identities protected from duplicate admission.
    pub(crate) fn command_owner_len(&self) -> usize {
        self.command_owners.len()
    }

    /// Returns whether no live operation or terminal shell remains.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of retained terminal Reject-correlation records.
    pub(crate) fn correlation_history_len(&self) -> usize {
        self.correlations.terminal_len()
    }

    /// Returns orchestration facts without exposing command completion ownership.
    pub(crate) fn operation_snapshot(
        &self,
        operation_id: OperationId,
    ) -> Option<OperationSnapshot> {
        self.entries
            .get(&operation_id)
            .map(|entry| OperationSnapshot {
                operation_id,
                purpose: entry.purpose,
                scope: entry.scope,
                registry_binding: entry.registry_binding,
                active_write: entry.active_write,
                terminal_cause: match entry.semantic {
                    SemanticState::Live => None,
                    SemanticState::TerminalClaimed(cause) => Some(cause),
                },
            })
    }

    /// Performs non-mutating validation, capacity, operation-ID, and command-ID preflight.
    pub(crate) fn prepare_registration(
        &self,
        spec: &OperationSpec,
    ) -> Result<OperationRegistrationToken, OperationRegisterError> {
        spec.validate()
            .map_err(OperationRegisterError::InvalidSpec)?;
        let command_id = spec.owner.command_id();
        self.ensure_registration_allowed(spec.operation_id, command_id)?;
        Ok(OperationRegistrationToken {
            operation_id: spec.operation_id,
            command_id,
        })
    }

    /// Commits a fully coherent specification after revalidating preflight.
    ///
    /// The token is move-only, and all failure paths leave the entries and
    /// correlation index unchanged.
    pub(crate) fn commit_registration(
        &mut self,
        token: OperationRegistrationToken,
        spec: OperationSpec,
    ) -> Result<(), OperationRegistrationRejection> {
        if token.operation_id != spec.operation_id {
            let error = OperationRegisterError::TokenOperationMismatch {
                expected: token.operation_id,
                actual: spec.operation_id,
            };
            return Err(OperationRegistrationRejection::new(error, spec));
        }
        let command_id = spec.owner.command_id();
        if token.command_id != command_id {
            let error = OperationRegisterError::TokenCommandMismatch {
                expected: token.command_id,
                actual: command_id,
            };
            return Err(OperationRegistrationRejection::new(error, spec));
        }
        if let Err(error) = self.ensure_registration_allowed(spec.operation_id, command_id) {
            return Err(OperationRegistrationRejection::new(error, spec));
        }
        if let Err(error) = spec.validate() {
            return Err(OperationRegistrationRejection::new(
                OperationRegisterError::InvalidSpec(error),
                spec,
            ));
        }

        if let Some(write) = spec.active_write {
            if let Err(error) = self
                .correlations
                .register(spec.operation_id, write.identity())
            {
                let error = match error {
                    CorrelationRegisterError::DuplicateOperation { operation_id } => {
                        OperationRegisterError::DuplicateOperation { operation_id }
                    }
                };
                return Err(OperationRegistrationRejection::new(error, spec));
            }
        }
        let OperationSpec {
            operation_id,
            owner,
            purpose,
            scope,
            registry_binding,
            active_write,
        } = spec;
        let previous = self.entries.insert(
            operation_id,
            OperationEntry {
                owner: Some(owner),
                command_id,
                purpose,
                scope,
                registry_binding,
                active_write: active_write.map(ActiveOperationWrite::write_id),
                semantic: SemanticState::Live,
                reset_notified: false,
            },
        );
        debug_assert!(previous.is_none(), "registration was revalidated as unique");
        if let Some(command_id) = command_id {
            let previous = self.command_owners.insert(command_id, operation_id);
            debug_assert!(
                previous.is_none(),
                "command registration was revalidated as unique"
            );
        }
        Ok(())
    }

    /// Advances one exact active write and its correlation to possible visibility.
    pub(crate) fn mark_may_be_visible(
        &mut self,
        operation_id: OperationId,
        write_id: WriteId,
    ) -> OperationVisibilityDecision {
        let Some(entry) = self.entries.get(&operation_id) else {
            return OperationVisibilityDecision::UnknownOperation;
        };
        if let SemanticState::TerminalClaimed(cause) = entry.semantic {
            return OperationVisibilityDecision::AlreadyTerminal { cause };
        }
        let Some(expected) = entry.active_write else {
            return OperationVisibilityDecision::NoActiveWrite;
        };
        if expected != write_id {
            return OperationVisibilityDecision::WrongWrite {
                expected,
                actual: write_id,
            };
        }
        match self.correlations.mark_may_be_visible(operation_id) {
            CorrelationVisibilityDecision::Marked => OperationVisibilityDecision::Marked,
            CorrelationVisibilityDecision::AlreadyVisible => {
                OperationVisibilityDecision::AlreadyVisible
            }
            CorrelationVisibilityDecision::UnknownOperation => {
                OperationVisibilityDecision::CorrelationInvariantViolation
            }
        }
    }

    /// Releases an exact Registry class without interpreting transaction state.
    ///
    /// Future `CoreResources` must call the Registry first and pass only the
    /// class confirmed by its removal decision.
    pub(crate) fn release_registry_binding(
        &mut self,
        operation_id: OperationId,
        class: OperationClass,
    ) -> RegistryReleaseDecision {
        let Some(entry) = self.entries.get_mut(&operation_id) else {
            return RegistryReleaseDecision::UnknownOperation;
        };
        match entry.registry_binding {
            None => RegistryReleaseDecision::NoBinding,
            Some(expected) if expected != class => RegistryReleaseDecision::ClassMismatch {
                expected,
                actual: class,
            },
            Some(_) => {
                entry.registry_binding = None;
                RegistryReleaseDecision::Released {
                    retention: self.retire_if_complete(operation_id),
                }
            }
        }
    }

    /// Releases only the exact active `WriteId`.
    ///
    /// A semantically terminal fast-response shell is retired only here; a
    /// still-live request keeps its independent outbound correlation identity
    /// after write cleanup so a later peer Reject can still match it.
    pub(crate) fn finish_active_write(
        &mut self,
        operation_id: OperationId,
        write_id: WriteId,
    ) -> ActiveWriteReleaseDecision {
        let Some(entry) = self.entries.get_mut(&operation_id) else {
            return ActiveWriteReleaseDecision::UnknownOperation;
        };
        match entry.active_write {
            None => ActiveWriteReleaseDecision::NoActiveWrite,
            Some(expected) if expected != write_id => ActiveWriteReleaseDecision::WrongWrite {
                expected,
                actual: write_id,
            },
            Some(_) => {
                entry.active_write = None;
                ActiveWriteReleaseDecision::Released {
                    retention: self.retire_if_complete(operation_id),
                }
            }
        }
    }

    /// Attempts the first semantic terminal claim for one operation.
    ///
    /// The completion target is issued immediately even when Registry/write
    /// resources retain a bounded shell. Later terminal sources receive only
    /// `AlreadyTerminal` and can never obtain another permit.
    pub(crate) fn claim_terminal(
        &mut self,
        operation_id: OperationId,
        cause: OperationClaimCause,
    ) -> TerminalClaimDecision {
        self.claim_terminal_inner(operation_id, cause.into_terminal())
    }

    /// Discovers global unique peer Reject attribution without mutation.
    pub(crate) fn discover_peer_reject(
        &self,
        reference: RejectReference,
    ) -> RejectDiscoveryDecision {
        match self.correlations.discover_peer_reject(reference) {
            CorrelationRejectDiscovery::Live(correlation) => {
                let Some(entry) = self.entries.get(&correlation.operation_id()) else {
                    return RejectDiscoveryDecision::InvariantViolation;
                };
                if !matches!(entry.semantic, SemanticState::Live) {
                    return RejectDiscoveryDecision::InvariantViolation;
                }
                RejectDiscoveryDecision::Live(RejectCommitToken {
                    correlation,
                    expected_registry_binding: entry.registry_binding,
                })
            }
            CorrelationRejectDiscovery::Unknown => RejectDiscoveryDecision::Unknown,
            CorrelationRejectDiscovery::Ambiguous {
                live_matches,
                terminal_matches,
            } => RejectDiscoveryDecision::Ambiguous {
                live_matches,
                terminal_matches,
            },
            CorrelationRejectDiscovery::Late => RejectDiscoveryDecision::Late,
            CorrelationRejectDiscovery::Duplicate => RejectDiscoveryDecision::Duplicate,
            CorrelationRejectDiscovery::Conflicting => RejectDiscoveryDecision::Conflicting,
            CorrelationRejectDiscovery::UnsupportedExtension => {
                RejectDiscoveryDecision::UnsupportedExtension
            }
            CorrelationRejectDiscovery::WrongGeneration { expected, actual } => {
                RejectDiscoveryDecision::WrongGeneration { expected, actual }
            }
        }
    }

    /// Revalidates one discovered peer Reject before any cross-ledger mutation.
    ///
    /// The private `authority` can only be borrowed by `CoreResources`. A valid
    /// result freezes every Operation-side fact required by the subsequent
    /// synchronous Registry removal and Operation commit.
    pub(crate) fn validate_peer_reject_commit(
        &self,
        _authority: &PeerRejectMutationAuthority,
        token: RejectCommitToken,
    ) -> RejectValidationDecision {
        let operation_id = token.operation_id();
        let Some(entry) = self.entries.get(&operation_id) else {
            return RejectValidationDecision::StaleToken;
        };
        if let SemanticState::TerminalClaimed(cause) = entry.semantic {
            return RejectValidationDecision::AlreadyTerminal { cause };
        }
        if entry.registry_binding != token.expected_registry_binding {
            return RejectValidationDecision::InvariantViolation {
                validation: RejectTokenInvalidity::RegistryBindingChanged,
            };
        }
        let validation = self.correlations.validate_reject_token(&token.correlation);
        if validation != CorrelationTokenValidation::Valid {
            return RejectValidationDecision::InvariantViolation {
                validation: Self::map_token_invalidity(validation),
            };
        }
        RejectValidationDecision::Validated(ValidatedRejectCommitToken { token })
    }

    /// Commits a prevalidated peer Reject after exact Registry release.
    ///
    /// `CoreResources` is the only owner of both required capability values.
    /// It performs no Operation mutation between validation and this call.
    /// Therefore the transition is infallible by construction: the Registry
    /// binding is cleared and the original move-only command authority is
    /// terminalized in the same Operation mutation.
    pub(crate) fn commit_peer_reject(
        &mut self,
        _authority: &mut PeerRejectMutationAuthority,
        token: ValidatedRejectCommitToken,
        registry_release: Option<PeerRejectRegistryRelease>,
    ) -> PeerRejectOperationCommit {
        let operation_id = token.operation_id();
        let reference = token.reference();
        let released_class = match (token.expected_registry_binding(), registry_release.as_ref()) {
            (None, None) => None,
            (Some(expected), Some(release)) => {
                assert_eq!(
                    release.operation_id(),
                    operation_id,
                    "CoreResources must release the exact peer-Reject operation"
                );
                assert_eq!(
                    release.class(),
                    expected,
                    "CoreResources must release the exact peer-Reject Registry class"
                );
                Some(release.class())
            }
            (None, Some(_)) => {
                panic!("a peer-Reject operation without Registry ownership cannot accept a release")
            }
            (Some(_), None) => {
                panic!("a peer-Reject operation with Registry ownership requires a release")
            }
        };
        {
            let entry = self
                .entries
                .get_mut(&operation_id)
                .expect("validated peer-Reject operation must remain registered");
            assert!(
                matches!(entry.semantic, SemanticState::Live),
                "validated peer-Reject operation must remain semantically live"
            );
            assert_eq!(
                entry.registry_binding, released_class,
                "validated peer-Reject binding must remain unchanged"
            );
            entry.registry_binding = None;
        }
        match self.claim_terminal_inner(
            operation_id,
            OperationTerminalCause::PeerRejected(PeerRejectTerminal { reference }),
        ) {
            TerminalClaimDecision::Claimed {
                target,
                retention,
                correlation,
            } => PeerRejectOperationCommit {
                target,
                retention,
                correlation,
            },
            TerminalClaimDecision::AlreadyTerminal { .. }
            | TerminalClaimDecision::UnknownOperation => {
                unreachable!("validated peer-Reject authority must commit exactly once")
            }
        }
    }

    /// Ends newly observed selected-session Data work in stable ID order.
    ///
    /// A terminal shell is reported once so its active write can be cancelled,
    /// but its first semantic result is not replaced. Generation-control work
    /// is untouched and future newly registered Data work can participate in a
    /// later re-selectable reset.
    pub(crate) fn reset_selected_session(&mut self) -> OperationSessionResetDecision {
        let mut operation_ids: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(operation_id, entry)| {
                (entry.scope == OperationScope::SelectedSessionData && !entry.reset_notified)
                    .then_some(*operation_id)
            })
            .collect();
        operation_ids.sort_unstable();

        let mut operations = Vec::new();
        for operation_id in operation_ids {
            let (active_write, registry_binding, semantic) = {
                let entry = self
                    .entries
                    .get_mut(&operation_id)
                    .expect("reset snapshot contains an existing operation");
                entry.reset_notified = true;
                (entry.active_write, entry.registry_binding, entry.semantic)
            };
            let terminal = match semantic {
                SemanticState::Live => {
                    match self.claim_terminal_inner(
                        operation_id,
                        OperationTerminalCause::SessionDeselected,
                    ) {
                        TerminalClaimDecision::Claimed { target, .. } => {
                            LifecycleTerminalStatus::Claimed(target)
                        }
                        TerminalClaimDecision::AlreadyTerminal { cause } => {
                            LifecycleTerminalStatus::AlreadyTerminal(cause)
                        }
                        TerminalClaimDecision::UnknownOperation => {
                            unreachable!("reset operation cannot disappear before its claim")
                        }
                    }
                }
                SemanticState::TerminalClaimed(cause) => {
                    LifecycleTerminalStatus::AlreadyTerminal(cause)
                }
            };
            operations.push(LifecycleOperationDecision {
                operation_id,
                active_write,
                registry_binding,
                terminal,
            });
        }
        OperationSessionResetDecision { operations }
    }

    /// Permanently fences registration and claims every live operation once.
    ///
    /// The first call returns stable operation-id-ordered resource and
    /// completion dispositions. Later calls are idempotent and empty.
    pub(crate) fn begin_close(&mut self) -> OperationCloseDecision {
        if self.closing {
            return OperationCloseDecision {
                began_close: false,
                operations: Vec::new(),
            };
        }
        self.closing = true;
        let mut operation_ids: Vec<_> = self.entries.keys().copied().collect();
        operation_ids.sort_unstable();

        let mut operations = Vec::new();
        for operation_id in operation_ids {
            let entry = self
                .entries
                .get(&operation_id)
                .expect("close snapshot contains an existing operation");
            let active_write = entry.active_write;
            let registry_binding = entry.registry_binding;
            let semantic = entry.semantic;
            let terminal = match semantic {
                SemanticState::Live => {
                    match self.claim_terminal_inner(
                        operation_id,
                        OperationTerminalCause::GenerationClosing,
                    ) {
                        TerminalClaimDecision::Claimed { target, .. } => {
                            LifecycleTerminalStatus::Claimed(target)
                        }
                        TerminalClaimDecision::AlreadyTerminal { cause } => {
                            LifecycleTerminalStatus::AlreadyTerminal(cause)
                        }
                        TerminalClaimDecision::UnknownOperation => {
                            unreachable!("close operation cannot disappear before its claim")
                        }
                    }
                }
                SemanticState::TerminalClaimed(cause) => {
                    LifecycleTerminalStatus::AlreadyTerminal(cause)
                }
            };
            operations.push(LifecycleOperationDecision {
                operation_id,
                active_write,
                registry_binding,
                terminal,
            });
        }
        OperationCloseDecision {
            began_close: true,
            operations,
        }
    }

    /// Validates close, logical capacity, and operation identity for admission.
    fn ensure_registration_allowed(
        &self,
        operation_id: OperationId,
        command_id: Option<CommandId>,
    ) -> Result<(), OperationRegisterError> {
        if self.closing {
            return Err(OperationRegisterError::Closing);
        }
        if self.entries.contains_key(&operation_id) {
            return Err(OperationRegisterError::DuplicateOperation { operation_id });
        }
        if let Some(command_id) = command_id {
            if let Some(existing_operation_id) = self.command_owners.get(&command_id).copied() {
                return Err(OperationRegisterError::DuplicateCommand {
                    command_id,
                    existing_operation_id,
                });
            }
        }
        if self.entries.len() >= self.capacity {
            return Err(OperationRegisterError::CapacityExhausted {
                capacity: self.capacity,
            });
        }
        Ok(())
    }

    /// Applies the sole terminal transition and returns its unique owner target.
    fn claim_terminal_inner(
        &mut self,
        operation_id: OperationId,
        cause: OperationTerminalCause,
    ) -> TerminalClaimDecision {
        let target = {
            let Some(entry) = self.entries.get_mut(&operation_id) else {
                return TerminalClaimDecision::UnknownOperation;
            };
            if let SemanticState::TerminalClaimed(previous) = entry.semantic {
                return TerminalClaimDecision::AlreadyTerminal { cause: previous };
            }
            let owner = entry
                .owner
                .take()
                .expect("live semantic operation must retain terminal authority");
            entry.semantic = SemanticState::TerminalClaimed(cause);
            Self::completion_target(owner)
        };
        let correlation_cause = match cause {
            OperationTerminalCause::PeerRejected(terminal) => {
                CorrelationTerminalCause::PeerRejected(terminal.reference())
            }
            _ => CorrelationTerminalCause::Other,
        };
        let correlation = Self::map_terminal_correlation(
            self.correlations
                .terminalize(operation_id, correlation_cause),
        );
        let retention = self.retire_if_complete(operation_id);
        TerminalClaimDecision::Claimed {
            target,
            retention,
            correlation,
        }
    }

    /// Builds a move-only command permit or autonomous terminal marker.
    fn completion_target(owner: OperationOwner) -> CompletionTarget {
        match owner {
            OperationOwner::Command(authority) => CompletionTarget::Command(authority),
            OperationOwner::Autonomous => CompletionTarget::Autonomous,
        }
    }

    /// Maps private correlation cleanup into the operation-level decision.
    fn map_terminal_correlation(
        decision: CorrelationTerminalDecision,
    ) -> TerminalCorrelationRetention {
        match decision {
            CorrelationTerminalDecision::NoCorrelation => TerminalCorrelationRetention::None,
            CorrelationTerminalDecision::DiscardedBeforeProceed => {
                TerminalCorrelationRetention::DiscardedBeforeProceed
            }
            CorrelationTerminalDecision::Retained {
                evicted_operation_id,
            } => TerminalCorrelationRetention::RetainedHistory {
                evicted_operation_id,
            },
        }
    }

    /// Maps private correlation validation into a stable operation diagnostic.
    fn map_token_invalidity(validation: CorrelationTokenValidation) -> RejectTokenInvalidity {
        match validation {
            CorrelationTokenValidation::Valid => {
                unreachable!("valid tokens are committed before invalidity mapping")
            }
            CorrelationTokenValidation::UnknownOperation => RejectTokenInvalidity::UnknownOperation,
            CorrelationTokenValidation::IdentityChanged => RejectTokenInvalidity::IdentityChanged,
            CorrelationTokenValidation::NotLiveEligible => RejectTokenInvalidity::NotLiveEligible,
            CorrelationTokenValidation::WrongGeneration => RejectTokenInvalidity::WrongGeneration,
            CorrelationTokenValidation::ReferenceMismatch => {
                RejectTokenInvalidity::ReferenceMismatch
            }
            CorrelationTokenValidation::NoLongerUnique => RejectTokenInvalidity::NoLongerUnique,
        }
    }

    /// Removes a terminal entry only after both exact resource axes are empty.
    fn retire_if_complete(&mut self, operation_id: OperationId) -> OperationRetention {
        let Some(entry) = self.entries.get(&operation_id) else {
            return OperationRetention::Retired;
        };
        let terminal = matches!(entry.semantic, SemanticState::TerminalClaimed(_));
        let registry_binding = entry.registry_binding;
        let active_write = entry.active_write;
        if terminal && registry_binding.is_none() && active_write.is_none() {
            let removed = self
                .entries
                .remove(&operation_id)
                .expect("retirement candidate was read immediately before removal");
            if let Some(command_id) = removed.command_id {
                let removed_owner = self.command_owners.remove(&command_id);
                debug_assert_eq!(
                    removed_owner,
                    Some(operation_id),
                    "command index must point at its exact retiring operation"
                );
            }
            OperationRetention::Retired
        } else {
            OperationRetention::Retained {
                terminal,
                registry_binding,
                active_write,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::secs2::SecsItem;

    use crate::hsms::{
        contracts::{
            CommandCompletionAuthority, OperationOwner, OutboundHeaderIdentity,
            OutboundMessageShapeError, RejectReference,
        },
        core::{
            operation::{
                ActiveOperationWrite, ActiveWriteReleaseDecision, LifecycleTerminalStatus,
                OperationClaimCause, OperationLedger, OperationLedgerBuildError, OperationPurpose,
                OperationRegisterError, OperationRetention, OperationScope,
                OperationSessionResetDecision, OperationSpec, OperationTerminalCause,
                OperationVisibilityDecision, RegistryReleaseDecision, RejectDiscoveryDecision,
                RejectValidationDecision, TerminalClaimDecision, TerminalCorrelationRetention,
            },
            resources::authority::PeerRejectMutationAuthority,
            transaction::PeerRejectRegistryRelease,
            transaction::{ControlKind, OneWayKind, OperationClass},
        },
        error::OperationError,
        model::ids::{
            CommandId, ConnectionGeneration, Function, OperationId, SystemBytes, WriteId,
        },
        protocol::{
            header::{ControlMessage, DataHeader, RejectReason},
            message::{DataMessage, ProtocolMessage},
        },
        SessionId, Stream,
    };

    /// Deterministic generation used by all operation tests.
    const GENERATION: ConnectionGeneration = ConnectionGeneration::new(7);

    /// Creates a lazy bounded ledger with deterministic capacities.
    fn ledger(capacity: usize, history_capacity: usize) -> OperationLedger {
        OperationLedger::new(GENERATION, capacity, history_capacity)
            .expect("non-zero operation capacities")
    }

    /// Creates a typed outbound Data identity for one System Bytes value.
    fn data_identity(
        system_bytes: u32,
        function: u8,
        reply_expected: bool,
    ) -> OutboundHeaderIdentity {
        let message = ProtocolMessage::Data(DataMessage::new(
            DataHeader::new(
                SessionId::new(3).expect("non-control session"),
                Stream::new(1).expect("seven-bit stream"),
                Function::new(function),
                reply_expected,
                SystemBytes::new(system_bytes),
            ),
            None,
        ));
        OutboundHeaderIdentity::from_protocol_message(&message)
            .expect("typed outbound message shape")
    }

    /// Creates a typed outbound Select request identity.
    fn control_identity(system_bytes: u32) -> OutboundHeaderIdentity {
        OutboundHeaderIdentity::from_protocol_message(&ProtocolMessage::Control(
            ControlMessage::SelectRequest {
                session_id: 3,
                system_bytes: SystemBytes::new(system_bytes),
            },
        ))
        .expect("typed outbound message shape")
    }

    /// Creates a coherent W=1 command operation specification.
    fn request_spec(operation: u64, command: u64, write: u64, system_bytes: u32) -> OperationSpec {
        OperationSpec::new(
            OperationId::new(operation),
            OperationOwner::Command(CommandCompletionAuthority::for_test(CommandId::new(
                command,
            ))),
            OperationPurpose::Request,
            OperationScope::SelectedSessionData,
            Some(OperationClass::Request),
            Some(ActiveOperationWrite::new(
                WriteId::new(write),
                data_identity(system_bytes, 1, true),
            )),
        )
    }

    /// Creates a coherent W=0 command operation specification.
    fn send_spec(operation: u64, command: u64, write: u64, system_bytes: u32) -> OperationSpec {
        OperationSpec::new(
            OperationId::new(operation),
            OperationOwner::Command(CommandCompletionAuthority::for_test(CommandId::new(
                command,
            ))),
            OperationPurpose::Send,
            OperationScope::SelectedSessionData,
            Some(OperationClass::OneWay(OneWayKind::Data)),
            Some(ActiveOperationWrite::new(
                WriteId::new(write),
                data_identity(system_bytes, 1, false),
            )),
        )
    }

    /// Creates a coherent autonomous Select operation specification.
    fn autonomous_control_spec(operation: u64, write: u64, system_bytes: u32) -> OperationSpec {
        OperationSpec::new(
            OperationId::new(operation),
            OperationOwner::Autonomous,
            OperationPurpose::Control(ControlKind::Select),
            OperationScope::GenerationControl,
            Some(OperationClass::Control(ControlKind::Select)),
            Some(ActiveOperationWrite::new(
                WriteId::new(write),
                control_identity(system_bytes),
            )),
        )
    }

    /// Registers a specification through its move-only preflight token.
    fn register(ledger: &mut OperationLedger, spec: OperationSpec) {
        let token = ledger
            .prepare_registration(&spec)
            .expect("registration preflight");
        ledger
            .commit_registration(token, spec)
            .expect("registration commit");
    }

    /// Builds a base-standard unsupported-PType Reject for a Data identity.
    fn reject(system_bytes: u32) -> RejectReference {
        RejectReference::new(
            GENERATION,
            3,
            0,
            RejectReason::UNSUPPORTED_PTYPE,
            SystemBytes::new(system_bytes),
        )
    }

    /// Extracts a command id from a newly claimed terminal decision.
    fn claimed_command(decision: TerminalClaimDecision) -> CommandId {
        match decision {
            TerminalClaimDecision::Claimed {
                target: super::CompletionTarget::Command(permit),
                ..
            } => {
                let command_id = permit.command_id();
                let completion = permit.failed(OperationError::RuntimeStopped);
                assert_eq!(completion.command_id(), command_id);
                command_id
            }
            other => panic!("expected command terminal claim, got {other:?}"),
        }
    }

    /// Confirms logical capacities are validated without eagerly allocating
    /// their potentially extreme configured size.
    #[test]
    fn construction_is_bounded_and_lazy() {
        assert!(OperationLedger::new(GENERATION, usize::MAX, usize::MAX).is_ok());
        assert!(matches!(
            OperationLedger::new(GENERATION, 0, 1),
            Err(OperationLedgerBuildError::ZeroOperationCapacity)
        ));
        assert!(matches!(
            OperationLedger::new(GENERATION, 1, 0),
            Err(OperationLedgerBuildError::ZeroCorrelationHistoryCapacity)
        ));
    }

    /// Confirms one CommandId cannot own two Operations, including while the
    /// first command has completed but its exact resource shell still exists.
    #[test]
    fn command_identity_has_an_independent_unique_owner_index() {
        let mut ledger = ledger(3, 3);
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        assert_eq!(ledger.command_owner_len(), 1);

        let duplicate = send_spec(2, 10, 200, 0x22);
        assert!(matches!(
            ledger.prepare_registration(&duplicate),
            Err(OperationRegisterError::DuplicateCommand {
                command_id,
                existing_operation_id,
            }) if command_id == CommandId::new(10)
                && existing_operation_id == OperationId::new(1)
        ));
        assert_eq!(
            claimed_command(
                ledger.claim_terminal(OperationId::new(1), OperationClaimCause::ResponseMatched,)
            ),
            CommandId::new(10)
        );
        assert_eq!(ledger.command_owner_len(), 1);
        assert!(matches!(
            ledger.prepare_registration(&duplicate),
            Err(OperationRegisterError::DuplicateCommand { .. })
        ));

        assert!(matches!(
            ledger.release_registry_binding(OperationId::new(1), OperationClass::Request),
            RegistryReleaseDecision::Released { .. }
        ));
        assert!(matches!(
            ledger.finish_active_write(OperationId::new(1), WriteId::new(100)),
            ActiveWriteReleaseDecision::Released {
                retention: OperationRetention::Retired
            }
        ));
        assert_eq!(ledger.command_owner_len(), 0);
    }

    /// Confirms a stale preflight token cannot bypass CommandId uniqueness
    /// after another registration wins; every Operation and correlation store
    /// remains reusable by an unrelated replacement after the failed commit.
    #[test]
    fn stale_registration_token_cannot_commit_a_duplicate_command_owner() {
        let mut ledger = ledger(3, 3);
        let stale = request_spec(1, 10, 100, 0x11);
        let stale_token = ledger
            .prepare_registration(&stale)
            .expect("initial stale-token preflight");

        register(&mut ledger, send_spec(2, 10, 200, 0x22));
        let rejection = ledger
            .commit_registration(stale_token, stale)
            .expect_err("duplicate command owner must reject stale registration");
        assert!(matches!(
            rejection.error(),
            OperationRegisterError::DuplicateCommand {
                command_id,
                existing_operation_id,
            } if command_id == CommandId::new(10)
                && existing_operation_id == OperationId::new(2)
        ));
        assert_eq!(rejection.spec().operation_id(), OperationId::new(1));
        let (_, recovered_spec) = rejection.into_parts();
        match recovered_spec.into_owner() {
            OperationOwner::Command(authority) => {
                drop(authority.failed(OperationError::RuntimeStopped));
            }
            OperationOwner::Autonomous => panic!("expected recovered command authority"),
        }
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.command_owner_len(), 1);
        assert_eq!(ledger.correlation_history_len(), 0);
        assert!(ledger.operation_snapshot(OperationId::new(1)).is_none());
        assert!(ledger.operation_snapshot(OperationId::new(2)).is_some());

        register(&mut ledger, request_spec(1, 11, 100, 0x11));
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger.command_owner_len(), 2);
    }

    /// Confirms semantic purpose validation distinguishes the W-bit and exact
    /// transactional control SType instead of accepting a coarse message role.
    #[test]
    fn operation_spec_rejects_w_bit_and_control_stype_confusion() {
        let ledger = ledger(4, 4);
        let request_with_w0 = OperationSpec::new(
            OperationId::new(1),
            OperationOwner::Command(CommandCompletionAuthority::for_test(CommandId::new(10))),
            OperationPurpose::Request,
            OperationScope::SelectedSessionData,
            Some(OperationClass::Request),
            Some(ActiveOperationWrite::new(
                WriteId::new(100),
                data_identity(0x11, 1, false),
            )),
        );
        assert!(matches!(
            ledger.prepare_registration(&request_with_w0),
            Err(OperationRegisterError::InvalidSpec(
                super::OperationSpecError::OutboundKindMismatch {
                    expected: crate::hsms::contracts::OutboundOperationKind::DataPrimaryW1,
                    actual: crate::hsms::contracts::OutboundOperationKind::DataPrimaryW0,
                }
            ))
        ));

        let send_with_w1 = OperationSpec::new(
            OperationId::new(2),
            OperationOwner::Command(CommandCompletionAuthority::for_test(CommandId::new(20))),
            OperationPurpose::Send,
            OperationScope::SelectedSessionData,
            Some(OperationClass::OneWay(OneWayKind::Data)),
            Some(ActiveOperationWrite::new(
                WriteId::new(200),
                data_identity(0x22, 1, true),
            )),
        );
        assert!(matches!(
            ledger.prepare_registration(&send_with_w1),
            Err(OperationRegisterError::InvalidSpec(
                super::OperationSpecError::OutboundKindMismatch {
                    expected: crate::hsms::contracts::OutboundOperationKind::DataPrimaryW0,
                    actual: crate::hsms::contracts::OutboundOperationKind::DataPrimaryW1,
                }
            ))
        ));

        let secondary_with_w1 = OperationSpec::new(
            OperationId::new(4),
            OperationOwner::Command(CommandCompletionAuthority::for_test(CommandId::new(40))),
            OperationPurpose::Reply,
            OperationScope::SelectedSessionData,
            None,
            Some(ActiveOperationWrite::new(
                WriteId::new(400),
                data_identity(0x44, 2, true),
            )),
        );
        assert!(matches!(
            ledger.prepare_registration(&secondary_with_w1),
            Err(OperationRegisterError::InvalidSpec(
                super::OperationSpecError::OutboundKindMismatch {
                    expected: crate::hsms::contracts::OutboundOperationKind::DataSecondaryW0,
                    actual: crate::hsms::contracts::OutboundOperationKind::DataSecondaryW1,
                }
            ))
        ));

        let deselect_message = ProtocolMessage::Control(ControlMessage::DeselectRequest {
            session_id: 3,
            system_bytes: SystemBytes::new(0x33),
        });
        let select_with_deselect_stype = OperationSpec::new(
            OperationId::new(3),
            OperationOwner::Autonomous,
            OperationPurpose::Control(ControlKind::Select),
            OperationScope::GenerationControl,
            Some(OperationClass::Control(ControlKind::Select)),
            Some(ActiveOperationWrite::new(
                WriteId::new(300),
                OutboundHeaderIdentity::from_protocol_message(&deselect_message)
                    .expect("valid outbound message shape"),
            )),
        );
        assert!(matches!(
            ledger.prepare_registration(&select_with_deselect_stype),
            Err(OperationRegisterError::InvalidSpec(
                super::OperationSpecError::OutboundKindMismatch {
                    expected: crate::hsms::contracts::OutboundOperationKind::SelectRequest,
                    actual: crate::hsms::contracts::OutboundOperationKind::DeselectRequest,
                }
            ))
        ));
        assert!(ledger.is_empty());
        assert_eq!(ledger.command_owner_len(), 0);
    }

    /// Rejects a body-bearing SxF0 before it can mutate any operation-ledger index.
    #[test]
    fn body_bearing_abort_cannot_reach_operation_registration() {
        let ledger = ledger(1, 1);
        let message = ProtocolMessage::Data(DataMessage::new(
            DataHeader::new(
                SessionId::new(3).expect("non-control session"),
                Stream::new(1).expect("seven-bit stream"),
                Function::new(0),
                false,
                SystemBytes::new(0x55),
            ),
            Some(SecsItem::Binary(vec![1])),
        ));

        assert_eq!(
            OutboundHeaderIdentity::from_protocol_message(&message),
            Err(OutboundMessageShapeError::AbortCarriesBody)
        );
        assert!(ledger.is_empty());
        assert_eq!(ledger.command_owner_len(), 0);
        assert_eq!(ledger.correlation_history_len(), 0);
    }

    /// Confirms a terminal resource shell still consumes logical operation
    /// capacity until both exact resource axes are released.
    #[test]
    fn terminal_shell_holds_capacity_until_exact_cleanup() {
        let mut ledger = ledger(1, 2);
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        assert_eq!(
            claimed_command(
                ledger.claim_terminal(OperationId::new(1), OperationClaimCause::ResponseMatched,)
            ),
            CommandId::new(10)
        );
        let second = request_spec(2, 20, 200, 0x22);
        assert!(matches!(
            ledger.prepare_registration(&second),
            Err(OperationRegisterError::CapacityExhausted { capacity: 1 })
        ));

        assert!(matches!(
            ledger.release_registry_binding(OperationId::new(1), OperationClass::Request,),
            RegistryReleaseDecision::Released {
                retention: OperationRetention::Retained {
                    terminal: true,
                    active_write: Some(_),
                    ..
                }
            }
        ));
        assert!(matches!(
            ledger.finish_active_write(OperationId::new(1), WriteId::new(100)),
            ActiveWriteReleaseDecision::Released {
                retention: OperationRetention::Retired
            }
        ));
        assert!(ledger.is_empty());
        assert!(ledger.prepare_registration(&second).is_ok());
    }

    /// Confirms a fast response completes its command immediately and every
    /// later write outcome only clears the exact resource shell.
    #[test]
    fn fast_response_wins_before_write_terminal() {
        let mut ledger = ledger(2, 2);
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        assert_eq!(
            ledger.mark_may_be_visible(OperationId::new(1), WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );
        assert!(matches!(
            ledger.release_registry_binding(OperationId::new(1), OperationClass::Request,),
            RegistryReleaseDecision::Released { .. }
        ));
        let decision =
            ledger.claim_terminal(OperationId::new(1), OperationClaimCause::ResponseMatched);
        assert_eq!(claimed_command(decision), CommandId::new(10));
        let snapshot = ledger
            .operation_snapshot(OperationId::new(1))
            .expect("fast-response resource shell");
        assert_eq!(snapshot.operation_id(), OperationId::new(1));
        assert_eq!(snapshot.purpose(), OperationPurpose::Request);
        assert_eq!(snapshot.scope(), OperationScope::SelectedSessionData);
        assert_eq!(snapshot.registry_binding(), None);
        assert_eq!(snapshot.active_write(), Some(WriteId::new(100)));
        assert_eq!(
            snapshot.terminal_cause(),
            Some(OperationTerminalCause::ResponseMatched)
        );

        assert!(matches!(
            ledger.claim_terminal(
                OperationId::new(1),
                OperationClaimCause::DeliveryIndeterminate,
            ),
            TerminalClaimDecision::AlreadyTerminal {
                cause: OperationTerminalCause::ResponseMatched,
            }
        ));
        assert!(matches!(
            ledger.finish_active_write(OperationId::new(1), WriteId::new(999)),
            ActiveWriteReleaseDecision::WrongWrite {
                expected,
                actual
            } if expected == WriteId::new(100) && actual == WriteId::new(999)
        ));
        assert_eq!(ledger.len(), 1);
        assert!(matches!(
            ledger.finish_active_write(OperationId::new(1), WriteId::new(100)),
            ActiveWriteReleaseDecision::Released {
                retention: OperationRetention::Retired
            }
        ));
        assert!(ledger.is_empty());
    }

    /// Confirms write completion does not discard a still-live request's
    /// outbound identity, allowing a later unique peer Reject to match it.
    #[test]
    fn pending_request_keeps_correlation_after_write_cleanup() {
        let mut ledger = ledger(2, 2);
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        assert_eq!(
            ledger.mark_may_be_visible(OperationId::new(1), WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );
        assert!(matches!(
            ledger.finish_active_write(OperationId::new(1), WriteId::new(100)),
            ActiveWriteReleaseDecision::Released {
                retention: OperationRetention::Retained {
                    terminal: false,
                    active_write: None,
                    ..
                }
            }
        ));
        assert!(matches!(
            ledger.discover_peer_reject(reject(0x11)),
            RejectDiscoveryDecision::Live(_)
        ));
    }

    /// Confirms an unproceeded identity never participates in Reject
    /// attribution and creates no terminal diagnostic history.
    #[test]
    fn before_proceed_is_excluded_and_not_tombstoned() {
        let mut ledger = ledger(2, 2);
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        assert!(matches!(
            ledger.discover_peer_reject(reject(0x11)),
            RejectDiscoveryDecision::Unknown
        ));
        let decision = ledger.claim_terminal(OperationId::new(1), OperationClaimCause::WriteFailed);
        assert!(matches!(
            decision,
            TerminalClaimDecision::Claimed {
                correlation: TerminalCorrelationRetention::DiscardedBeforeProceed,
                ..
            }
        ));
        assert_eq!(ledger.correlation_history_len(), 0);
    }

    /// Confirms gated Reject commit consumes an exact Registry release proof
    /// and emits the original move-only command completion authority.
    #[test]
    fn reject_commit_consumes_exact_registry_release_proof() {
        let mut ledger = ledger(2, 2);
        let mut authority = PeerRejectMutationAuthority::for_test();
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        assert_eq!(
            ledger.mark_may_be_visible(OperationId::new(1), WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );
        let token = match ledger.discover_peer_reject(reject(0x11)) {
            RejectDiscoveryDecision::Live(token) => token,
            other => panic!("expected live Reject token, got {other:?}"),
        };
        assert_eq!(
            token.expected_registry_binding(),
            Some(OperationClass::Request)
        );
        let validated = match ledger.validate_peer_reject_commit(&authority, token) {
            RejectValidationDecision::Validated(validated) => validated,
            other => panic!("expected validated Reject token, got {other:?}"),
        };
        let committed = ledger.commit_peer_reject(
            &mut authority,
            validated,
            Some(PeerRejectRegistryRelease::for_test(
                OperationId::new(1),
                OperationClass::Request,
            )),
        );
        let (target, _, correlation) = committed.into_parts();
        assert!(matches!(
            correlation,
            TerminalCorrelationRetention::RetainedHistory { .. }
        ));
        match target {
            super::CompletionTarget::Command(authority) => {
                assert_eq!(authority.command_id(), CommandId::new(10));
                assert_eq!(
                    authority
                        .failed(OperationError::RuntimeStopped)
                        .command_id(),
                    CommandId::new(10)
                );
            }
            other => panic!("expected command completion authority, got {other:?}"),
        }
        assert!(matches!(
            ledger.discover_peer_reject(reject(0x11)),
            RejectDiscoveryDecision::Duplicate
        ));
    }

    /// Confirms commit repeats global uniqueness validation, so a candidate
    /// added after discovery cannot be selected using a stale token.
    #[test]
    fn reject_commit_revalidates_global_uniqueness() {
        let mut ledger = ledger(3, 3);
        let authority = PeerRejectMutationAuthority::for_test();
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        assert_eq!(
            ledger.mark_may_be_visible(OperationId::new(1), WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );
        let token = match ledger.discover_peer_reject(reject(0x11)) {
            RejectDiscoveryDecision::Live(token) => token,
            other => panic!("expected live Reject token, got {other:?}"),
        };

        register(&mut ledger, request_spec(2, 20, 200, 0x11));
        assert_eq!(
            ledger.mark_may_be_visible(OperationId::new(2), WriteId::new(200)),
            OperationVisibilityDecision::Marked
        );
        assert!(matches!(
            ledger.validate_peer_reject_commit(&authority, token),
            RejectValidationDecision::InvariantViolation {
                validation: super::RejectTokenInvalidity::NoLongerUnique,
            }
        ));
        assert_eq!(
            claimed_command(
                ledger.claim_terminal(OperationId::new(1), OperationClaimCause::ResponseMatched,)
            ),
            CommandId::new(10)
        );
    }

    /// Confirms live and terminal matches share one global uniqueness domain.
    #[test]
    fn live_plus_terminal_reject_match_is_ambiguous() {
        let mut ledger = ledger(3, 3);
        register(&mut ledger, send_spec(1, 10, 100, 0x11));
        assert_eq!(
            ledger.mark_may_be_visible(OperationId::new(1), WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );
        assert_eq!(
            claimed_command(
                ledger.claim_terminal(OperationId::new(1), OperationClaimCause::Completed,),
            ),
            CommandId::new(10)
        );

        register(&mut ledger, request_spec(2, 20, 200, 0x11));
        assert_eq!(
            ledger.mark_may_be_visible(OperationId::new(2), WriteId::new(200)),
            OperationVisibilityDecision::Marked
        );
        assert!(matches!(
            ledger.discover_peer_reject(reject(0x11)),
            RejectDiscoveryDecision::Ambiguous {
                live_matches: 1,
                terminal_matches: 1,
            }
        ));
    }

    /// Confirms terminal Reject diagnostics distinguish late, duplicate, and
    /// conflicting references without mutating operation state.
    #[test]
    fn terminal_reject_diagnostics_are_precise() {
        let mut late = ledger(2, 2);
        register(&mut late, request_spec(1, 10, 100, 0x11));
        assert_eq!(
            late.mark_may_be_visible(OperationId::new(1), WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );
        assert_eq!(
            claimed_command(
                late.claim_terminal(OperationId::new(1), OperationClaimCause::ResponseMatched,)
            ),
            CommandId::new(10)
        );
        assert!(matches!(
            late.discover_peer_reject(reject(0x11)),
            RejectDiscoveryDecision::Late
        ));

        let mut rejected = ledger(2, 2);
        let mut authority = PeerRejectMutationAuthority::for_test();
        register(&mut rejected, request_spec(1, 10, 100, 0x11));
        assert_eq!(
            rejected.mark_may_be_visible(OperationId::new(1), WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );
        let token = match rejected.discover_peer_reject(reject(0x11)) {
            RejectDiscoveryDecision::Live(token) => token,
            other => panic!("expected live Reject token, got {other:?}"),
        };
        let validated = match rejected.validate_peer_reject_commit(&authority, token) {
            RejectValidationDecision::Validated(validated) => validated,
            other => panic!("expected validated Reject token, got {other:?}"),
        };
        let committed = rejected.commit_peer_reject(
            &mut authority,
            validated,
            Some(PeerRejectRegistryRelease::for_test(
                OperationId::new(1),
                OperationClass::Request,
            )),
        );
        match committed.into_parts().0 {
            super::CompletionTarget::Command(authority) => {
                assert_eq!(authority.command_id(), CommandId::new(10));
                drop(authority.failed(OperationError::RuntimeStopped));
            }
            other => panic!("expected command completion authority, got {other:?}"),
        }
        assert!(matches!(
            rejected.discover_peer_reject(reject(0x11)),
            RejectDiscoveryDecision::Duplicate
        ));
        let conflicting = RejectReference::new(
            GENERATION,
            3,
            0,
            RejectReason::UNSUPPORTED_STYPE,
            SystemBytes::new(0x11),
        );
        assert!(matches!(
            rejected.discover_peer_reject(conflicting),
            RejectDiscoveryDecision::Conflicting
        ));
    }

    /// Confirms extension and stale-generation Rejects are classified before
    /// candidate scanning and leave every operation unchanged.
    #[test]
    fn unsupported_extension_and_wrong_generation_are_non_mutating() {
        let mut ledger = ledger(2, 2);
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        assert_eq!(
            ledger.mark_may_be_visible(OperationId::new(1), WriteId::new(100)),
            OperationVisibilityDecision::Marked
        );
        let extension = RejectReference::new(
            GENERATION,
            3,
            0,
            RejectReason::new(0x80).expect("non-zero extension"),
            SystemBytes::new(0x11),
        );
        assert!(matches!(
            ledger.discover_peer_reject(extension),
            RejectDiscoveryDecision::UnsupportedExtension
        ));
        let stale = RejectReference::new(
            ConnectionGeneration::new(8),
            3,
            0,
            RejectReason::UNSUPPORTED_PTYPE,
            SystemBytes::new(0x11),
        );
        assert!(matches!(
            ledger.discover_peer_reject(stale),
            RejectDiscoveryDecision::WrongGeneration { .. }
        ));
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms the independent terminal FIFO evicts diagnostics only and
    /// leaves live operation ownership intact.
    #[test]
    fn terminal_history_fifo_is_bounded_independently() {
        let mut ledger = ledger(4, 1);
        for (operation, command, write, system_bytes) in [(1, 10, 100, 0x11), (2, 20, 200, 0x22)] {
            register(
                &mut ledger,
                send_spec(operation, command, write, system_bytes),
            );
            assert_eq!(
                ledger.mark_may_be_visible(OperationId::new(operation), WriteId::new(write),),
                OperationVisibilityDecision::Marked
            );
            assert_eq!(
                claimed_command(
                    ledger.claim_terminal(
                        OperationId::new(operation),
                        OperationClaimCause::Completed,
                    )
                ),
                CommandId::new(command)
            );
        }
        assert_eq!(ledger.correlation_history_len(), 1);
        assert!(matches!(
            ledger.discover_peer_reject(reject(0x11)),
            RejectDiscoveryDecision::Unknown
        ));
        assert!(matches!(
            ledger.discover_peer_reject(reject(0x22)),
            RejectDiscoveryDecision::Late
        ));
        assert_eq!(ledger.len(), 2);
    }

    /// Confirms session reset affects Data work once, preserves control work,
    /// and returns stable operation-id ordering.
    #[test]
    fn session_reset_is_scoped_stable_and_idempotent() {
        let mut ledger = ledger(4, 4);
        register(&mut ledger, request_spec(2, 20, 200, 0x22));
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        register(&mut ledger, autonomous_control_spec(3, 300, 0x33));

        let reset = ledger.reset_selected_session();
        let ids: Vec<_> = reset
            .operations()
            .iter()
            .map(|operation| operation.operation_id())
            .collect();
        assert_eq!(ids, [OperationId::new(1), OperationId::new(2)]);
        assert!(reset
            .operations()
            .iter()
            .all(|operation| matches!(operation.terminal(), LifecycleTerminalStatus::Claimed(_))));
        assert!(ledger.reset_selected_session().is_empty());
        assert_eq!(ledger.len(), 3);
        assert!(matches!(
            ledger.claim_terminal(OperationId::new(3), OperationClaimCause::Completed,),
            TerminalClaimDecision::Claimed {
                target: super::CompletionTarget::Autonomous,
                ..
            }
        ));
    }

    /// Confirms close fences admission, claims each live owner once in stable
    /// order, and keeps exact resource shells for later cleanup.
    #[test]
    fn close_is_stable_first_terminal_and_idempotent() {
        let mut ledger = ledger(3, 3);
        register(&mut ledger, request_spec(2, 20, 200, 0x22));
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        let close = ledger.begin_close();
        assert!(close.began_close());
        assert_eq!(
            close
                .operations()
                .iter()
                .map(|operation| operation.operation_id())
                .collect::<Vec<_>>(),
            [OperationId::new(1), OperationId::new(2)]
        );
        assert!(close
            .operations()
            .iter()
            .all(|operation| matches!(operation.terminal(), LifecycleTerminalStatus::Claimed(_))));
        assert_eq!(ledger.len(), 2);
        let rejected = request_spec(3, 30, 300, 0x33);
        assert!(matches!(
            ledger.prepare_registration(&rejected),
            Err(OperationRegisterError::Closing)
        ));
        let duplicate = ledger.begin_close();
        assert!(!duplicate.began_close());
        assert!(duplicate.operations().is_empty());
        assert!(matches!(
            ledger.claim_terminal(OperationId::new(1), OperationClaimCause::ResponseMatched,),
            TerminalClaimDecision::AlreadyTerminal {
                cause: OperationTerminalCause::GenerationClosing
            }
        ));
    }

    /// Confirms incoherent owner, scope, Registry, and outbound-kind
    /// specifications fail without occupying ledger capacity.
    #[test]
    fn invalid_specifications_are_atomic() {
        let ledger = ledger(2, 2);
        let invalid = OperationSpec::new(
            OperationId::new(1),
            OperationOwner::Autonomous,
            OperationPurpose::Request,
            OperationScope::GenerationControl,
            None,
            Some(ActiveOperationWrite::new(
                WriteId::new(1),
                data_identity(0x11, 2, false),
            )),
        );
        assert!(matches!(
            ledger.prepare_registration(&invalid),
            Err(OperationRegisterError::InvalidSpec(_))
        ));
        assert!(ledger.is_empty());
    }

    /// Confirms lifecycle decision containers can be consumed without losing
    /// exact operation resource identities.
    #[test]
    fn reset_decision_preserves_exact_resources_when_consumed() {
        let mut ledger = ledger(2, 2);
        register(&mut ledger, request_spec(1, 10, 100, 0x11));
        let reset: OperationSessionResetDecision = ledger.reset_selected_session();
        let mut operations = reset.into_operations();
        assert_eq!(operations[0].active_write(), Some(WriteId::new(100)));
        assert_eq!(
            operations[0].registry_binding(),
            Some(OperationClass::Request)
        );
        let (operation_id, write_id, registry_binding, terminal) =
            operations.remove(0).into_parts();
        assert_eq!(operation_id, OperationId::new(1));
        assert_eq!(write_id, Some(WriteId::new(100)));
        assert_eq!(registry_binding, Some(OperationClass::Request));
        match terminal {
            LifecycleTerminalStatus::Claimed(super::CompletionTarget::Command(authority)) => {
                assert_eq!(authority.command_id(), CommandId::new(10));
                drop(authority.failed(OperationError::RuntimeStopped));
            }
            other => panic!("expected claimed reset command, got {other:?}"),
        }
    }
}
