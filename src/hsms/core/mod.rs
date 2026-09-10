//! Runtime-neutral Session Core with owned commands and ordered actions.
//!
//! This module owns selection, transactions, reply capabilities and deadlines
//! behind the values exchanged with `SessionDriver`. It deliberately contains
//! no runtime, transport, channel, socket, or clock implementation.

mod action;
mod command;
mod session;
mod transaction;

pub(crate) use action::{CoreAction, CoreActions};
pub(crate) use command::{
    CommittedWrite, CoreCommand, CoreCommandKind, CoreCommandResult, MatchedSecondary,
    OutboundPrimary,
};
pub(crate) use session::{SessionCore, SessionCoreConfig};
