//! Runtime-neutral Session Core command and ordered-action contracts.
//!
//! This module freezes the values exchanged between `SessionDriver` and the
//! future deterministic `SessionCore`. It deliberately contains no runtime,
//! transport, channel, socket, or clock implementation.

#![allow(dead_code, unused_imports)]

mod action;
mod command;

pub(crate) use action::{CoreAction, CoreActions};
pub(crate) use command::{CoreCommand, CoreCommandKind, CoreCommandResult};
