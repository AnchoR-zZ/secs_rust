//! Typed protocol commands accepted by endpoint admission and submitted to Core.
//!
//! Every command carries separate command and operation identities so runtime
//! delivery, protocol execution, and exactly-once completion can be correlated
//! without exposing raw HSMS headers to applications.

#![allow(dead_code)]

use crate::{
    hsms::model::ids::{CommandId, OperationId},
    secs2::SecsItem,
};

use super::{PrimaryMessage, ReplyToken};

/// Typed control-plane intent. Applications cannot construct raw control
/// headers or choose System Bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlIntent {
    /// Initiate the HSMS Select handshake.
    Select,
    /// Initiate the HSMS Deselect handshake.
    Deselect,
    /// Probe the selected connection with Linktest.
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
        /// Core operation tracked through scheduling and write completion.
        operation_id: OperationId,
        /// Application-provided primary content; the Core supplies W=0 metadata.
        message: PrimaryMessage,
    },
    /// Write a W=1 Primary and await a response that matches its transaction.
    Request {
        /// Application command whose Secondary or failure completes the request.
        command_id: CommandId,
        /// Core operation tracked through write, T3, and response matching.
        operation_id: OperationId,
        /// Application-provided primary content; the Core supplies W=1 metadata.
        message: PrimaryMessage,
    },
    /// Consume an inbound reply capability and send its Secondary body.
    Reply {
        /// Application command completed when the Secondary frame commits.
        command_id: CommandId,
        /// Core operation tracked through scheduling and write completion.
        operation_id: OperationId,
        /// Single-use authority containing generation and transaction metadata.
        token: ReplyToken,
        /// Optional Secondary Message Text supplied by the application.
        body: Option<SecsItem>,
    },
    /// Execute a typed HSMS control-plane operation.
    Control {
        /// Application command completed by the control transaction outcome.
        command_id: CommandId,
        /// Core operation tracked through control state and any write/timer work.
        operation_id: OperationId,
        /// Control operation requested without exposing a raw header.
        intent: ControlIntent,
    },
}
