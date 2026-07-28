//! Private Delivery child of the publication resource subaggregate.
//!
//! The private pure ledger owns move-only W=0, W=1, and protocol-notice
//! bindings while the parent resource aggregate coordinates Reply mutation.

mod ledger;

/// Exposes Delivery-ledger values only to the parent publication facade.
#[allow(unused_imports)]
pub(super) use ledger::{
    ApplicationDeliveryLedger, DeliveryBinding, DeliveryClearAuthorizationError,
    DeliveryClearAuthorizationFailure, DeliveryCloseCommit, DeliveryClosePreparation,
    DeliveryCloseSummary, DeliveryCommitError, DeliveryCommitFailure, DeliveryDisposition,
    DeliveryFinishCommit, DeliveryFinishPreparation, DeliveryLedgerConfigError,
    DeliveryPrepareError, DeliveryRegisterError, DeliveryRegisterRejection,
    DeliveryRegistrationPreparation, DeliveryResetCommit, DeliveryResetPreparation,
    DeliveryResetSummary, DeliveryTerminal, PreparedDeliveryView,
};
