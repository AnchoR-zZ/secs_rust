//! Use-case-level transaction boundary across private Core resource owners.
//!
//! The assembled reducer will expose only atomic orchestration methods here;
//! callers will not receive independent mutable access to the contained ledgers.

mod ids;
