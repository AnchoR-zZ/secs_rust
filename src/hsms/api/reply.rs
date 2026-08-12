//! Ownership-preserving failures for reply commands rejected before Core admission.
//!
//! These values return the original single-use token and optional Secondary
//! body when bounded admission or generation validation fails. They never
//! represent failures after a command has entered the protocol core.

#![allow(dead_code)]

use std::{error::Error as StdError, fmt};

use thiserror::Error;

use crate::secs2::SecsItem;

use super::ReplyToken;

/// Application reply operation attempted before admission failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReplyIntent {
    /// Send the normal F+1 Secondary with an optional Message Text body.
    Secondary,
    /// Send a header-only SxF0 transaction abort.
    Abort,
    /// Release the capability locally without writing a protocol frame.
    Abandon,
}

/// Stable reason a reply command was rejected before Core admission.
///
/// These reasons are intentionally limited to checks that the endpoint
/// boundary can make without consuming the reply capability. Failures after
/// Core accepts ownership are reported through the normal command completion.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReplyAdmissionReason {
    /// The logical endpoint has not been started.
    #[error("HSMS endpoint is not running")]
    NotRunning,
    /// The endpoint is running but has no usable TCP generation.
    #[error("HSMS endpoint has no open TCP generation")]
    NotConnected,
    /// The reply operation requires an established Selected session.
    #[error("HSMS operation requires a Selected session")]
    NotSelected,
    /// Shutdown has closed admission for reply commands.
    #[error("HSMS endpoint is draining and no longer accepts this operation")]
    Draining,
    /// Cleanup could not prove that the previous generation released resources.
    #[error("HSMS endpoint is faulted and must be stopped before restart")]
    Faulted,
    /// The token belongs to a TCP incarnation that is no longer current.
    #[error("operation belongs to a stale TCP generation")]
    StaleConnectionGeneration,
    /// Admission could not reserve all bounded resources atomically.
    #[error("operation was rejected by bounded admission")]
    Backpressure,
    /// The owning endpoint runtime is no longer executing.
    #[error("HSMS runtime has stopped")]
    RuntimeStopped,
    /// The token's private hint says a normal F+1 Secondary cannot be formed.
    ///
    /// Core validates the authoritative reply contract again after admission.
    #[error("this inbound Primary cannot form an F+1 Secondary; use abort_reply or abandon_reply")]
    ReplyRequiresAbort,
}

/// A reply operation rejected before ownership reached the protocol core.
///
/// The caller receives every moved input back and may retry, abort, abandon,
/// or otherwise dispose of the token. Once a typed command enters Core, its
/// token is not returned through this admission error.
pub struct ReplyAdmissionError {
    /// Stable reason why admission rejected the operation.
    reason: ReplyAdmissionReason,
    /// Kind of reply operation that was attempted.
    intent: ReplyIntent,
    /// Still-unconsumed reply capability returned to the caller.
    token: ReplyToken,
    /// Original normal-Secondary body, or `None` for Abort and Abandon.
    body: Option<SecsItem>,
}

impl ReplyAdmissionError {
    /// Returns a rejected normal-Secondary operation with all inputs intact.
    pub(crate) const fn secondary(
        reason: ReplyAdmissionReason,
        token: ReplyToken,
        body: Option<SecsItem>,
    ) -> Self {
        Self {
            reason,
            intent: ReplyIntent::Secondary,
            token,
            body,
        }
    }

    /// Returns a rejected header-only SxF0 abort with its token intact.
    pub(crate) const fn abort(reason: ReplyAdmissionReason, token: ReplyToken) -> Self {
        Self {
            reason,
            intent: ReplyIntent::Abort,
            token,
            body: None,
        }
    }

    /// Returns a rejected local capability abandonment with its token intact.
    pub(crate) const fn abandon(reason: ReplyAdmissionReason, token: ReplyToken) -> Self {
        Self {
            reason,
            intent: ReplyIntent::Abandon,
            token,
            body: None,
        }
    }

    /// Returns the stable pre-Core admission failure.
    #[must_use]
    pub const fn reason(&self) -> ReplyAdmissionReason {
        self.reason
    }

    /// Returns the reply operation whose admission failed.
    #[must_use]
    pub const fn intent(&self) -> ReplyIntent {
        self.intent
    }

    /// Borrows the unconsumed reply capability.
    pub const fn token(&self) -> &ReplyToken {
        &self.token
    }

    /// Borrows the original normal-Secondary body, if one was supplied.
    pub const fn body(&self) -> Option<&SecsItem> {
        self.body.as_ref()
    }

    /// Consumes the failure and returns intent, token, body, and stable reason.
    pub fn into_parts(
        self,
    ) -> (
        ReplyIntent,
        ReplyToken,
        Option<SecsItem>,
        ReplyAdmissionReason,
    ) {
        (self.intent, self.token, self.body, self.reason)
    }
}

impl fmt::Debug for ReplyAdmissionError {
    /// Formats safe admission metadata without traversing caller-owned Message Text.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplyAdmissionError")
            .field("reason", &self.reason)
            .field("intent", &self.intent)
            .field("token", &self.token)
            .field("has_body", &self.body.is_some())
            .finish()
    }
}

impl fmt::Display for ReplyAdmissionError {
    /// Formats the attempted reply intent and its stable admission failure.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "HSMS {:?} reply command was not admitted: {}",
            self.intent, self.reason
        )
    }
}

impl StdError for ReplyAdmissionError {
    /// Returns the underlying stable pre-Core admission reason.
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&self.reason)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use crate::{
        hsms::model::ids::{ConnectionGeneration, ReplyCapabilityId},
        secs2::{AsciiString, SecsItem},
    };

    use super::{ReplyAdmissionError, ReplyAdmissionReason, ReplyIntent, ReplyToken};

    /// Creates a deterministic single-use token for admission tests.
    fn token(normal_secondary_available: bool) -> ReplyToken {
        ReplyToken::for_test(
            ReplyCapabilityId::new(17),
            ConnectionGeneration::new(3),
            normal_secondary_available,
        )
    }

    /// Confirms a rejected Secondary returns its token, absent-or-present body,
    /// intent, and error without conflating absent text with abort semantics.
    #[test]
    fn secondary_failure_returns_all_moved_inputs() {
        let failure = ReplyAdmissionError::secondary(
            ReplyAdmissionReason::Backpressure,
            token(true),
            Some(SecsItem::Ascii(
                AsciiString::new("NO").expect("fixed ASCII"),
            )),
        );

        assert_eq!(failure.intent(), ReplyIntent::Secondary);
        assert_eq!(failure.reason(), ReplyAdmissionReason::Backpressure);
        assert!(failure.token().normal_secondary_available());
        assert!(matches!(failure.body(), Some(SecsItem::Ascii(value)) if value.as_str() == "NO"));

        let (intent, token, body, reason) = failure.into_parts();
        assert_eq!(intent, ReplyIntent::Secondary);
        assert!(token.normal_secondary_available());
        assert!(matches!(body, Some(SecsItem::Ascii(value)) if value.as_str() == "NO"));
        assert_eq!(reason, ReplyAdmissionReason::Backpressure);
    }

    /// Confirms abort and abandonment constructors cannot accidentally carry a
    /// normal-Secondary body and remain distinguishable by intent.
    #[test]
    fn abort_and_abandon_failures_are_bodyless_and_distinct() {
        let abort = ReplyAdmissionError::abort(ReplyAdmissionReason::NotSelected, token(false));
        let abandon = ReplyAdmissionError::abandon(
            ReplyAdmissionReason::StaleConnectionGeneration,
            token(false),
        );

        assert_eq!(abort.intent(), ReplyIntent::Abort);
        assert!(abort.body().is_none());
        assert_eq!(abandon.intent(), ReplyIntent::Abandon);
        assert!(abandon.body().is_none());
    }

    /// Confirms formatting identifies the attempted action and exposes the
    /// underlying stable operation error through the standard error chain.
    #[test]
    fn admission_failure_formats_and_exposes_source() {
        let failure = ReplyAdmissionError::abort(ReplyAdmissionReason::RuntimeStopped, token(true));

        assert_eq!(
            failure.to_string(),
            "HSMS Abort reply command was not admitted: HSMS runtime has stopped"
        );
        assert_eq!(
            failure.source().map(|source| source.to_string()),
            Some("HSMS runtime has stopped".to_owned())
        );
    }

    /// Confirms diagnostics reveal only body presence and never the caller's
    /// potentially sensitive SECS-II Message Text.
    #[test]
    fn admission_failure_debug_redacts_body_contents() {
        let failure = ReplyAdmissionError::secondary(
            ReplyAdmissionReason::Backpressure,
            token(true),
            Some(SecsItem::Ascii(
                AsciiString::new("PRIVATE-CONTENT").expect("fixed ASCII"),
            )),
        );

        let debug = format!("{failure:?}");
        assert!(debug.contains("has_body: true"));
        assert!(!debug.contains("PRIVATE-CONTENT"));
        assert!(!debug.contains("Ascii"));
    }
}
