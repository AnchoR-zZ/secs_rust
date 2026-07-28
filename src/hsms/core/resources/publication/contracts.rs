//! Defines semantic values and failures exposed by the publication facade.
//!
//! Reply values freeze normal F+1 and header-only SxF0 semantics. Lifecycle
//! errors preserve useful generation, identity, capacity, closing, and
//! exhaustion diagnostics without exposing ledger or commit-token internals.

use crate::hsms::{
    model::ids::{ConnectionGeneration, DeliveryId, ReplyCapabilityId, SystemBytes},
    protocol::{header::DataHeader, message::DataMessage},
    Function, SessionId, Stream,
};
use crate::secs2::SecsItem;

/// Core-authoritative response forms available for one reply capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyCapabilityMode {
    /// Normal F+1 Secondary is available and SxF0 remains allowed.
    NormalSecondary {
        /// Exact even Secondary function derived without wrapping.
        function: Function,
    },
    /// F255 cannot form F+1, so only a header-only SxF0 may be written.
    AbortOnly,
}

/// Failure compiling reply authority from an inbound Data header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyContractError {
    /// The inbound Primary did not set W and therefore grants no reply authority.
    ReplyNotExpected,
    /// Function zero or an even function cannot be classified as a Primary.
    NotPrimaryFunction {
        /// Function rejected while compiling the contract.
        function: Function,
    },
}

/// Marker returned when a capability cannot form a normal F+1 Secondary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NormalSecondaryUnavailable;

/// Immutable Core authority for constructing responses to one W=1 Primary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplyContract {
    /// Connection generation that owns the capability.
    generation: ConnectionGeneration,
    /// Data Session ID copied exactly into every response form.
    session_id: SessionId,
    /// Stream copied exactly into every response form.
    stream: Stream,
    /// Core-authoritative normal-or-abort-only response mode.
    mode: ReplyCapabilityMode,
    /// Peer System Bytes copied exactly into every response form.
    system_bytes: SystemBytes,
}

impl ReplyContract {
    /// Compiles response authority from one validated semantic Data header.
    pub(crate) const fn from_w1_primary(
        generation: ConnectionGeneration,
        header: DataHeader,
    ) -> Result<Self, ReplyContractError> {
        Self::from_primary_parts(
            generation,
            header.session_id(),
            header.stream(),
            header.function(),
            header.reply_expected(),
            header.system_bytes(),
        )
    }

    /// Compiles response authority from validated inbound Data-header fields.
    ///
    /// `wait` must be true. Odd functions 1 through 253 derive exact F+1;
    /// F255 becomes abort-only. Zero and even functions are not Primaries.
    pub(crate) const fn from_primary_parts(
        generation: ConnectionGeneration,
        session_id: SessionId,
        stream: Stream,
        function: Function,
        wait: bool,
        system_bytes: SystemBytes,
    ) -> Result<Self, ReplyContractError> {
        if !wait {
            return Err(ReplyContractError::ReplyNotExpected);
        }
        let raw = function.get();
        if raw == 0 || raw.is_multiple_of(2) {
            return Err(ReplyContractError::NotPrimaryFunction { function });
        }
        let mode = if raw == u8::MAX {
            ReplyCapabilityMode::AbortOnly
        } else {
            ReplyCapabilityMode::NormalSecondary {
                function: Function::new(raw + 1),
            }
        };
        Ok(Self {
            generation,
            session_id,
            stream,
            mode,
            system_bytes,
        })
    }

    /// Returns the connection generation that owns this contract.
    pub(crate) const fn generation(self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the Data Session ID copied into a response.
    pub(crate) const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the stream copied into a response.
    pub(crate) const fn stream(self) -> Stream {
        self.stream
    }

    /// Returns the Core-authoritative response mode.
    pub(crate) const fn mode(self) -> ReplyCapabilityMode {
        self.mode
    }

    /// Returns the peer System Bytes copied into a response.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        self.system_bytes
    }

    /// Returns the non-authoritative availability hint placed in the public token.
    ///
    /// The reply ledger remains authoritative and validates the exact contract
    /// when Core later consumes the capability.
    pub(crate) const fn supports_normal_secondary(self) -> bool {
        matches!(self.mode, ReplyCapabilityMode::NormalSecondary { .. })
    }

    /// Returns the exact F+1 function or an abort-only marker.
    pub(crate) const fn normal_secondary_function(
        self,
    ) -> Result<Function, NormalSecondaryUnavailable> {
        match self.mode {
            ReplyCapabilityMode::NormalSecondary { function } => Ok(function),
            ReplyCapabilityMode::AbortOnly => Err(NormalSecondaryUnavailable),
        }
    }

    /// Builds the normal W=false F+1 Secondary with caller-owned Message Text.
    ///
    /// Abort-only F255 authority returns an error without constructing a frame.
    pub(crate) fn normal_secondary(
        self,
        body: Option<SecsItem>,
    ) -> Result<DataMessage, NormalSecondaryUnavailable> {
        let function = self.normal_secondary_function()?;
        Ok(DataMessage::new(
            DataHeader::new(
                self.session_id,
                self.stream,
                function,
                false,
                self.system_bytes,
            ),
            body,
        ))
    }

    /// Builds a header-only W=false SxF0 abort for either capability mode.
    pub(crate) const fn abort(self) -> DataMessage {
        DataMessage::new(
            DataHeader::new(
                self.session_id,
                self.stream,
                Function::new(0),
                false,
                self.system_bytes,
            ),
            None,
        )
    }
}

/// Application-selected terminal use of one available Reply capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyUseKind {
    /// Send the exact normal F+1 Secondary authorized by the contract.
    Normal,
    /// Send a header-only SxF0 transaction abort.
    Abort,
    /// Release the capability locally without writing a frame.
    Abandon,
}

/// Reason the publication facade could not prepare one Reply-token use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyUseUnavailable {
    /// The token was minted by a different publication-resource instance.
    ForeignIssuer,
    /// The token targets a different TCP generation.
    WrongGeneration {
        /// Generation owned by the publication resources.
        expected: ConnectionGeneration,
        /// Generation supplied with the token.
        actual: ConnectionGeneration,
    },
    /// The token is still inside a reliable application-publication attempt.
    PendingPublication,
    /// The token names an earlier reservation that reused the same public ID.
    IncarnationChanged {
        /// Reused identity whose private incarnation did not match.
        capability_id: ReplyCapabilityId,
    },
    /// The stored F255 contract permits only SxF0 abort or local abandon.
    NormalSecondaryUnavailable,
    /// The identity is unknown, already consumed, or already revoked.
    UnknownOrTerminal,
}

/// Successful terminal transition produced by committing one Reply use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "reply-use terminals determine completion and outbound-write behavior"]
pub(crate) enum ReplyUseTerminal {
    /// A valid normal, abort, or abandon action consumed the capability.
    Consumed {
        /// Authoritative contract removed from publication resources.
        contract: ReplyContract,
        /// Exact application action that consumed the authority.
        use_kind: ReplyUseKind,
    },
}

/// Failure committing a prepared Reply use after downstream preflight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyUseCommitError {
    /// The supplied token was minted by another publication-resource instance.
    ForeignIssuer,
    /// The supplied token belongs to a different TCP connection generation.
    TokenWrongGeneration {
        /// Generation owned by the publication resources.
        expected: ConnectionGeneration,
        /// Generation carried by the supplied token.
        actual: ConnectionGeneration,
    },
    /// The supplied token does not name the entry captured by the preparation.
    TokenPlanMismatch,
    /// The preparation belongs to another publication-resource instance.
    ForeignResources,
    /// The preparation was produced by a different TCP generation.
    WrongGeneration {
        /// Generation owned by the publication resources.
        expected: ConnectionGeneration,
        /// Generation captured by the stale preparation.
        actual: ConnectionGeneration,
    },
    /// The capability was consumed, revoked, reset, or never registered.
    UnknownOrTerminal {
        /// Identity that no longer names a live capability.
        capability_id: ReplyCapabilityId,
    },
    /// The capability still exists but publication has not made it available.
    PendingPublication {
        /// Identity whose use cannot commit before publication.
        capability_id: ReplyCapabilityId,
    },
    /// The immutable Reply contract changed after preparation.
    ContractChanged {
        /// Identity whose authoritative contract failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The public ID now names a later reservation.
    IncarnationChanged {
        /// Reused identity whose incarnation failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The requested use or derived outcome differs from the preparation.
    PlanChanged {
        /// Identity whose prepared decision failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
}

/// Logical publication resource named by capacity and exhaustion diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationResourceKind {
    /// Reliable application-delivery correlation.
    ApplicationDelivery,
    /// Pending or application-available Reply capability.
    ReplyCapability,
}

/// Cross-resource inconsistency detected without exposing child-ledger mechanics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationInvariantViolation {
    /// A value was issued by another publication-resource aggregate.
    OwnershipMismatch,
    /// Two values that must share one TCP generation disagreed.
    GenerationMismatch {
        /// Generation owned by the receiving publication resources.
        expected: ConnectionGeneration,
        /// Generation carried by the inconsistent value.
        actual: ConnectionGeneration,
    },
    /// One delivery changed after a read-only decision was prepared.
    DeliveryChanged {
        /// Delivery whose exact retained state no longer matched.
        delivery_id: DeliveryId,
    },
    /// The complete live delivery set changed during one synchronous use case.
    DeliverySetChanged,
    /// One Reply capability changed after a read-only decision was prepared.
    ReplyCapabilityChanged {
        /// Capability whose exact retained state no longer matched.
        capability_id: ReplyCapabilityId,
    },
    /// The complete pending Reply-publication set disagreed with Delivery state.
    ReplyCapabilitySetChanged {
        /// Capability identifying the mismatch, when one was available.
        capability_id: Option<ReplyCapabilityId>,
    },
    /// The open-versus-closing lifecycle changed during one use case.
    LifecycleChanged {
        /// Closing state captured before the mutation boundary.
        expected_closing: bool,
        /// Closing state observed at the mutation boundary.
        actual_closing: bool,
    },
    /// Reset-versus-close intent did not match one same-call lifecycle value.
    LifecycleScopeMismatch,
    /// A closed resource unexpectedly retained live entries.
    ClosedWithRetainedResources {
        /// Resource kind that retained impossible live state.
        resource: PublicationResourceKind,
        /// Number of entries retained behind the close fence.
        count: usize,
    },
}

/// Failure constructing one generation-scoped publication subaggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationResourcesBuildError {
    /// A logical publication capacity was configured as zero.
    ZeroCapacity {
        /// Resource kind whose capacity must be non-zero.
        resource: PublicationResourceKind,
    },
    /// Child construction exposed an impossible ownership or generation mismatch.
    InvariantViolation {
        /// Public-safe description of the inconsistent construction state.
        violation: PublicationInvariantViolation,
    },
}

/// Failure admitting one reliable application publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationAdmissionError {
    /// Generation close permanently fenced later publications.
    Closing,
    /// A supplied publication value targets a different TCP generation.
    WrongGeneration {
        /// Generation owned by the publication resources.
        expected: ConnectionGeneration,
        /// Generation carried by the supplied value.
        actual: ConnectionGeneration,
    },
    /// The delivery identity already names a live publication.
    DuplicateDelivery {
        /// Live delivery identity that cannot be admitted twice.
        delivery_id: DeliveryId,
    },
    /// The delivery identity is stale, reordered, or previously used.
    NonMonotonicOrReusedDelivery {
        /// Greatest delivery identity ever admitted by this generation.
        highest_registered_id: DeliveryId,
        /// Stale, reordered, or reused identity that was rejected.
        attempted_id: DeliveryId,
    },
    /// The Reply capability identity already names live authority.
    DuplicateReplyCapability {
        /// Active capability identity that cannot be reserved twice.
        capability_id: ReplyCapabilityId,
    },
    /// One logical publication capacity is currently occupied.
    CapacityExhausted {
        /// Resource kind whose configured bound is full.
        resource: PublicationResourceKind,
        /// Maximum number of simultaneously live entries.
        capacity: usize,
    },
    /// A never-reused internal identity space is permanently exhausted.
    IdentityExhausted {
        /// Resource kind whose monotonic identity space ended.
        resource: PublicationResourceKind,
    },
    /// The aggregate changed between read-only admission and exact commit.
    AdmissionStateChanged {
        /// Delivery identity whose frozen admission conditions changed.
        delivery_id: DeliveryId,
    },
    /// A child detected an impossible same-aggregate inconsistency.
    InvariantViolation {
        /// Public-safe description of the inconsistent state.
        violation: PublicationInvariantViolation,
    },
}

/// Failure completing one reliable application publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationFinishError {
    /// The completion targets a different TCP generation.
    WrongGeneration {
        /// Generation owned by the publication resources.
        expected: ConnectionGeneration,
        /// Generation supplied with the completion.
        actual: ConnectionGeneration,
    },
    /// The delivery is unknown, already terminal, reset, or closed.
    UnknownOrTerminalDelivery {
        /// Delivery identity that no longer names a live publication.
        delivery_id: DeliveryId,
    },
    /// Reply authority changed before the matching publication could finish.
    ReplyAuthorityChanged {
        /// Capability whose publication or revocation could not be completed.
        capability_id: ReplyCapabilityId,
    },
    /// The aggregate detected an impossible callback-free inconsistency.
    InvariantViolation {
        /// Public-safe description of the inconsistent state.
        violation: PublicationInvariantViolation,
    },
}

/// Failure clearing Selected-session publication state before any mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationResetError {
    /// Generation close already fenced Selected-session reset.
    Closing,
    /// Delivery and Reply state failed one atomic reset invariant.
    InvariantViolation {
        /// Public-safe description of the inconsistent state.
        violation: PublicationInvariantViolation,
    },
}

/// Failure permanently closing generation publication state before any mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationCloseError {
    /// Delivery and Reply state failed one atomic close invariant.
    InvariantViolation {
        /// Public-safe description of the inconsistent state.
        violation: PublicationInvariantViolation,
    },
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        model::ids::{ConnectionGeneration, SystemBytes},
        protocol::header::DataHeader,
        Function, SessionId, Stream,
    };

    use super::{
        NormalSecondaryUnavailable, ReplyCapabilityMode, ReplyContract, ReplyContractError,
    };

    /// Compiles a deterministic W=1 contract for the supplied function.
    fn compile(function: u8) -> Result<ReplyContract, ReplyContractError> {
        ReplyContract::from_primary_parts(
            ConnectionGeneration::new(3),
            SessionId::new(7).expect("valid Data Session ID"),
            Stream::new(5).expect("valid stream"),
            Function::new(function),
            true,
            SystemBytes::new(0x0102_0304),
        )
    }

    /// Confirms the supported odd range derives exact F+1 without wrapping.
    #[test]
    fn normal_primary_functions_compile_exact_secondary_function() {
        for (primary, secondary) in [(1, 2), (253, 254)] {
            let contract = compile(primary).expect("normal W=1 Primary");

            assert_eq!(
                contract.mode(),
                ReplyCapabilityMode::NormalSecondary {
                    function: Function::new(secondary)
                }
            );
            assert_eq!(
                contract.normal_secondary_function(),
                Ok(Function::new(secondary))
            );
            assert!(contract.supports_normal_secondary());
        }
    }

    /// Confirms F255 is modeled as abort-only instead of wrapping F+1 to F0.
    #[test]
    fn function_255_compiles_abort_only_authority() {
        let contract = compile(255).expect("F255 W=1 Primary");

        assert_eq!(contract.mode(), ReplyCapabilityMode::AbortOnly);
        assert!(!contract.supports_normal_secondary());
        assert_eq!(
            contract.normal_secondary_function(),
            Err(NormalSecondaryUnavailable)
        );
    }

    /// Confirms absent W, F0, and even functions cannot create reply authority.
    #[test]
    fn non_w1_or_non_primary_headers_are_rejected() {
        let no_wait = ReplyContract::from_primary_parts(
            ConnectionGeneration::new(3),
            SessionId::new(7).expect("valid Data Session ID"),
            Stream::new(5).expect("valid stream"),
            Function::new(1),
            false,
            SystemBytes::new(1),
        );
        assert_eq!(no_wait, Err(ReplyContractError::ReplyNotExpected));

        for function in [0, 2, 254] {
            assert_eq!(
                compile(function),
                Err(ReplyContractError::NotPrimaryFunction {
                    function: Function::new(function)
                })
            );
        }
    }

    /// Confirms every private header field is retained only in the Core contract.
    #[test]
    fn reply_contract_preserves_private_header_correlation() {
        let contract = compile(1).expect("normal W=1 Primary");

        assert_eq!(contract.generation(), ConnectionGeneration::new(3));
        assert_eq!(
            contract.session_id(),
            SessionId::new(7).expect("valid Data Session ID")
        );
        assert_eq!(contract.stream(), Stream::new(5).expect("valid stream"));
        assert_eq!(contract.system_bytes(), SystemBytes::new(0x0102_0304));
    }

    /// Confirms normal and abort frames copy correlation, always clear W, and
    /// keep SxF0 strictly header-only.
    #[test]
    fn response_frames_preserve_header_contract_without_wrapping() {
        let contract = compile(1).expect("normal W=1 Primary");
        let normal = contract
            .normal_secondary(None)
            .expect("normal Secondary available");
        let normal_header = normal.header();
        assert_eq!(normal_header.session_id(), contract.session_id());
        assert_eq!(normal_header.stream(), contract.stream());
        assert_eq!(normal_header.function(), Function::new(2));
        assert!(!normal_header.reply_expected());
        assert_eq!(normal_header.system_bytes(), contract.system_bytes());
        assert!(normal.body().is_none());

        for primary in [1, 255] {
            let contract = compile(primary).expect("odd W=1 Primary");
            let abort = contract.abort();
            let abort_header = abort.header();
            assert_eq!(abort_header.session_id(), contract.session_id());
            assert_eq!(abort_header.stream(), contract.stream());
            assert_eq!(abort_header.function(), Function::new(0));
            assert!(!abort_header.reply_expected());
            assert_eq!(abort_header.system_bytes(), contract.system_bytes());
            assert!(abort.body().is_none());
        }
    }

    /// Confirms the semantic DataHeader entry point preserves the same
    /// abort-only classification as the primitive contract compiler.
    #[test]
    fn data_header_entry_point_classifies_f255_abort_only() {
        let header = DataHeader::new(
            SessionId::new(7).expect("valid Data Session ID"),
            Stream::new(5).expect("valid stream"),
            Function::new(255),
            true,
            SystemBytes::new(0x0102_0304),
        );
        let contract = ReplyContract::from_w1_primary(ConnectionGeneration::new(3), header)
            .expect("F255 W=1 Primary");

        assert_eq!(contract.mode(), ReplyCapabilityMode::AbortOnly);
    }
}
