//! Typed protocol commands accepted by endpoint admission and submitted to Core.
//!
//! Every command carries the application command identity used for exactly-once
//! completion. `HsmsCore` allocates the separate operation identity only after
//! it accepts a command or starts an autonomous protocol operation.

#![allow(dead_code)]

use crate::{hsms::model::ids::CommandId, secs2::SecsItem};

use super::{PrimaryMessage, ReplyToken};

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
pub(crate) enum ProtocolCommand {
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
    /// Consume an inbound reply capability and send its Secondary body.
    Reply {
        /// Application command completed when the Secondary frame commits.
        command_id: CommandId,
        /// Single-use authority containing generation and transaction metadata.
        token: ReplyToken,
        /// Optional Secondary Message Text supplied by the application.
        body: Option<SecsItem>,
    },
    /// Execute a typed HSMS control-plane operation.
    Control {
        /// Application command completed by the control transaction outcome.
        command_id: CommandId,
        /// Control operation requested without exposing a raw header.
        intent: ControlIntent,
    },
}
