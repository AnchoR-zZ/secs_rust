//! Immutable configuration consumed by one generation-scoped HSMS Core.
//!
//! This module derives the pure Core's startup policy from validated endpoint
//! values without introducing connection roles, sockets, clocks, or runtime
//! implementation types into the reducer boundary.

use std::time::Duration;

use crate::hsms::{model::ids::ConnectionGeneration, ConfigError, SessionId};

/// One generation permits at most one transactional control operation.
const CONTROL_OPERATION_CAPACITY: usize = 1;

/// Selection behavior used when a newly connected generation starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionStartup {
    /// Initiate the HSMS Select handshake after transport establishment.
    Initiate,
    /// Remain unselected until the peer sends `Select.req`.
    AwaitPeer,
}

/// Timer policy required by one generation-scoped Core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CoreTimeouts {
    /// E37 reply timeout for a sent primary Data message.
    t3: Duration,
    /// E37 control-transaction response timeout.
    t6: Duration,
    /// Maximum duration of each contiguous `NotSelected` tenure.
    ///
    /// T7 starts or re-arms whenever the session enters or re-enters
    /// `NotSelected`, and it is cancelled upon entering `Selected`.
    t7: Duration,
    /// Optional idle interval after which the Core initiates Linktest.
    linktest: Option<Duration>,
}

impl CoreTimeouts {
    /// Creates the Core's narrow timer policy.
    ///
    /// Generation assembly supplies `t3`, `t6`, `t7`, and `linktest` from an
    /// already validated endpoint timeout policy. This constructor performs no
    /// additional validation and returns only the values used by Core logic.
    pub(crate) const fn new(
        t3: Duration,
        t6: Duration,
        t7: Duration,
        linktest: Option<Duration>,
    ) -> Self {
        Self {
            t3,
            t6,
            t7,
            linktest,
        }
    }

    /// Returns the E37 T3 reply timeout used for Data transactions.
    pub(crate) const fn t3(self) -> Duration {
        self.t3
    }

    /// Returns the E37 T6 timeout used for control transactions.
    pub(crate) const fn t6(self) -> Duration {
        self.t6
    }

    /// Returns the maximum duration of each contiguous `NotSelected` tenure.
    ///
    /// T7 starts or re-arms whenever the session enters or re-enters
    /// `NotSelected`, and it is cancelled upon entering `Selected`.
    pub(crate) const fn t7(self) -> Duration {
        self.t7
    }

    /// Returns the optional idle interval used to initiate Linktest.
    pub(crate) const fn linktest(self) -> Option<Duration> {
        self.linktest
    }
}

/// Bounded resource capacities required by one generation-scoped Core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CoreLimits {
    /// Maximum number of active writes owned by the critical scheduler lane.
    critical_lane_capacity: usize,
    /// Maximum number of active writes owned by the Data scheduler lane.
    data_lane_capacity: usize,
    /// Maximum number of in-flight application-delivery correlations.
    application_delivery_capacity: usize,
    /// Maximum number of simultaneously pending request/response transactions.
    transaction_capacity: usize,
    /// Maximum number of completed transaction identities retained as tombstones.
    tombstone_capacity: usize,
    /// Maximum number of pending-publication or available reply capabilities.
    reply_capability_capacity: usize,
    /// Maximum number of live semantic operations across all ownership classes.
    operation_capacity: usize,
    /// Independent bound for terminal outbound-header correlation records.
    outbound_correlation_history_capacity: usize,
}

impl CoreLimits {
    /// Creates the Core's narrow bounded-resource policy.
    ///
    /// Generation assembly supplies every value from already validated
    /// endpoint limits. The two write-lane values must be identical to those
    /// used by `WireScheduler`; `application_delivery_capacity` comes from the
    /// application-event queue policy. The constructor derives the
    /// `OperationLedger` bound as `transactions + one control operation + both
    /// active write lanes`; it rejects arithmetic overflow rather than
    /// allowing an accidentally unbounded ledger.
    pub(crate) fn new(
        critical_lane_capacity: usize,
        data_lane_capacity: usize,
        application_delivery_capacity: usize,
        transaction_capacity: usize,
        tombstone_capacity: usize,
        reply_capability_capacity: usize,
    ) -> Result<Self, ConfigError> {
        let operation_capacity = transaction_capacity
            .checked_add(CONTROL_OPERATION_CAPACITY)
            .and_then(|capacity| capacity.checked_add(data_lane_capacity))
            .and_then(|capacity| capacity.checked_add(critical_lane_capacity))
            .ok_or(ConfigError::DerivedCapacityOverflow {
                field: "operation_capacity",
            })?;

        Ok(Self {
            critical_lane_capacity,
            data_lane_capacity,
            application_delivery_capacity,
            transaction_capacity,
            tombstone_capacity,
            reply_capability_capacity,
            operation_capacity,
            outbound_correlation_history_capacity: tombstone_capacity,
        })
    }

    /// Returns the independent active-write limit for the critical lane.
    pub(crate) const fn critical_lane_capacity(self) -> usize {
        self.critical_lane_capacity
    }

    /// Returns the independent active-write limit for the Data lane.
    pub(crate) const fn data_lane_capacity(self) -> usize {
        self.data_lane_capacity
    }

    /// Returns the maximum number of in-flight application deliveries.
    pub(crate) const fn application_delivery_capacity(self) -> usize {
        self.application_delivery_capacity
    }

    /// Returns the maximum number of simultaneously pending transactions.
    pub(crate) const fn transaction_capacity(self) -> usize {
        self.transaction_capacity
    }

    /// Returns the maximum number of retained transaction tombstones.
    pub(crate) const fn tombstone_capacity(self) -> usize {
        self.tombstone_capacity
    }

    /// Returns the maximum number of pending or available reply capabilities.
    pub(crate) const fn reply_capability_capacity(self) -> usize {
        self.reply_capability_capacity
    }

    /// Returns the derived maximum number of live semantic operations.
    pub(crate) const fn operation_capacity(self) -> usize {
        self.operation_capacity
    }

    /// Returns the independent terminal-history bound used for outbound
    /// Reject-correlation records.
    pub(crate) const fn outbound_correlation_history_capacity(self) -> usize {
        self.outbound_correlation_history_capacity
    }
}

/// Read-only inputs that configure one generation-scoped Core instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CoreConfig {
    /// TCP incarnation whose events and effects this Core owns.
    generation: ConnectionGeneration,
    /// Data-message Session ID accepted and emitted by this HSMS-SS session.
    session_id: SessionId,
    /// Whether this generation initiates selection or awaits its peer.
    startup: SelectionStartup,
    /// Narrow validated timer policy consumed by Core decisions.
    timeouts: CoreTimeouts,
    /// Narrow validated registry-capacity policy consumed by Core decisions.
    limits: CoreLimits,
}

impl CoreConfig {
    /// Creates Core configuration for one connection generation.
    ///
    /// Generation assembly must derive `generation`, `session_id`, `startup`,
    /// `timeouts`, and `limits` from the already validated endpoint
    /// configuration. The returned value contains only policy needed by Core
    /// logic and performs no additional validation.
    pub(crate) const fn new(
        generation: ConnectionGeneration,
        session_id: SessionId,
        startup: SelectionStartup,
        timeouts: CoreTimeouts,
        limits: CoreLimits,
    ) -> Self {
        Self {
            generation,
            session_id,
            startup,
            timeouts,
            limits,
        }
    }

    /// Returns the TCP generation owned by the configured Core.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the Data-message Session ID owned by the configured Core.
    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Returns whether the Core initiates selection or awaits its peer.
    pub(crate) const fn startup(&self) -> SelectionStartup {
        self.startup
    }

    /// Returns the narrow validated timer policy used for Core decisions.
    pub(crate) const fn timeouts(&self) -> CoreTimeouts {
        self.timeouts
    }

    /// Returns the narrow validated registry capacities used by Core logic.
    pub(crate) const fn limits(&self) -> CoreLimits {
        self.limits
    }
}

#[cfg(test)]
mod tests {
    use super::CoreLimits;

    /// Confirms asymmetric capacities retain their exact source fields instead
    /// of being reordered or collapsed into a shared write-lane limit.
    #[test]
    fn core_limits_preserve_independent_resource_capacities() {
        let limits = CoreLimits::new(3, 7, 11, 13, 17, 19).expect("capacities fit");

        assert_eq!(limits.critical_lane_capacity(), 3);
        assert_eq!(limits.data_lane_capacity(), 7);
        assert_eq!(limits.application_delivery_capacity(), 11);
        assert_eq!(limits.transaction_capacity(), 13);
        assert_eq!(limits.tombstone_capacity(), 17);
        assert_eq!(limits.reply_capability_capacity(), 19);
        assert_eq!(limits.operation_capacity(), 24);
        assert_eq!(limits.outbound_correlation_history_capacity(), 17);
    }

    /// Confirms an overflowing aggregate operation bound is rejected during
    /// configuration derivation instead of wrapping to a smaller capacity.
    #[test]
    fn core_limits_reject_operation_capacity_overflow() {
        assert_eq!(
            CoreLimits::new(1, 1, 1, usize::MAX, 1, 1),
            Err(crate::hsms::ConfigError::DerivedCapacityOverflow {
                field: "operation_capacity",
            })
        );
    }
}
