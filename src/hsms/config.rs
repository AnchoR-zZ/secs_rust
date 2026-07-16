//! Validated configuration for one long-lived HSMS endpoint.
//!
//! The types in this file define connection role, protocol timers, bounded
//! resource capacities, and the SECS-II decoding policy consumed by later
//! lifecycle and generation-runtime implementations.

use std::{net::SocketAddr, time::Duration};

use crate::{
    hsms::{ConfigError, SessionId},
    secs2::DecodeLimits,
};

/// Minimum E37 Message Length: the mandatory ten-byte HSMS header.
const HSMS_HEADER_LENGTH: usize = 10;

/// Whether this endpoint initiates or accepts the TCP connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionMode {
    /// Initiate outbound TCP connections to the configured peer address.
    Active,
    /// Listen for inbound TCP connections on the configured bind address.
    Passive,
}

/// HSMS timeout configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HsmsTimeouts {
    /// Maximum duration allowed for one TCP connect attempt.
    connect: Duration,
    /// E37 reply timeout for a sent primary Data message.
    t3: Duration,
    /// E37 delay between active connection attempts.
    t5: Duration,
    /// E37 control-transaction response timeout.
    t6: Duration,
    /// E37 timeout for completing selection after TCP establishment.
    t7: Duration,
    /// E37 maximum interval between bytes of an incomplete message.
    t8: Duration,
    /// Optional idle interval after which the endpoint initiates Linktest.
    linktest: Option<Duration>,
}

impl HsmsTimeouts {
    /// Creates and validates an HSMS timer set.
    ///
    /// `connect` bounds TCP establishment; `t3`, `t5`, `t6`, `t7`, and `t8`
    /// carry their E37 meanings; `linktest` enables an optional idle probe.
    /// Returns [`ConfigError::ZeroDuration`] if any configured duration is
    /// zero, otherwise the validated timer set.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        connect: Duration,
        t3: Duration,
        t5: Duration,
        t6: Duration,
        t7: Duration,
        t8: Duration,
        linktest: Option<Duration>,
    ) -> Result<Self, ConfigError> {
        let timeouts = Self {
            connect,
            t3,
            t5,
            t6,
            t7,
            t8,
            linktest,
        };
        timeouts.validate()?;
        Ok(timeouts)
    }

    /// Validates that every mandatory timer and any enabled Linktest interval
    /// is non-zero, returning the first invalid field as [`ConfigError`].
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, duration) in [
            ("connect", self.connect),
            ("t3", self.t3),
            ("t5", self.t5),
            ("t6", self.t6),
            ("t7", self.t7),
            ("t8", self.t8),
        ] {
            if duration.is_zero() {
                return Err(ConfigError::ZeroDuration { field });
            }
        }

        if self.linktest.is_some_and(|duration| duration.is_zero()) {
            return Err(ConfigError::ZeroDuration { field: "linktest" });
        }

        Ok(())
    }

    #[must_use]
    /// Returns the TCP connect-attempt timeout.
    pub const fn connect(self) -> Duration {
        self.connect
    }

    #[must_use]
    /// Returns the E37 T3 reply timeout.
    pub const fn t3(self) -> Duration {
        self.t3
    }

    #[must_use]
    /// Returns the E37 T5 reconnect delay.
    pub const fn t5(self) -> Duration {
        self.t5
    }

    #[must_use]
    /// Returns the E37 T6 control-transaction timeout.
    pub const fn t6(self) -> Duration {
        self.t6
    }

    #[must_use]
    /// Returns the E37 T7 selection timeout.
    pub const fn t7(self) -> Duration {
        self.t7
    }

    #[must_use]
    /// Returns the E37 T8 inter-byte timeout.
    pub const fn t8(self) -> Duration {
        self.t8
    }

    #[must_use]
    /// Returns the optional idle Linktest interval.
    pub const fn linktest(self) -> Option<Duration> {
        self.linktest
    }
}

impl Default for HsmsTimeouts {
    /// Returns the library's initial timer policy for an endpoint.
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            t3: Duration::from_secs(45),
            t5: Duration::from_secs(10),
            t6: Duration::from_secs(5),
            t7: Duration::from_secs(10),
            t8: Duration::from_secs(5),
            linktest: Some(Duration::from_secs(30)),
        }
    }
}

/// Bounded resource capacities for one endpoint and generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointLimits {
    /// Largest E37 Message Length value accepted, excluding its four-byte prefix.
    max_message_length: usize,
    /// Maximum number of application commands waiting for admission.
    command_capacity: usize,
    /// Reserved outbound capacity for control and other critical frames.
    critical_lane_capacity: usize,
    /// Bounded outbound capacity for Data frames.
    data_lane_capacity: usize,
    /// Maximum number of reliable events buffered for the application.
    application_event_capacity: usize,
    /// Maximum number of simultaneously pending request/response transactions.
    transaction_capacity: usize,
    /// Maximum number of completed transaction identities retained as tombstones.
    tombstone_capacity: usize,
}

impl EndpointLimits {
    /// Creates and validates all endpoint resource bounds.
    ///
    /// `max_message_length` includes the ten-byte HSMS header but excludes the
    /// four-byte length prefix. The remaining parameters bound their named
    /// queues or registries. Returns [`ConfigError`] if the message length is
    /// not E37-representable or any capacity is zero.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_message_length: usize,
        command_capacity: usize,
        critical_lane_capacity: usize,
        data_lane_capacity: usize,
        application_event_capacity: usize,
        transaction_capacity: usize,
        tombstone_capacity: usize,
    ) -> Result<Self, ConfigError> {
        let limits = Self {
            max_message_length,
            command_capacity,
            critical_lane_capacity,
            data_lane_capacity,
            application_event_capacity,
            transaction_capacity,
            tombstone_capacity,
        };
        limits.validate()?;
        Ok(limits)
    }

    /// Validates the E37 Message Length bound and all queue/registry capacities.
    ///
    /// Returns `Ok(())` for a usable configuration or the first invalid field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_message_length < HSMS_HEADER_LENGTH {
            return Err(ConfigError::MessageLengthTooSmall {
                value: self.max_message_length,
            });
        }
        if self.max_message_length > u32::MAX as usize {
            return Err(ConfigError::MessageLengthTooLarge {
                value: self.max_message_length,
            });
        }

        for (field, value) in [
            ("command_capacity", self.command_capacity),
            ("critical_lane_capacity", self.critical_lane_capacity),
            ("data_lane_capacity", self.data_lane_capacity),
            (
                "application_event_capacity",
                self.application_event_capacity,
            ),
            ("transaction_capacity", self.transaction_capacity),
            ("tombstone_capacity", self.tombstone_capacity),
        ] {
            if value == 0 {
                return Err(ConfigError::ZeroCapacity { field });
            }
        }

        Ok(())
    }

    /// Maximum value accepted from the four-byte E37 Message Length field.
    ///
    /// The value includes the ten-byte HSMS header and Message Text, but not
    /// the four-byte length prefix itself.
    #[must_use]
    pub const fn max_message_length(self) -> usize {
        self.max_message_length
    }

    #[must_use]
    /// Returns the maximum number of commands waiting for admission.
    pub const fn command_capacity(self) -> usize {
        self.command_capacity
    }

    #[must_use]
    /// Returns the reserved capacity of the critical outbound lane.
    pub const fn critical_lane_capacity(self) -> usize {
        self.critical_lane_capacity
    }

    #[must_use]
    /// Returns the capacity of the bounded Data outbound lane.
    pub const fn data_lane_capacity(self) -> usize {
        self.data_lane_capacity
    }

    #[must_use]
    /// Returns the capacity of the reliable application event queue.
    pub const fn application_event_capacity(self) -> usize {
        self.application_event_capacity
    }

    #[must_use]
    /// Returns the maximum number of pending request/response transactions.
    pub const fn transaction_capacity(self) -> usize {
        self.transaction_capacity
    }

    #[must_use]
    /// Returns the maximum retained transaction tombstones.
    pub const fn tombstone_capacity(self) -> usize {
        self.tombstone_capacity
    }
}

impl Default for EndpointLimits {
    /// Returns bounded, general-purpose endpoint capacities for Wave 0.
    fn default() -> Self {
        Self {
            max_message_length: 16 * 1024 * 1024,
            command_capacity: 256,
            critical_lane_capacity: 32,
            data_lane_capacity: 256,
            application_event_capacity: 256,
            transaction_capacity: 256,
            tombstone_capacity: 512,
        }
    }
}

/// Validatable configuration for one long-lived logical HSMS endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointConfig {
    /// Whether the endpoint actively connects or passively accepts.
    mode: ConnectionMode,
    /// Peer address in active mode or local bind address in passive mode.
    address: SocketAddr,
    /// Data-message session identifier owned by this HSMS-SS endpoint.
    session_id: SessionId,
    /// Protocol and connection timer policy.
    timeouts: HsmsTimeouts,
    /// Queue, frame, and registry capacity policy.
    limits: EndpointLimits,
    /// Resource limits applied while decoding SECS-II Message Text.
    secs2_limits: DecodeLimits,
}

impl EndpointConfig {
    /// Builds an active endpoint that connects to `peer` and uses `session_id`
    /// for Data messages, returning a configuration with default policies.
    #[must_use]
    pub fn active(peer: SocketAddr, session_id: SessionId) -> Self {
        Self::new(ConnectionMode::Active, peer, session_id)
    }

    /// Builds a passive endpoint that listens on `bind` and uses `session_id`
    /// for Data messages, returning a configuration with default policies.
    #[must_use]
    pub fn passive(bind: SocketAddr, session_id: SessionId) -> Self {
        Self::new(ConnectionMode::Passive, bind, session_id)
    }

    /// Creates the shared configuration representation for `mode`, `address`,
    /// and `session_id`, installing default timer and resource policies.
    fn new(mode: ConnectionMode, address: SocketAddr, session_id: SessionId) -> Self {
        Self {
            mode,
            address,
            session_id,
            timeouts: HsmsTimeouts::default(),
            limits: EndpointLimits::default(),
            secs2_limits: DecodeLimits::default(),
        }
    }

    #[must_use]
    /// Replaces the timer policy with `timeouts` and returns the updated value.
    pub fn with_timeouts(mut self, timeouts: HsmsTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    #[must_use]
    /// Replaces endpoint resource capacities with `limits` and returns the
    /// updated value.
    pub fn with_limits(mut self, limits: EndpointLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    /// Replaces the SECS-II decoding policy with `limits` and returns the
    /// updated value.
    pub fn with_secs2_limits(mut self, limits: DecodeLimits) -> Self {
        self.secs2_limits = limits;
        self
    }

    /// Validates the nested timer, endpoint-capacity, and SECS-II policies.
    ///
    /// Returns `Ok(())` when every nested configuration is usable, otherwise
    /// the first [`ConfigError`].
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.timeouts.validate()?;
        self.limits.validate()?;
        self.secs2_limits.validate()?;
        Ok(())
    }

    #[must_use]
    /// Returns the configured active or passive connection role.
    pub const fn mode(&self) -> ConnectionMode {
        self.mode
    }

    #[must_use]
    /// Returns the peer address in active mode or bind address in passive mode.
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    #[must_use]
    /// Returns the configured Data-message session identifier.
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[must_use]
    /// Returns the endpoint's timer policy.
    pub const fn timeouts(&self) -> HsmsTimeouts {
        self.timeouts
    }

    #[must_use]
    /// Returns the endpoint's queue, frame, and registry limits.
    pub const fn limits(&self) -> EndpointLimits {
        self.limits
    }

    #[must_use]
    /// Returns the endpoint's SECS-II decoding limits.
    pub const fn secs2_limits(&self) -> DecodeLimits {
        self.secs2_limits
    }
}
