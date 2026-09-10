//! Runtime-specific resource bounds and finite lifecycle deadlines.
//! These settings are plain values usable without Tokio. Validation rejects
//! impossible capacities before any channel, listener, task or socket is created.

use super::{EndpointLimits, HsmsTimeouts};
use crate::hsms::ConfigError;
use std::time::{Duration, Instant};

/// Byte budgets and local runtime policy for one reusable logical endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimePolicy {
    /// Encoded-byte charge for application Primaries retained through completion.
    command_bytes: u32,
    /// Independent count of application replies retained through completion.
    reply_capacity: usize,
    /// Independent encoded-byte budget for application replies through completion.
    reply_bytes: u32,
    /// Aggregate wire-byte reservation for incomplete/reported incoming frames.
    inbound_bytes: u32,
    /// Independent encoded-byte charge held by queued application Primaries.
    primary_queue_bytes: u32,
    /// Aggregate encoded-byte reservation for Data writes and unconsumed outcomes.
    write_bytes: u32,
    /// Independent reliable protocol-error queue capacity.
    protocol_error_capacity: usize,
    /// Best-effort diagnostic queue capacity, independent of reliable messages.
    diagnostic_capacity: usize,
    /// Maximum local drain interval before Deselect or graceful connection close.
    drain: Duration,
    /// Maximum admission-to-full-write time, including all FIFO residence.
    residence: Duration,
    /// Maximum time spent actively writing a single frame.
    write: Duration,
    /// Maximum resource cleanup/join interval after cancellation.
    cleanup: Duration,
    /// Absolute bound on graceful close including drain and transport cleanup.
    shutdown: Duration,
    /// Whether Active connections automatically initiate one Select procedure.
    auto_select: bool,
}

impl Default for RuntimePolicy {
    /// Installs finite budgets and conservative local deadlines.
    fn default() -> Self {
        Self {
            command_bytes: 64 * 1024 * 1024,
            reply_capacity: 64,
            reply_bytes: 32 * 1024 * 1024,
            inbound_bytes: 32 * 1024 * 1024,
            primary_queue_bytes: 32 * 1024 * 1024,
            write_bytes: 64 * 1024 * 1024,
            protocol_error_capacity: 64,
            diagnostic_capacity: 64,
            drain: Duration::from_secs(5),
            residence: Duration::from_secs(10),
            write: Duration::from_secs(10),
            cleanup: Duration::from_secs(5),
            shutdown: Duration::from_secs(20),
            auto_select: true,
        }
    }
}

impl RuntimePolicy {
    /// Sets independent reply count and encoded-byte bounds, validated at build.
    #[must_use]
    pub const fn with_reply_budget(mut self, capacity: usize, bytes: u32) -> Self {
        self.reply_capacity = capacity;
        self.reply_bytes = bytes;
        self
    }

    /// Returns reply/abort/abandon admission slots independent of Primary commands.
    pub const fn reply_capacity(self) -> usize {
        self.reply_capacity
    }

    /// Returns the encoded-byte budget reserved exclusively for replies.
    pub const fn reply_bytes(self) -> u32 {
        self.reply_bytes
    }

    /// Sets best-effort diagnostic queue capacity; dropped records are counted.
    #[must_use]
    pub const fn with_diagnostic_capacity(mut self, capacity: usize) -> Self {
        self.diagnostic_capacity = capacity;
        self
    }

    /// Returns the independent best-effort diagnostic record capacity.
    pub const fn diagnostic_capacity(self) -> usize {
        self.diagnostic_capacity
    }
    /// Sets Primary-command, Reader-wire and Writer-Data budgets. Also initializes
    /// the independent application Primary queue budget to `inbound`; override it
    /// afterwards with `with_primary_queue_bytes` when different bounds are needed.
    #[must_use]
    pub const fn with_byte_budgets(mut self, command: u32, inbound: u32, write: u32) -> Self {
        self.command_bytes = command;
        self.inbound_bytes = inbound;
        self.primary_queue_bytes = inbound;
        self.write_bytes = write;
        self
    }

    /// Sets the independent application Primary queue budget in encoded wire bytes.
    #[must_use]
    pub const fn with_primary_queue_bytes(mut self, bytes: u32) -> Self {
        self.primary_queue_bytes = bytes;
        self
    }

    /// Returns queued application Primary bytes, independent of Reader reservations.
    pub const fn primary_queue_bytes(self) -> u32 {
        self.primary_queue_bytes
    }

    /// Sets independent reliable malformed-frame report capacity.
    #[must_use]
    pub const fn with_protocol_error_capacity(mut self, capacity: usize) -> Self {
        self.protocol_error_capacity = capacity;
        self
    }

    /// Sets local drain, queue residence, active write, cleanup and total-close bounds.
    #[must_use]
    pub const fn with_deadlines(
        mut self,
        drain: Duration,
        residence: Duration,
        write: Duration,
        cleanup: Duration,
        shutdown: Duration,
    ) -> Self {
        self.drain = drain;
        self.residence = residence;
        self.write = write;
        self.cleanup = cleanup;
        self.shutdown = shutdown;
        self
    }

    /// Enables or disables the Active supervisor's automatic initial Select.
    #[must_use]
    pub const fn with_auto_select(mut self, enabled: bool) -> Self {
        self.auto_select = enabled;
        self
    }

    /// Returns aggregate encoded-command byte capacity.
    pub const fn command_bytes(self) -> u32 {
        self.command_bytes
    }
    /// Returns Reader wire-byte capacity, excluding application queues and tree overhead.
    pub const fn inbound_bytes(self) -> u32 {
        self.inbound_bytes
    }
    /// Returns aggregate outbound Data-frame byte capacity.
    pub const fn write_bytes(self) -> u32 {
        self.write_bytes
    }
    /// Returns independent reliable malformed-frame report count capacity.
    pub const fn protocol_error_capacity(self) -> usize {
        self.protocol_error_capacity
    }
    /// Returns the local finite drain interval.
    pub const fn drain(self) -> Duration {
        self.drain
    }
    /// Returns the maximum admission-to-full-write residence.
    pub const fn residence(self) -> Duration {
        self.residence
    }
    /// Returns the active single-frame write interval.
    pub const fn write(self) -> Duration {
        self.write
    }
    /// Returns the cancellation-to-cleanup-proof interval.
    pub const fn cleanup(self) -> Duration {
        self.cleanup
    }
    /// Returns the absolute graceful-close bound including cleanup.
    pub const fn shutdown(self) -> Duration {
        self.shutdown
    }
    /// Returns whether Active generations initiate Select automatically.
    pub const fn auto_select(self) -> bool {
        self.auto_select
    }

    /// Checks all runtime arithmetic, budgets and monotonic-clock deadlines.
    /// A portable conservative capacity ceiling also fits Tokio on 32-bit hosts.
    pub fn validate(
        self,
        limits: EndpointLimits,
        timeouts: HsmsTimeouts,
    ) -> Result<(), ConfigError> {
        const MAX_CAPACITY: usize = 1 << 28;
        let invalid = |description| ConfigError::RuntimePolicy { description };
        let frame = limits
            .max_message_length()
            .checked_add(4)
            .ok_or_else(|| invalid("frame prefix length overflow"))?;
        if (self.command_bytes as usize) < frame
            || (self.reply_bytes as usize) < frame
            || (self.inbound_bytes as usize) < frame
            || (self.primary_queue_bytes as usize) < frame
            || (self.write_bytes as usize) < frame
        {
            return Err(invalid("each wire-byte budget must hold one maximum frame"));
        }
        let lanes = limits
            .critical_lane_capacity()
            .checked_add(limits.data_lane_capacity())
            .ok_or_else(|| invalid("Writer lane capacity overflow"))?;
        let commands = limits
            .command_capacity()
            .checked_add(self.reply_capacity)
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| invalid("endpoint command capacity overflow"))?;
        for count in [
            limits.command_capacity(),
            commands,
            self.reply_capacity,
            self.reply_bytes as usize,
            limits.critical_lane_capacity(),
            limits.data_lane_capacity(),
            limits.application_event_capacity(),
            limits.transaction_capacity(),
            limits.tombstone_capacity(),
            limits.reply_capability_capacity(),
            self.protocol_error_capacity,
            self.diagnostic_capacity,
            lanes,
            self.command_bytes as usize,
            self.inbound_bytes as usize,
            self.primary_queue_bytes as usize,
            self.write_bytes as usize,
        ] {
            if count == 0 || count > MAX_CAPACITY {
                return Err(invalid(
                    "runtime capacity is zero or exceeds portable channel limits",
                ));
            }
        }
        let now = Instant::now();
        for duration in [
            self.drain,
            self.residence,
            self.write,
            self.cleanup,
            self.shutdown,
            timeouts.connect(),
            timeouts.t3(),
            timeouts.t5(),
            timeouts.t6(),
            timeouts.t7(),
            timeouts.t8(),
        ] {
            if duration.is_zero() || now.checked_add(duration).is_none() {
                return Err(invalid(
                    "runtime deadline is zero or overflows the monotonic clock",
                ));
            }
        }
        if timeouts
            .linktest()
            .is_some_and(|duration| now.checked_add(duration).is_none())
        {
            return Err(invalid("Linktest interval overflows the monotonic clock"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsms::{EndpointConfig, SessionId};

    /// Creates a valid small endpoint configuration without touching transport.
    fn config() -> EndpointConfig {
        EndpointConfig::active(
            "127.0.0.1:5000".parse().unwrap(),
            SessionId::new(7).unwrap(),
        )
        .with_limits(EndpointLimits::new(10, 2, 1, 2, 2, 2, 2, 2).unwrap())
    }

    /// Default runtime policy is independently usable in the pure-feature build.
    #[test]
    fn defaults_validate_and_custom_settings_round_trip() {
        assert!(config().validate().is_ok());
        let policy = RuntimePolicy::default()
            .with_byte_budgets(14, 28, 42)
            .with_protocol_error_capacity(3)
            .with_auto_select(false)
            .with_deadlines(
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(3),
                Duration::from_secs(4),
                Duration::from_secs(5),
            );
        let endpoint = config().with_runtime(policy);
        assert!(endpoint.validate().is_ok());
        assert_eq!(endpoint.runtime(), policy);
        assert_eq!(
            (
                policy.command_bytes(),
                policy.inbound_bytes(),
                policy.write_bytes()
            ),
            (14, 28, 42)
        );
        assert_eq!(policy.protocol_error_capacity(), 3);
        assert!(!policy.auto_select());
        assert_eq!(
            (
                policy.drain(),
                policy.residence(),
                policy.write(),
                policy.cleanup(),
                policy.shutdown()
            ),
            (
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(3),
                Duration::from_secs(4),
                Duration::from_secs(5)
            )
        );
    }

    /// Reply count, byte and total queue arithmetic are validated before allocation.
    #[test]
    fn invalid_reply_budgets_are_rejected() {
        for (count, bytes) in [
            (0, 14),
            (1, 13),
            (usize::MAX, 14),
            (1 << 28, 14),
            (1, u32::MAX),
        ] {
            assert!(config()
                .with_runtime(RuntimePolicy::default().with_reply_budget(count, bytes))
                .validate()
                .is_err());
        }
        let policy = RuntimePolicy::default().with_reply_budget(1, 14);
        assert!(config().with_runtime(policy).validate().is_ok());
        assert_eq!((policy.reply_capacity(), policy.reply_bytes()), (1, 14));
    }

    /// The application queue has its own validated budget and explicit override.
    #[test]
    fn primary_queue_budget_is_separate_and_validated() {
        let policy = RuntimePolicy::default()
            .with_byte_budgets(14, 28, 42)
            .with_primary_queue_bytes(14);
        assert_eq!(policy.inbound_bytes(), 28);
        assert_eq!(policy.primary_queue_bytes(), 14);
        assert!(config().with_runtime(policy).validate().is_ok());
        for invalid in [0, 13, u32::MAX] {
            assert!(config()
                .with_runtime(policy.with_primary_queue_bytes(invalid))
                .validate()
                .is_err());
        }
    }

    /// Every aggregate byte budget must fit one complete maximum frame.
    #[test]
    fn undersized_wire_budgets_fail_before_runtime_construction() {
        for budgets in [(13, 14, 14), (14, 13, 14), (14, 14, 13), (0, 0, 0)] {
            assert!(config()
                .with_runtime(
                    RuntimePolicy::default().with_byte_budgets(budgets.0, budgets.1, budgets.2)
                )
                .validate()
                .is_err());
        }
    }

    /// Channel lane addition and registry counts cannot trigger runtime panics.
    #[test]
    fn unrepresentable_counts_and_lane_sums_are_rejected() {
        for limits in [
            EndpointLimits::new(10, usize::MAX, 1, 1, 1, 1, 1, 1).unwrap(),
            EndpointLimits::new(10, 1, usize::MAX, 1, 1, 1, 1, 1).unwrap(),
            EndpointLimits::new(10, 1, 1, 1, 1, 1, usize::MAX, 1).unwrap(),
        ] {
            assert!(config().with_limits(limits).validate().is_err());
        }
        for capacity in [0, usize::MAX] {
            assert!(config()
                .with_runtime(RuntimePolicy::default().with_diagnostic_capacity(capacity))
                .validate()
                .is_err());
            assert!(config()
                .with_runtime(RuntimePolicy::default().with_protocol_error_capacity(capacity))
                .validate()
                .is_err());
        }
    }

    /// Each local deadline rejects zero and clock overflow independently.
    #[test]
    fn zero_or_unrepresentable_runtime_deadlines_fail_validation() {
        for invalid in [Duration::ZERO, Duration::MAX] {
            for index in 0..5 {
                let mut deadlines = [Duration::from_secs(1); 5];
                deadlines[index] = invalid;
                let policy = RuntimePolicy::default().with_deadlines(
                    deadlines[0],
                    deadlines[1],
                    deadlines[2],
                    deadlines[3],
                    deadlines[4],
                );
                assert!(config().with_runtime(policy).validate().is_err());
            }
        }
    }
}
