//! Private implementation boundary for the generation-local WriteLedger.
//!
//! Cross-layer write plans and receipts live in `hsms::contracts`; this
//! module owns only ledger state and mutation logic.

mod ledger;
