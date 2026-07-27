//! Defines shared values that coordinate generation-scoped Core resources.
//!
//! These values carry ownership, delivery, and Reject-attribution facts across
//! pure ledgers. They contain no task, socket, channel, clock, or side effect.

use crate::hsms::{
    model::ids::ConnectionGeneration,
    model::ids::{CommandId, ReplyCapabilityId, SystemBytes},
    protocol::{
        header::{ControlMessage, DataHeader, RejectReason},
        message::{DataMessage, ProtocolMessage},
    },
};

use super::completion::CommandCompletionAuthority;

/// Whether one Core operation belongs to an application command.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "an operation owner must be retained as part of its operation specification"]
pub(crate) enum OperationOwner {
    /// Accepted application command that must complete exactly once.
    Command(
        /// Move-only authority transferred from command admission.
        CommandCompletionAuthority,
    ),
    /// Protocol work initiated internally without an application completion.
    Autonomous,
}

impl OperationOwner {
    /// Returns the command identity used for uniqueness indexing, if present.
    pub(crate) const fn command_id(&self) -> Option<CommandId> {
        match self {
            Self::Command(authority) => Some(authority.command_id()),
            Self::Autonomous => None,
        }
    }
}

/// Why one reliable application publication must be correlated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DeliveryPurpose {
    /// Inbound W=0 Primary carrying no reply authority.
    InboundPrimary,
    /// Inbound W=1 Primary whose token names this capability.
    InboundReplyCapability(
        /// Capability made available only after successful publication.
        ReplyCapabilityId,
    ),
    /// Non-data protocol diagnostic.
    ProtocolNotice,
}

/// Semantic role of one outbound HSMS header during Reject attribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum OutboundRole {
    /// W=0 or W=1 Data Primary sent by the local application.
    DataPrimary,
    /// Data Secondary or SxF0 sent for a peer Primary.
    DataResponse,
    /// Locally initiated transactional or one-way control request.
    ControlRequest,
    /// Mandatory response to a peer control request.
    ControlResponse,
    /// Locally generated `Reject.req`.
    RejectRequest,
}

/// Exact typed outbound message form used by operation admission.
///
/// This classification is derived exhaustively from [`ProtocolMessage`].
/// Operation code cannot construct it independently from coarse role fields,
/// preventing W-bit or control-request kind mismatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum OutboundOperationKind {
    /// Odd-function Data Primary with W=false.
    DataPrimaryW0,
    /// Odd-function Data Primary with W=true.
    DataPrimaryW1,
    /// Non-zero even-function Data Secondary with the required W=false bit.
    DataSecondaryW0,
    /// Non-zero even-function Data shape carrying an invalid W=true bit.
    DataSecondaryW1,
    /// Header-only semantic SxF0 Data abort response with W=false.
    DataAbortW0,
    /// Header-only SxF0 Data shape carrying an invalid W=true bit.
    DataAbortW1,
    /// `Select.req` transactional control request.
    SelectRequest,
    /// `Select.rsp` control response.
    SelectResponse,
    /// `Deselect.req` transactional control request.
    DeselectRequest,
    /// `Deselect.rsp` control response.
    DeselectResponse,
    /// `Linktest.req` transactional control request.
    LinktestRequest,
    /// `Linktest.rsp` control response.
    LinktestResponse,
    /// Locally generated `Reject.req`.
    RejectRequest,
    /// One-way `Separate.req`.
    SeparateRequest,
}

impl OutboundOperationKind {
    /// Returns the coarse Reject-compatibility role derived from this exact kind.
    pub(crate) const fn role(self) -> OutboundRole {
        match self {
            Self::DataPrimaryW0 | Self::DataPrimaryW1 => OutboundRole::DataPrimary,
            Self::DataSecondaryW0
            | Self::DataSecondaryW1
            | Self::DataAbortW0
            | Self::DataAbortW1 => OutboundRole::DataResponse,
            Self::SelectRequest
            | Self::DeselectRequest
            | Self::LinktestRequest
            | Self::SeparateRequest => OutboundRole::ControlRequest,
            Self::SelectResponse | Self::DeselectResponse | Self::LinktestResponse => {
                OutboundRole::ControlResponse
            }
            Self::RejectRequest => OutboundRole::RejectRequest,
        }
    }
}

/// Complete immutable header identity required for peer Reject attribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct OutboundHeaderIdentity {
    /// Raw two-byte Session ID written into the outbound header.
    session_id: u16,
    /// Raw Presentation Type written into the outbound header.
    p_type: u8,
    /// Raw Session Type written into the outbound header.
    s_type: u8,
    /// Four-byte outbound correlation copied into the header.
    system_bytes: SystemBytes,
    /// Exact outbound message kind derived from the typed protocol value.
    kind: OutboundOperationKind,
}

/// Invalid semantic shape found while classifying a complete outbound message.
#[must_use = "an outbound message-shape error must be handled"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutboundMessageShapeError {
    /// Function-zero Data messages must not carry SECS-II Message Text.
    AbortCarriesBody,
}

impl OutboundHeaderIdentity {
    /// Derives the complete outbound identity from one typed protocol message.
    ///
    /// The caller cannot independently supply raw PType, SType, or semantic
    /// role values. Data messages use the HSMS-SS fixed zero PType/SType and
    /// derive Primary versus response role from the SECS function. Every
    /// current control variant maps exhaustively to its fixed E37 SType and
    /// request, response, or Reject role.
    ///
    /// # Errors
    ///
    /// Returns [`OutboundMessageShapeError::AbortCarriesBody`] when an SxF0
    /// Data message incorrectly carries SECS-II Message Text.
    pub(crate) fn from_protocol_message(
        message: &ProtocolMessage,
    ) -> Result<Self, OutboundMessageShapeError> {
        match message {
            ProtocolMessage::Data(message) => Self::from_data_message(message),
            ProtocolMessage::Control(message) => Ok(Self::from_control_message(*message)),
        }
    }

    /// Derives an outbound identity only from a semantically valid complete Data message.
    ///
    /// `message` supplies both the fixed header identity and Message Text presence.
    /// Returns a structured error instead of classifying an invalid body-bearing
    /// SxF0 message as a valid transaction Abort.
    fn from_data_message(message: &DataMessage) -> Result<Self, OutboundMessageShapeError> {
        if message.header().function().get() == 0 && message.body().is_some() {
            return Err(OutboundMessageShapeError::AbortCarriesBody);
        }
        Ok(Self::from_data_header(message.header()))
    }

    /// Derives the fixed HSMS-SS identity and exact operation kind of a Data header.
    const fn from_data_header(header: DataHeader) -> Self {
        let function = header.function().get();
        let kind = if function % 2 == 1 {
            if header.reply_expected() {
                OutboundOperationKind::DataPrimaryW1
            } else {
                OutboundOperationKind::DataPrimaryW0
            }
        } else if function == 0 {
            if header.reply_expected() {
                OutboundOperationKind::DataAbortW1
            } else {
                OutboundOperationKind::DataAbortW0
            }
        } else if header.reply_expected() {
            OutboundOperationKind::DataSecondaryW1
        } else {
            OutboundOperationKind::DataSecondaryW0
        };

        Self {
            session_id: header.session_id().get(),
            p_type: 0,
            s_type: 0,
            system_bytes: header.system_bytes(),
            kind,
        }
    }

    /// Derives fixed wire identity and semantic role from a typed control value.
    const fn from_control_message(message: ControlMessage) -> Self {
        match message {
            ControlMessage::SelectRequest {
                session_id,
                system_bytes,
            } => Self {
                session_id,
                p_type: 0,
                s_type: 1,
                system_bytes,
                kind: OutboundOperationKind::SelectRequest,
            },
            ControlMessage::SelectResponse {
                session_id,
                system_bytes,
                ..
            } => Self {
                session_id,
                p_type: 0,
                s_type: 2,
                system_bytes,
                kind: OutboundOperationKind::SelectResponse,
            },
            ControlMessage::DeselectRequest {
                session_id,
                system_bytes,
            } => Self {
                session_id,
                p_type: 0,
                s_type: 3,
                system_bytes,
                kind: OutboundOperationKind::DeselectRequest,
            },
            ControlMessage::DeselectResponse {
                session_id,
                system_bytes,
                ..
            } => Self {
                session_id,
                p_type: 0,
                s_type: 4,
                system_bytes,
                kind: OutboundOperationKind::DeselectResponse,
            },
            ControlMessage::LinktestRequest { system_bytes } => Self {
                session_id: u16::MAX,
                p_type: 0,
                s_type: 5,
                system_bytes,
                kind: OutboundOperationKind::LinktestRequest,
            },
            ControlMessage::LinktestResponse { system_bytes } => Self {
                session_id: u16::MAX,
                p_type: 0,
                s_type: 6,
                system_bytes,
                kind: OutboundOperationKind::LinktestResponse,
            },
            ControlMessage::RejectRequest {
                session_id,
                system_bytes,
                ..
            } => Self {
                session_id,
                p_type: 0,
                s_type: 7,
                system_bytes,
                kind: OutboundOperationKind::RejectRequest,
            },
            ControlMessage::SeparateRequest {
                session_id,
                system_bytes,
            } => Self {
                session_id,
                p_type: 0,
                s_type: 9,
                system_bytes,
                kind: OutboundOperationKind::SeparateRequest,
            },
        }
    }

    /// Returns the raw two-byte Session ID.
    pub(crate) const fn session_id(self) -> u16 {
        self.session_id
    }

    /// Returns the raw Presentation Type.
    pub(crate) const fn p_type(self) -> u8 {
        self.p_type
    }

    /// Returns the raw Session Type.
    pub(crate) const fn s_type(self) -> u8 {
        self.s_type
    }

    /// Returns the four-byte header correlation.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        self.system_bytes
    }

    /// Returns the exact outbound operation kind.
    pub(crate) const fn kind(self) -> OutboundOperationKind {
        self.kind
    }

    /// Returns the semantic role of the outbound message.
    pub(crate) const fn role(self) -> OutboundRole {
        self.kind.role()
    }
}

/// Visibility state that governs how one outbound identity may be correlated.
///
/// Core advances a live operation from `BeforeProceed` to `MayBeVisible`
/// atomically before emitting `ProceedWrite`. Only the latter state may
/// participate in live Reject attribution. Terminal history is retained only
/// for an identity that had reached `MayBeVisible`; it can classify a later
/// Reject as late, duplicate, or conflicting but can never terminate work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum OutboundCorrelationState {
    /// The write is scheduling, queued, or fenced and Core has not proceeded it.
    BeforeProceed,
    /// Core authorized Proceed, so bytes may already be peer-visible.
    MayBeVisible,
    /// Previously visible work is terminal and retained for diagnostics only.
    TerminalHistory,
}

impl OutboundCorrelationState {
    /// Returns the only Reject-matching authority granted by this state.
    pub(crate) const fn reject_eligibility(self) -> RejectCorrelationEligibility {
        match self {
            Self::BeforeProceed => RejectCorrelationEligibility::Excluded,
            Self::MayBeVisible => RejectCorrelationEligibility::Live,
            Self::TerminalHistory => RejectCorrelationEligibility::TerminalDiagnostic,
        }
    }

    /// Returns whether a matching Reject may terminate this live operation.
    pub(crate) const fn is_live_reject_candidate(self) -> bool {
        matches!(
            self.reject_eligibility(),
            RejectCorrelationEligibility::Live
        )
    }

    /// Returns whether this state may support terminal Reject diagnostics only.
    pub(crate) const fn is_terminal_reject_history(self) -> bool {
        matches!(
            self.reject_eligibility(),
            RejectCorrelationEligibility::TerminalDiagnostic
        )
    }
}

/// Permitted use of an outbound identity during peer Reject attribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RejectCorrelationEligibility {
    /// Exclude definitely non-visible work from both live and historical matches.
    Excluded,
    /// Permit global unique matching as a live operation candidate.
    Live,
    /// Permit late, duplicate, or conflicting classification, never mutation.
    TerminalDiagnostic,
}

/// Header field selected by one base or extension Reject reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RejectSelector {
    /// Header Byte 2 identifies the rejected Session Type.
    SessionType(
        /// Referenced SType value.
        u8,
    ),
    /// Header Byte 2 identifies the rejected Presentation Type.
    PresentationType(
        /// Referenced PType value.
        u8,
    ),
    /// Extension reason whose Header Byte 2 semantics are not configured.
    UnsupportedExtension(
        /// Losslessly retained raw Header Byte 2 value.
        u8,
    ),
}

impl RejectSelector {
    /// Interprets Header Byte 2 according to the frozen base-reason rules.
    ///
    /// Reason 2 selects PType; base reasons 1, 3, and 4 select SType.
    /// Extension reasons remain lossless but are never guessed.
    pub(crate) const fn from_reason(reason: RejectReason, header_byte_2: u8) -> Self {
        match reason.get() {
            2 => Self::PresentationType(header_byte_2),
            1 | 3 | 4 => Self::SessionType(header_byte_2),
            _ => Self::UnsupportedExtension(header_byte_2),
        }
    }

    /// Returns whether this selector exactly matches `identity`.
    ///
    /// Extension selectors always return `false` until explicit semantics are
    /// configured by a future subordinate profile.
    pub(crate) const fn matches(self, identity: OutboundHeaderIdentity) -> bool {
        match self {
            Self::SessionType(value) => identity.s_type == value,
            Self::PresentationType(value) => identity.p_type == value,
            Self::UnsupportedExtension(_) => false,
        }
    }
}

/// Generation-stamped peer Reject reference used for global unique matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RejectReference {
    /// TCP incarnation on which the Reject arrived.
    generation: ConnectionGeneration,
    /// Raw Session ID copied from the Reject header.
    session_id: u16,
    /// Four-byte correlation copied from the Reject header.
    system_bytes: SystemBytes,
    /// Exact non-zero base or extension reason.
    reason: RejectReason,
    /// Reason-dependent interpretation of Header Byte 2.
    selector: RejectSelector,
}

impl RejectReference {
    /// Creates a lossless peer Reject reference from validated header fields.
    pub(crate) const fn new(
        generation: ConnectionGeneration,
        session_id: u16,
        header_byte_2: u8,
        reason: RejectReason,
        system_bytes: SystemBytes,
    ) -> Self {
        Self {
            generation,
            session_id,
            system_bytes,
            reason,
            selector: RejectSelector::from_reason(reason, header_byte_2),
        }
    }

    /// Returns the generation that received this Reject.
    pub(crate) const fn generation(self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the raw Session ID referenced by the peer.
    pub(crate) const fn session_id(self) -> u16 {
        self.session_id
    }

    /// Returns the four-byte correlation referenced by the peer.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        self.system_bytes
    }

    /// Returns the exact non-zero Reject reason.
    pub(crate) const fn reason(self) -> RejectReason {
        self.reason
    }

    /// Returns the reason-dependent Header Byte 2 selector.
    pub(crate) const fn selector(self) -> RejectSelector {
        self.selector
    }

    /// Returns whether this reason can semantically reference `role`.
    ///
    /// Base reasons 1 and 2 may reference any outbound message. Reason 3
    /// describes an unexpected response and therefore admits response roles
    /// only. Reason 4 applies only to Data messages. Extension reasons remain
    /// non-attributable until explicit semantics are configured.
    pub(crate) const fn role_is_compatible(self, role: OutboundRole) -> bool {
        match self.reason.get() {
            1 | 2 => true,
            3 => matches!(
                role,
                OutboundRole::DataResponse | OutboundRole::ControlResponse
            ),
            4 => matches!(role, OutboundRole::DataPrimary | OutboundRole::DataResponse),
            _ => false,
        }
    }

    /// Returns whether raw correlation and selector fields match `identity`.
    ///
    /// This method intentionally does not decide role compatibility or global
    /// uniqueness; the operation correlation index owns those policies.
    pub(crate) const fn matches_header(self, identity: OutboundHeaderIdentity) -> bool {
        self.session_id == identity.session_id
            && self.system_bytes.get() == identity.system_bytes.get()
            && self.selector.matches(identity)
    }

    /// Classifies one outbound identity for this Reject reference.
    ///
    /// A candidate is excluded unless its complete selected header fields and
    /// semantic role both match. An exact candidate then inherits the
    /// visibility state's frozen authority: live mutation after Proceed,
    /// terminal diagnostics after completion, or no correlation beforehand.
    /// Extension reasons always return [`RejectCorrelationEligibility::Excluded`].
    pub(crate) const fn candidate_eligibility(
        self,
        identity: OutboundHeaderIdentity,
        state: OutboundCorrelationState,
    ) -> RejectCorrelationEligibility {
        if !self.role_is_compatible(identity.role()) || !self.matches_header(identity) {
            return RejectCorrelationEligibility::Excluded;
        }
        state.reject_eligibility()
    }
}

#[cfg(test)]
mod tests {
    use super::OutboundMessageShapeError;

    use crate::hsms::{
        model::ids::{ConnectionGeneration, Function, SessionId, Stream, SystemBytes},
        protocol::{
            header::{ControlMessage, DataHeader, DeselectStatus, RejectReason, SelectStatus},
            message::{DataMessage, ProtocolMessage},
        },
    };

    use super::{
        OutboundCorrelationState, OutboundHeaderIdentity, OutboundOperationKind, OutboundRole,
        RejectCorrelationEligibility, RejectReference, RejectSelector,
    };

    /// Creates a deterministic Data header identity for selector tests.
    fn identity() -> OutboundHeaderIdentity {
        let header = DataHeader::new(
            SessionId::new(7).expect("non-control Session ID"),
            Stream::new(1).expect("seven-bit stream"),
            Function::new(1),
            false,
            SystemBytes::new(0x0102_0304),
        );
        OutboundHeaderIdentity::from_protocol_message(&ProtocolMessage::Data(DataMessage::new(
            header, None,
        )))
        .expect("valid outbound message shape")
    }

    /// Creates one deterministic typed Data protocol message.
    fn data_message(function: u8, reply_expected: bool) -> ProtocolMessage {
        ProtocolMessage::Data(DataMessage::new(
            DataHeader::new(
                SessionId::new(7).expect("non-control Session ID"),
                Stream::new(3).expect("seven-bit stream"),
                Function::new(function),
                reply_expected,
                SystemBytes::new(0x0102_0304),
            ),
            None,
        ))
    }

    /// Confirms Data identities cannot disagree with the typed HSMS-SS header
    /// and distinguish W=0 Primary, W=1 Primary, Secondary, and SxF0 exactly.
    #[test]
    fn data_identity_is_derived_from_typed_protocol_message() {
        for (function, reply_expected, expected_kind, expected_role) in [
            (
                1,
                false,
                OutboundOperationKind::DataPrimaryW0,
                OutboundRole::DataPrimary,
            ),
            (
                1,
                true,
                OutboundOperationKind::DataPrimaryW1,
                OutboundRole::DataPrimary,
            ),
            (
                2,
                false,
                OutboundOperationKind::DataSecondaryW0,
                OutboundRole::DataResponse,
            ),
            (
                2,
                true,
                OutboundOperationKind::DataSecondaryW1,
                OutboundRole::DataResponse,
            ),
            (
                0,
                false,
                OutboundOperationKind::DataAbortW0,
                OutboundRole::DataResponse,
            ),
            (
                0,
                true,
                OutboundOperationKind::DataAbortW1,
                OutboundRole::DataResponse,
            ),
        ] {
            let identity = OutboundHeaderIdentity::from_protocol_message(&data_message(
                function,
                reply_expected,
            ))
            .expect("typed outbound message shape");

            assert_eq!(identity.session_id(), 7);
            assert_eq!(identity.p_type(), 0);
            assert_eq!(identity.s_type(), 0);
            assert_eq!(identity.system_bytes(), SystemBytes::new(0x0102_0304));
            assert_eq!(identity.kind(), expected_kind);
            assert_eq!(identity.role(), expected_role);
        }
    }

    /// Rejects a body-bearing SxF0 before an operation can retain its identity.
    #[test]
    fn body_bearing_abort_is_rejected_before_identity_construction() {
        let ProtocolMessage::Data(abort) = data_message(0, false) else {
            panic!("the Data-message helper must construct a Data variant");
        };
        let invalid = ProtocolMessage::Data(DataMessage::new(
            abort.header(),
            Some(crate::secs2::SecsItem::Binary(vec![1])),
        ));

        assert_eq!(
            OutboundHeaderIdentity::from_protocol_message(&invalid),
            Err(OutboundMessageShapeError::AbortCarriesBody)
        );
    }

    /// Confirms every typed control message maps to its exact fixed identity.
    #[test]
    fn control_identity_mapping_is_exhaustive_and_type_derived() {
        let system_bytes = SystemBytes::new(0x1122_3344);
        let cases = [
            (
                ControlMessage::SelectRequest {
                    session_id: 7,
                    system_bytes,
                },
                7,
                1,
                OutboundOperationKind::SelectRequest,
                OutboundRole::ControlRequest,
            ),
            (
                ControlMessage::SelectResponse {
                    session_id: 8,
                    status: SelectStatus::SUCCESS,
                    system_bytes,
                },
                8,
                2,
                OutboundOperationKind::SelectResponse,
                OutboundRole::ControlResponse,
            ),
            (
                ControlMessage::DeselectRequest {
                    session_id: 9,
                    system_bytes,
                },
                9,
                3,
                OutboundOperationKind::DeselectRequest,
                OutboundRole::ControlRequest,
            ),
            (
                ControlMessage::DeselectResponse {
                    session_id: 10,
                    status: DeselectStatus::SUCCESS,
                    system_bytes,
                },
                10,
                4,
                OutboundOperationKind::DeselectResponse,
                OutboundRole::ControlResponse,
            ),
            (
                ControlMessage::LinktestRequest { system_bytes },
                u16::MAX,
                5,
                OutboundOperationKind::LinktestRequest,
                OutboundRole::ControlRequest,
            ),
            (
                ControlMessage::LinktestResponse { system_bytes },
                u16::MAX,
                6,
                OutboundOperationKind::LinktestResponse,
                OutboundRole::ControlResponse,
            ),
            (
                ControlMessage::RejectRequest {
                    session_id: 11,
                    header_byte_2: 0,
                    reason: RejectReason::UNSUPPORTED_STYPE,
                    system_bytes,
                },
                11,
                7,
                OutboundOperationKind::RejectRequest,
                OutboundRole::RejectRequest,
            ),
            (
                ControlMessage::SeparateRequest {
                    session_id: 12,
                    system_bytes,
                },
                12,
                9,
                OutboundOperationKind::SeparateRequest,
                OutboundRole::ControlRequest,
            ),
        ];

        for (message, expected_session, expected_s_type, expected_kind, expected_role) in cases {
            let identity =
                OutboundHeaderIdentity::from_protocol_message(&ProtocolMessage::Control(message))
                    .expect("typed outbound message shape");

            assert_eq!(identity.session_id(), expected_session);
            assert_eq!(identity.p_type(), 0);
            assert_eq!(identity.s_type(), expected_s_type);
            assert_eq!(identity.system_bytes(), system_bytes);
            assert_eq!(identity.kind(), expected_kind);
            assert_eq!(identity.role(), expected_role);
        }
    }

    /// Confirms pre-Proceed work is excluded, MayBeVisible work is live, and
    /// terminal history can support diagnostics but never live mutation.
    #[test]
    fn correlation_state_freezes_reject_match_eligibility() {
        let reference = RejectReference::new(
            ConnectionGeneration::new(4),
            7,
            0,
            RejectReason::UNSUPPORTED_STYPE,
            SystemBytes::new(0x0102_0304),
        );
        assert_eq!(
            OutboundCorrelationState::BeforeProceed.reject_eligibility(),
            RejectCorrelationEligibility::Excluded
        );
        assert!(!OutboundCorrelationState::BeforeProceed.is_live_reject_candidate());
        assert!(!OutboundCorrelationState::BeforeProceed.is_terminal_reject_history());

        assert_eq!(
            OutboundCorrelationState::MayBeVisible.reject_eligibility(),
            RejectCorrelationEligibility::Live
        );
        assert!(OutboundCorrelationState::MayBeVisible.is_live_reject_candidate());
        assert!(!OutboundCorrelationState::MayBeVisible.is_terminal_reject_history());

        assert_eq!(
            OutboundCorrelationState::TerminalHistory.reject_eligibility(),
            RejectCorrelationEligibility::TerminalDiagnostic
        );
        assert!(!OutboundCorrelationState::TerminalHistory.is_live_reject_candidate());
        assert!(OutboundCorrelationState::TerminalHistory.is_terminal_reject_history());

        assert_eq!(
            reference.candidate_eligibility(identity(), OutboundCorrelationState::BeforeProceed),
            RejectCorrelationEligibility::Excluded
        );
        assert_eq!(
            reference.candidate_eligibility(identity(), OutboundCorrelationState::MayBeVisible),
            RejectCorrelationEligibility::Live
        );
        assert_eq!(
            reference.candidate_eligibility(identity(), OutboundCorrelationState::TerminalHistory),
            RejectCorrelationEligibility::TerminalDiagnostic
        );
    }

    /// Confirms reason 2 selects PType while the other base reasons select
    /// SType, and extensions retain bytes without inventing semantics.
    #[test]
    fn reject_selector_follows_base_reason_semantics() {
        assert_eq!(
            RejectSelector::from_reason(RejectReason::UNSUPPORTED_PTYPE, 9),
            RejectSelector::PresentationType(9)
        );
        for reason in [
            RejectReason::UNSUPPORTED_STYPE,
            RejectReason::TRANSACTION_NOT_OPEN,
            RejectReason::ENTITY_NOT_SELECTED,
        ] {
            assert_eq!(
                RejectSelector::from_reason(reason, 8),
                RejectSelector::SessionType(8)
            );
        }
        let extension = RejectReason::new(0x80).expect("non-zero extension reason");
        assert_eq!(
            RejectSelector::from_reason(extension, 6),
            RejectSelector::UnsupportedExtension(6)
        );
        assert!(!RejectSelector::UnsupportedExtension(0).matches(identity()));
    }

    /// Confirms matching requires exact Session ID, System Bytes, and the
    /// reason-selected header field rather than System Bytes alone.
    #[test]
    fn reject_reference_matches_complete_selected_header_identity() {
        let generation = ConnectionGeneration::new(4);
        let matching = RejectReference::new(
            generation,
            7,
            0,
            RejectReason::UNSUPPORTED_STYPE,
            SystemBytes::new(0x0102_0304),
        );
        let wrong_session = RejectReference::new(
            generation,
            8,
            0,
            RejectReason::UNSUPPORTED_STYPE,
            SystemBytes::new(0x0102_0304),
        );
        let wrong_system_bytes = RejectReference::new(
            generation,
            7,
            0,
            RejectReason::UNSUPPORTED_STYPE,
            SystemBytes::new(0x0102_0305),
        );

        assert!(matching.matches_header(identity()));
        assert!(!wrong_session.matches_header(identity()));
        assert!(!wrong_system_bytes.matches_header(identity()));
    }

    /// Confirms response-only and Data-only reasons cannot terminate an
    /// incompatible outbound role even when raw header fields happen to match.
    #[test]
    fn reject_reason_filters_outbound_roles_before_unique_attribution() {
        let generation = ConnectionGeneration::new(4);
        let roles = [
            OutboundRole::DataPrimary,
            OutboundRole::DataResponse,
            OutboundRole::ControlRequest,
            OutboundRole::ControlResponse,
            OutboundRole::RejectRequest,
        ];

        for reason in [
            RejectReason::UNSUPPORTED_STYPE,
            RejectReason::UNSUPPORTED_PTYPE,
        ] {
            let reference =
                RejectReference::new(generation, 7, 0, reason, SystemBytes::new(0x0102_0304));
            assert!(roles
                .into_iter()
                .all(|role| reference.role_is_compatible(role)));
        }

        let transaction_not_open = RejectReference::new(
            generation,
            7,
            0,
            RejectReason::TRANSACTION_NOT_OPEN,
            SystemBytes::new(0x0102_0304),
        );
        for role in roles {
            assert_eq!(
                transaction_not_open.role_is_compatible(role),
                matches!(
                    role,
                    OutboundRole::DataResponse | OutboundRole::ControlResponse
                )
            );
        }

        let not_selected = RejectReference::new(
            generation,
            7,
            0,
            RejectReason::ENTITY_NOT_SELECTED,
            SystemBytes::new(0x0102_0304),
        );
        for role in roles {
            assert_eq!(
                not_selected.role_is_compatible(role),
                matches!(role, OutboundRole::DataPrimary | OutboundRole::DataResponse)
            );
        }

        let extension = RejectReference::new(
            generation,
            7,
            0,
            RejectReason::new(0x80).expect("non-zero extension reason"),
            SystemBytes::new(0x0102_0304),
        );
        assert!(roles
            .into_iter()
            .all(|role| !extension.role_is_compatible(role)));
        assert!(!extension.matches_header(identity()));
        assert_eq!(
            extension.candidate_eligibility(identity(), OutboundCorrelationState::MayBeVisible),
            RejectCorrelationEligibility::Excluded
        );
    }
}
