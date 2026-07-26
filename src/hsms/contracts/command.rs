//! Defines typed application intents and generation-scoped Core commands.
//!
//! Every command carries the application command identity used for exactly-once
//! completion. `HsmsCore` allocates the separate operation identity only after
//! it accepts a command or starts an autonomous protocol operation.

#![allow(dead_code)]

use crate::{hsms::model::ids::CommandId, secs2::SecsItem};

use super::message::{PrimaryMessage, ReplyToken};

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
pub(crate) enum CoreCommand {
    /// Write a W=0 Primary and complete after the frame is committed locally.
    Send {
        /// Application command whose completion must be delivered exactly once.
        command_id: CommandId,
        /// Application-provided primary content; the Core supplies W=0 metadata.
        message: PrimaryMessage,
    },
    /// Write a W=1 Primary and await a response that matches its transaction.
    Request {
        /// Application command whose Secondary or failure completes the request.
        command_id: CommandId,
        /// Application-provided primary content; the Core supplies W=1 metadata.
        message: PrimaryMessage,
    },
    /// Consume a normal-mode inbound capability and send its F+1 Secondary.
    Reply {
        /// Application command completed when the Secondary frame commits.
        command_id: CommandId,
        /// Single-use authority identifying the inbound W=1 Primary.
        token: ReplyToken,
        /// Optional Secondary Message Text supplied by the application.
        body: Option<SecsItem>,
    },
    /// Consume an inbound reply capability and send a header-only SxF0 abort.
    AbortReply {
        /// Application command completed when the abort frame commits.
        command_id: CommandId,
        /// Single-use authority identifying the inbound W=1 Primary.
        token: ReplyToken,
    },
    /// Consume an inbound reply capability without writing a protocol frame.
    AbandonReply {
        /// Application command completed when Core commits local abandonment.
        command_id: CommandId,
        /// Single-use authority released without a peer-visible response.
        token: ReplyToken,
    },
    /// Execute a typed HSMS control-plane operation.
    Control {
        /// Application command completed by the control transaction outcome.
        command_id: CommandId,
        /// Control operation requested without exposing a raw header.
        intent: ControlIntent,
    },
}
