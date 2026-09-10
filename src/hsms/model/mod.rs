//! Cross-component immutable value types retained by the rewrite baseline.

pub(crate) mod ids;
#[cfg(any(feature = "runtime-tokio", test))]
pub(crate) mod runtime;
