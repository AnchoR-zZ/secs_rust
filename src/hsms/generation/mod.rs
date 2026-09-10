//! Single-owner protocol execution and bounded transport tasks for one TCP generation.

pub(crate) mod driver;
pub(crate) mod transport;

#[cfg(feature = "runtime-tokio")]
pub(crate) mod runtime;
