//! Pure HSMS protocol reducer boundary.
//!
//! Wave 1 implements `HsmsCore::step`. Wave 0 freezes its complete Event and
//! Effect vocabulary so the Core and Runtime agents can work independently.

#![allow(dead_code)]

pub(crate) mod config;
pub(crate) mod control;
pub(crate) mod drain;
pub(crate) mod effect;
pub(crate) mod event;
pub(crate) mod transaction;
