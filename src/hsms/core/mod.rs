//! Pure HSMS protocol reducer boundary.
//!
//! Pure resource owners are implemented before the final
//! `HsmsCore::step` reducer assembles them. Runtime services remain outside
//! this module and communicate only through neutral contract values.

#![allow(dead_code)]

pub(crate) mod config;
pub(crate) mod control;
pub(crate) mod delivery;
pub(crate) mod drain;
pub(crate) mod operation;
pub(crate) mod reducer;
pub(crate) mod reply;
pub(crate) mod resources;
pub(crate) mod transaction;
pub(crate) mod write;
