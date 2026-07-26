//! Reliable application-delivery correlation boundary.
//!
//! The later pure ledger maps `DeliveryId` to the frozen `DeliveryPurpose`
//! contract without owning application queues or payload transport.

mod ledger;
