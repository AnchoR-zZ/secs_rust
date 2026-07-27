//! Defines typed application intents and generation-scoped Core commands.
//!
//! Every command carries the application command identity used for exactly-once
//! completion. `HsmsCore` allocates the separate operation identity only after
//! it accepts a command or starts an autonomous protocol operation.

#![allow(dead_code)]

use crate::secs2::SecsItem;

use super::{
    completion::CommandCompletionAuthority,
    message::{PrimaryMessage, ReplyToken},
};

/// Typed control-plane intent. Applications cannot construct raw control
/// headers or choose System Bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlIntent {
    /// Initiate the HSMS Select handshake.
    Select,
    /// Initiate the HSMS Deselect handshake.
    Deselect,
    /// Probe the peer with Linktest while the generation remains open and the
    /// session is `NotSelected` or `Selected`; `Closing` and `Closed` sessions
    /// cannot start a probe.
    Linktest,
    /// Send `Separate.req` and terminate the generation without a response.
    Separate,
}

/// Generation-scoped command vocabulary consumed by `HsmsCore`.
#[derive(Debug)]
#[must_use = "a core command must be routed to the owning session driver"]
pub(crate) enum CoreCommand {
    /// Write a W=0 Primary and complete after the frame is committed locally.
    Send {
        /// Move-only authority eventually consumed by this command's terminal result.
        completion: CommandCompletionAuthority,
        /// Application-provided primary content; the Core supplies W=0 metadata.
        message: PrimaryMessage,
    },
    /// Write a W=1 Primary and await a response that matches its transaction.
    Request {
        /// Move-only authority eventually consumed by this request's terminal result.
        completion: CommandCompletionAuthority,
        /// Application-provided primary content; the Core supplies W=1 metadata.
        message: PrimaryMessage,
    },
    /// Consume a normal-mode inbound capability and send its F+1 Secondary.
    Reply {
        /// Move-only authority consumed when the Secondary frame terminates.
        completion: CommandCompletionAuthority,
        /// Single-use authority identifying the inbound W=1 Primary.
        token: ReplyToken,
        /// Optional Secondary Message Text supplied by the application.
        body: Option<SecsItem>,
    },
    /// Consume an inbound reply capability and send a header-only SxF0 abort.
    AbortReply {
        /// Move-only authority consumed when the abort frame terminates.
        completion: CommandCompletionAuthority,
        /// Single-use authority identifying the inbound W=1 Primary.
        token: ReplyToken,
    },
    /// Consume an inbound reply capability without writing a protocol frame.
    AbandonReply {
        /// Move-only authority consumed by the local abandonment transition.
        completion: CommandCompletionAuthority,
        /// Single-use authority released without a peer-visible response.
        token: ReplyToken,
    },
    /// Execute a typed HSMS control-plane operation.
    Control {
        /// Move-only authority consumed by the control transaction outcome.
        completion: CommandCompletionAuthority,
        /// Control operation requested without exposing a raw header.
        intent: ControlIntent,
    },
}
