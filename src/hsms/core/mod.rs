//! Runtime-neutral Session Core with owned commands and ordered actions.
//!
//! This module implements deterministic B1 control and minimal B2 Data behavior
//! behind the values exchanged with `SessionDriver`. It deliberately contains
//! no runtime, transport, channel, socket, or clock implementation.

#![allow(dead_code, unused_imports)]

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
