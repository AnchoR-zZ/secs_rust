//! Compiles and evaluates the complete response contract for one W=1 Data
//! request. Matching is pure and checks every E37 correlation field rather
//! than treating System Bytes alone as sufficient.

use crate::hsms::{
    model::ids::{Function, SessionId, Stream, SystemBytes},
    protocol::header::DataHeader,
};

/// Why a local request function could not produce a response matcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatcherBuildError {
    /// Request functions must be odd and within the locally supported 1..=253 range.
    InvalidPrimaryFunction {
        /// Function rejected while compiling the matcher.
        function: Function,
    },
}

/// First response-contract field that rejected an inbound candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MismatchField {
    /// Candidate carried different System Bytes.
    SystemBytes,
    /// Candidate carried a different Data Session ID.
    SessionId,
    /// Candidate carried a different SECS stream.
    Stream,
    /// Candidate carried neither expected F+1 nor F0.
    Function,
    /// Candidate set W=true even though replies, including F0 aborts, require W=false.
    ReplyExpected,
    /// Candidate carried Message Text even though E5 defines SxF0 as header-only.
    MessageText,
}

/// Result of comparing one even-function Data response candidate to a compiled matcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatcherDecision {
    /// Header is the exact expected F+1 Secondary.
    Secondary,
    /// Candidate is a header-only SxF0 abort for this exact transaction.
    Abort,
    /// Candidate failed one required response-contract field.
    Mismatch {
        /// First field whose value rejected the candidate.
        field: MismatchField,
    },
}

/// Complete immutable response contract compiled before request scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResponseMatcher {
    /// Locally allocated transaction correlation value.
    system_bytes: SystemBytes,
    /// Data Session ID that the peer must echo.
    session_id: SessionId,
    /// SECS stream that the peer must echo.
    stream: Stream,
    /// Even response function derived from the valid primary function.
    expected_function: Function,
}

impl ResponseMatcher {
    /// Compiles the exact response contract for a local W=1 primary.
    ///
    /// `primary_function` must be odd and between one and 253 inclusive. The
    /// returned matcher requires matching System Bytes, Session ID, stream,
    /// F+1, and W=false; it also recognizes same-transaction F0 as an abort.
    pub(crate) fn compile(
        system_bytes: SystemBytes,
        session_id: SessionId,
        stream: Stream,
        primary_function: Function,
    ) -> Result<Self, MatcherBuildError> {
        let function = primary_function.get();
        if function == 0 || function > 253 || function.is_multiple_of(2) {
            return Err(MatcherBuildError::InvalidPrimaryFunction {
                function: primary_function,
            });
        }

        Ok(Self {
            system_bytes,
            session_id,
            stream,
            expected_function: Function::new(function + 1),
        })
    }

    /// Compares one inbound Data candidate against the complete response contract.
    ///
    /// `candidate` supplies the validated Data header and `has_message_text`
    /// records whether a decoded SECS-II body is present. The returned decision
    /// identifies an exact F+1 Secondary, a header-only F0 abort, or the first
    /// mismatched contract field. Message Text is constrained only for F0;
    /// normal F+1 body semantics remain the upper message layer's responsibility.
    pub(crate) const fn classify(
        self,
        candidate: DataHeader,
        has_message_text: bool,
    ) -> MatcherDecision {
        if candidate.system_bytes().get() != self.system_bytes.get() {
            return MatcherDecision::Mismatch {
                field: MismatchField::SystemBytes,
            };
        }
        if candidate.session_id().get() != self.session_id.get() {
            return MatcherDecision::Mismatch {
                field: MismatchField::SessionId,
            };
        }
        if candidate.stream().get() != self.stream.get() {
            return MatcherDecision::Mismatch {
                field: MismatchField::Stream,
            };
        }
        if candidate.reply_expected() {
            return MatcherDecision::Mismatch {
                field: MismatchField::ReplyExpected,
            };
        }
        if candidate.function().get() == 0 {
            if has_message_text {
                return MatcherDecision::Mismatch {
                    field: MismatchField::MessageText,
                };
            }
            return MatcherDecision::Abort;
        }
        if candidate.function().get() != self.expected_function.get() {
            return MatcherDecision::Mismatch {
                field: MismatchField::Function,
            };
        }
        MatcherDecision::Secondary
    }

    /// Returns the transaction's locally allocated System Bytes.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        self.system_bytes
    }

    /// Returns the Data Session ID required of a response.
    pub(crate) const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the SECS stream required of a response.
    pub(crate) const fn stream(self) -> Stream {
        self.stream
    }

    /// Returns the compiled even F+1 response function.
    pub(crate) const fn expected_function(self) -> Function {
        self.expected_function
    }
}

#[cfg(test)]
mod tests {
    //! Matcher tests exercise every positive and negative correlation field.

    use super::*;

    /// Builds a valid matcher used by field-focused tests.
    fn matcher() -> ResponseMatcher {
        ResponseMatcher::compile(
            SystemBytes::new(9),
            SessionId::new(3).expect("data session"),
            Stream::new(5).expect("stream"),
            Function::new(1),
        )
        .expect("valid primary")
    }

    /// Builds a Data header while allowing one test to override each field.
    fn header(
        system_bytes: u32,
        session_id: u16,
        stream: u8,
        function: u8,
        reply_expected: bool,
    ) -> DataHeader {
        DataHeader::new(
            SessionId::new(session_id).expect("data session"),
            Stream::new(stream).expect("stream"),
            Function::new(function),
            reply_expected,
            SystemBytes::new(system_bytes),
        )
    }

    /// Confirms only odd functions in the inclusive 1..=253 range compile.
    #[test]
    fn compile_rejects_zero_even_and_255_functions() {
        for invalid in [0, 2, 254, 255] {
            assert_eq!(
                ResponseMatcher::compile(
                    SystemBytes::new(1),
                    SessionId::new(1).expect("session"),
                    Stream::new(1).expect("stream"),
                    Function::new(invalid),
                ),
                Err(MatcherBuildError::InvalidPrimaryFunction {
                    function: Function::new(invalid),
                })
            );
        }
        assert!(ResponseMatcher::compile(
            SystemBytes::new(1),
            SessionId::new(1).expect("session"),
            Stream::new(1).expect("stream"),
            Function::new(253),
        )
        .is_ok());
    }

    /// Confirms an exact F+1 header with W=false matches.
    #[test]
    fn exact_secondary_matches() {
        assert_eq!(
            matcher().classify(header(9, 3, 5, 2, false), false),
            MatcherDecision::Secondary
        );
    }

    /// Confirms normal F+1 body semantics remain outside transaction matching.
    #[test]
    fn exact_secondary_with_message_text_matches() {
        assert_eq!(
            matcher().classify(header(9, 3, 5, 2, false), true),
            MatcherDecision::Secondary
        );
    }

    /// Confirms different System Bytes reject an otherwise exact response.
    #[test]
    fn system_bytes_mismatch_is_rejected() {
        assert_eq!(
            matcher().classify(header(10, 3, 5, 2, false), false),
            MatcherDecision::Mismatch {
                field: MismatchField::SystemBytes,
            }
        );
    }

    /// Confirms a different Session ID rejects an otherwise exact response.
    #[test]
    fn session_mismatch_is_rejected() {
        assert_eq!(
            matcher().classify(header(9, 4, 5, 2, false), false),
            MatcherDecision::Mismatch {
                field: MismatchField::SessionId,
            }
        );
    }

    /// Confirms a different stream rejects an otherwise exact response.
    #[test]
    fn stream_mismatch_is_rejected() {
        assert_eq!(
            matcher().classify(header(9, 3, 6, 2, false), false),
            MatcherDecision::Mismatch {
                field: MismatchField::Stream,
            }
        );
    }

    /// Confirms a different even function rejects an otherwise exact response.
    #[test]
    fn function_mismatch_is_rejected() {
        assert_eq!(
            matcher().classify(header(9, 3, 5, 4, false), false),
            MatcherDecision::Mismatch {
                field: MismatchField::Function,
            }
        );
    }

    /// Confirms W=true rejects the expected F+1 response.
    #[test]
    fn response_with_w_bit_is_rejected() {
        assert_eq!(
            matcher().classify(header(9, 3, 5, 2, true), false),
            MatcherDecision::Mismatch {
                field: MismatchField::ReplyExpected,
            }
        );
    }

    /// Confirms same-session, same-stream, same-System-Bytes F0 is an abort.
    #[test]
    fn exact_f0_is_an_abort() {
        assert_eq!(
            matcher().classify(header(9, 3, 5, 0, false), false),
            MatcherDecision::Abort
        );
    }

    /// Confirms W=true rejects F0 before it can be classified as an abort.
    #[test]
    fn f0_with_w_bit_is_rejected() {
        assert_eq!(
            matcher().classify(header(9, 3, 5, 0, true), false),
            MatcherDecision::Mismatch {
                field: MismatchField::ReplyExpected,
            }
        );
    }

    /// Confirms F0 with Message Text is rejected because aborts are header-only.
    #[test]
    fn f0_with_message_text_is_rejected() {
        assert_eq!(
            matcher().classify(header(9, 3, 5, 0, false), true),
            MatcherDecision::Mismatch {
                field: MismatchField::MessageText,
            }
        );
    }

    /// Confirms F0 with another correlation field is not this transaction's abort.
    #[test]
    fn mismatched_f0_does_not_abort() {
        assert_eq!(
            matcher().classify(header(9, 3, 6, 0, false), false),
            MatcherDecision::Mismatch {
                field: MismatchField::Stream,
            }
        );
    }
}
