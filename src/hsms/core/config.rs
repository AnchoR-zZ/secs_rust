//! Immutable configuration consumed by one generation-scoped HSMS Core.
//!
//! This module derives the pure Core's startup policy from validated endpoint
//! values without introducing connection roles, sockets, clocks, or runtime
//! implementation types into the reducer boundary.

use std::time::Duration;

use crate::hsms::{model::ids::ConnectionGeneration, SessionId};

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

/// Registry capacities required by one generation-scoped Core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CoreLimits {
    /// Maximum number of simultaneously pending request/response transactions.
    transaction_capacity: usize,
    /// Maximum number of completed transaction identities retained as tombstones.
    tombstone_capacity: usize,
    /// Non-zero maximum number of live single-use reply capabilities.
    reply_capability_capacity: usize,
}

impl CoreLimits {
    /// Creates the Core's narrow registry-capacity policy.
    ///
    /// Generation assembly supplies `transaction_capacity`,
    /// `tombstone_capacity`, and `reply_capability_capacity` from already
    /// validated endpoint limits. This constructor performs no additional
    /// validation and returns only the capacities used by Core logic.
    pub(crate) const fn new(
        transaction_capacity: usize,
        tombstone_capacity: usize,
        reply_capability_capacity: usize,
    ) -> Self {
        Self {
            transaction_capacity,
            tombstone_capacity,
            reply_capability_capacity,
        }
    }

    /// Returns the maximum number of simultaneously pending transactions.
    pub(crate) const fn transaction_capacity(self) -> usize {
        self.transaction_capacity
    }

    /// Returns the maximum number of retained transaction tombstones.
    pub(crate) const fn tombstone_capacity(self) -> usize {
        self.tombstone_capacity
    }

    /// Returns the non-zero maximum number of live reply capabilities.
    pub(crate) const fn reply_capability_capacity(self) -> usize {
        self.reply_capability_capacity
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
