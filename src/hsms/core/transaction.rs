//! Pure response-contract matching and bounded B2 transaction tombstones.
//!
//! Session Core stores one immutable contract per outbound Data Request. This
//! module classifies normal Secondaries, header-only F0 aborts, peer Rejects,
//! and late duplicates without depending on runtime or session-state policy.

use std::collections::VecDeque;

use crate::hsms::{
    model::ids::{Function, SessionId, Stream, SystemBytes},
    protocol::{
        header::{DataHeader, RejectReason},
        message::DataMessage,
    },
};

/// Returns whether one base-standard Reject exactly attributes Data traffic.
///
/// Reason 2 interprets Header Byte 2 as PType, while reasons 1, 3, and 4
/// interpret it as SType. The B2 Data profile fixes both fields to zero.
/// Extension reasons never terminate a command even if their tuple matches.
pub(crate) fn matches_data_reject(
    expected_session_id: SessionId,
    expected_system_bytes: SystemBytes,
    session_id: u16,
    header_byte_2: u8,
    reason: RejectReason,
    system_bytes: SystemBytes,
) -> bool {
    if expected_session_id.get() != session_id || expected_system_bytes != system_bytes {
        return false;
    }
    match reason.get() {
        2 => {
            let expected_ptype = 0;
            header_byte_2 == expected_ptype
        }
        1 | 3 | 4 => {
            let expected_stype = 0;
            header_byte_2 == expected_stype
        }
        _ => false,
    }
}

/// Result of comparing one Data message with an immutable response contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseMatch {
    /// Every normal Secondary field matched the checked F+1 contract.
    Normal,
    /// The message was a same-transaction, header-only F0 abort.
    Abort,
    /// At least one required field or the F0 body contract did not match.
    Mismatch,
}

/// Full immutable matching contract for one outbound W=1 Data Primary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResponseContract {
    /// Data Session ID copied into the outbound request header.
    session_id: SessionId,
    /// Core-assigned transaction correlation value.
    system_bytes: SystemBytes,
    /// Seven-bit stream copied into the outbound request header.
    stream: Stream,
    /// Checked F+1 normal Secondary, or `None` for an F255 request.
    normal_function: Option<Function>,
}

impl ResponseContract {
    /// Compiles a response contract from one outbound Primary tuple.
    ///
    /// `primary_function` is expected to have already passed Core's non-zero
    /// odd-function validation. F255 deliberately produces no wrapped or
    /// saturated normal matcher and remains abort-only.
    pub(crate) fn for_request(
        session_id: SessionId,
        system_bytes: SystemBytes,
        stream: Stream,
        primary_function: Function,
    ) -> Self {
        let normal_function = primary_function.get().checked_add(1).map(Function::new);
        Self {
            session_id,
            system_bytes,
            stream,
            normal_function,
        }
    }

    /// Returns the Data Session ID required by this contract.
    pub(crate) const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the System Bytes required by this contract.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        self.system_bytes
    }

    /// Returns the request stream required by normal and abort matches.
    pub(crate) const fn stream(self) -> Stream {
        self.stream
    }

    /// Returns the checked normal function, absent for abort-only F255.
    pub(crate) const fn normal_function(self) -> Option<Function> {
        self.normal_function
    }

    /// Classifies `message` against every normal and F0 abort field.
    ///
    /// Normal matching requires Session ID, System Bytes, Stream, W=false and
    /// checked F+1. Abort matching requires the same tuple, W=false, F0, and no
    /// Message Text. A mismatch never consumes this immutable contract.
    pub(crate) fn classify(self, message: &DataMessage) -> ResponseMatch {
        let header = message.header();
        if !self.matches_base_header(header) || header.reply_expected() {
            return ResponseMatch::Mismatch;
        }
        if header.function() == Function::new(0) {
            return if message.body().is_none() {
                ResponseMatch::Abort
            } else {
                ResponseMatch::Mismatch
            };
        }
        if self.normal_function == Some(header.function()) {
            ResponseMatch::Normal
        } else {
            ResponseMatch::Mismatch
        }
    }

    /// Returns whether Data tuple fields shared by Secondary and Reject match.
    pub(crate) fn matches_correlation(self, session_id: u16, system_bytes: SystemBytes) -> bool {
        self.session_id.get() == session_id && self.system_bytes == system_bytes
    }

    /// Returns whether one reason-aware peer Reject names this request exactly.
    pub(crate) fn matches_reject(
        self,
        session_id: u16,
        header_byte_2: u8,
        reason: RejectReason,
        system_bytes: SystemBytes,
    ) -> bool {
        matches_data_reject(
            self.session_id,
            self.system_bytes,
            session_id,
            header_byte_2,
            reason,
            system_bytes,
        )
    }

    /// Returns whether `header` has this contract's full non-function tuple.
    fn matches_base_header(self, header: DataHeader) -> bool {
        self.session_id == header.session_id()
            && self.system_bytes == header.system_bytes()
            && self.stream == header.stream()
    }
}

/// Bounded FIFO of recently completed Data response contracts.
#[derive(Debug)]
pub(crate) struct TransactionTombstones {
    /// Maximum number of completed contracts retained for duplicate isolation.
    capacity: usize,
    /// Contracts ordered from oldest at the front to newest at the back.
    entries: VecDeque<ResponseContract>,
}

impl TransactionTombstones {
    /// Creates an empty FIFO that retains at most `capacity` contracts.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::with_capacity(capacity),
        }
    }

    /// Records the newest completed contract and evicts the oldest if full.
    pub(crate) fn insert(&mut self, contract: ResponseContract) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(contract);
    }

    /// Returns whether any retained contract classifies `message` as terminal.
    pub(crate) fn contains_message(&self, message: &DataMessage) -> bool {
        self.entries
            .iter()
            .any(|contract| contract.classify(message) != ResponseMatch::Mismatch)
    }

    /// Returns whether a retained contract has this Reject correlation tuple.
    pub(crate) fn contains_correlation(&self, session_id: u16, system_bytes: SystemBytes) -> bool {
        self.entries
            .iter()
            .any(|contract| contract.matches_correlation(session_id, system_bytes))
    }

    /// Returns whether a retained contract exactly matches a peer Reject.
    pub(crate) fn contains_reject(
        &self,
        session_id: u16,
        header_byte_2: u8,
        reason: RejectReason,
        system_bytes: SystemBytes,
    ) -> bool {
        self.entries.iter().any(|contract| {
            contract.matches_reject(session_id, header_byte_2, reason, system_bytes)
        })
    }

    /// Returns the number of retained completed contracts.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        hsms::{
            model::ids::{Function, SessionId, Stream, SystemBytes},
            protocol::{
                header::{DataHeader, RejectReason},
                message::DataMessage,
            },
        },
        secs2::SecsItem,
    };

    use super::{matches_data_reject, ResponseContract, ResponseMatch, TransactionTombstones};

    /// Returns the fixed Data Session ID used by matcher tests.
    fn session_id() -> SessionId {
        SessionId::new(7).expect("fixture session id must be valid")
    }

    /// Returns the fixed stream used by matcher tests.
    fn stream() -> Stream {
        Stream::new(3).expect("fixture stream must be valid")
    }

    /// Builds one Data message with independently variable matching fields.
    fn message(
        session_id: SessionId,
        system_bytes: u32,
        stream: Stream,
        function: u8,
        reply_expected: bool,
        body: Option<SecsItem>,
    ) -> DataMessage {
        DataMessage::new(
            DataHeader::new(
                session_id,
                stream,
                Function::new(function),
                reply_expected,
                SystemBytes::new(system_bytes),
            ),
            body,
        )
    }

    /// Confirms normal matching requires every response-header field.
    #[test]
    fn normal_match_requires_full_tuple_and_w_false() {
        let contract = ResponseContract::for_request(
            session_id(),
            SystemBytes::new(11),
            stream(),
            Function::new(5),
        );
        let correct = message(
            session_id(),
            11,
            stream(),
            6,
            false,
            Some(SecsItem::U1(vec![])),
        );
        assert_eq!(contract.classify(&correct), ResponseMatch::Normal);

        let other_session = SessionId::new(8).expect("fixture session id must be valid");
        let other_stream = Stream::new(4).expect("fixture stream must be valid");
        for mismatched in [
            message(other_session, 11, stream(), 6, false, None),
            message(session_id(), 12, stream(), 6, false, None),
            message(session_id(), 11, other_stream, 6, false, None),
            message(session_id(), 11, stream(), 4, false, None),
            message(session_id(), 11, stream(), 6, true, None),
        ] {
            assert_eq!(contract.classify(&mismatched), ResponseMatch::Mismatch);
        }
    }

    /// Confirms F0 aborts require no body and F255 has no normal response.
    #[test]
    fn abort_is_header_only_and_f255_is_abort_only() {
        let contract = ResponseContract::for_request(
            session_id(),
            SystemBytes::new(17),
            stream(),
            Function::new(255),
        );
        assert_eq!(contract.normal_function(), None);
        assert_eq!(
            contract.classify(&message(session_id(), 17, stream(), 0, false, None)),
            ResponseMatch::Abort
        );
        assert_eq!(
            contract.classify(&message(
                session_id(),
                17,
                stream(),
                0,
                false,
                Some(SecsItem::List(Vec::new())),
            )),
            ResponseMatch::Mismatch
        );
        assert_eq!(
            contract.classify(&message(session_id(), 17, stream(), 255, false, None)),
            ResponseMatch::Mismatch
        );
    }

    /// Confirms the bounded ledger isolates retained messages and evicts FIFO.
    #[test]
    fn tombstones_are_bounded_fifo() {
        let mut tombstones = TransactionTombstones::new(2);
        for system_bytes in [1, 2, 3] {
            tombstones.insert(ResponseContract::for_request(
                session_id(),
                SystemBytes::new(system_bytes),
                stream(),
                Function::new(1),
            ));
        }
        assert_eq!(tombstones.len(), 2);
        assert!(!tombstones.contains_message(&message(session_id(), 1, stream(), 2, false, None,)));
        assert!(tombstones.contains_message(&message(session_id(), 2, stream(), 2, false, None,)));
        assert!(tombstones.contains_correlation(session_id().get(), SystemBytes::new(3)));
    }

    /// Confirms base Reject reasons use Data PType/SType zero and exact tuple.
    #[test]
    fn reject_matching_is_reason_aware_and_exact() {
        for reason in [
            RejectReason::UNSUPPORTED_STYPE,
            RejectReason::UNSUPPORTED_PTYPE,
            RejectReason::TRANSACTION_NOT_OPEN,
            RejectReason::ENTITY_NOT_SELECTED,
        ] {
            assert!(matches_data_reject(
                session_id(),
                SystemBytes::new(9),
                session_id().get(),
                0,
                reason,
                SystemBytes::new(9),
            ));
            assert!(!matches_data_reject(
                session_id(),
                SystemBytes::new(9),
                session_id().get(),
                1,
                reason,
                SystemBytes::new(9),
            ));
        }
        assert!(!matches_data_reject(
            session_id(),
            SystemBytes::new(9),
            session_id().get(),
            0,
            RejectReason::new(5).expect("extension reason must be non-zero"),
            SystemBytes::new(9),
        ));
        assert!(!matches_data_reject(
            session_id(),
            SystemBytes::new(9),
            SessionId::new(8).unwrap().get(),
            0,
            RejectReason::TRANSACTION_NOT_OPEN,
            SystemBytes::new(9),
        ));
    }
}
