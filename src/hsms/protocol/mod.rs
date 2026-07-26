//! Runtime-neutral HSMS messages and protocol-violation contracts.
//!
//! These semantic values are shared by Wire, Profile, Core, and SessionDriver.
//! They contain no sockets, buffers, tasks, channels, or codec implementation
//! details, so higher layers do not need to depend on a concrete profile.

#![allow(dead_code)]

pub(crate) mod header;
pub(crate) mod message;
pub(crate) mod violation;
