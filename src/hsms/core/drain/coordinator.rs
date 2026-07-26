//! Coordinates one generation's first-reason-wins transport shutdown.
//!
//! The coordinator is a pure Sans-I/O state machine. It freezes an optional
//! exact-write barrier, emits at most one transport-close request, and
//! classifies stale or duplicate inputs without owning tasks, channels,
//! sockets, or runtime queues.

use crate::hsms::{
    core::drain::{DrainRequest, ResolvedCloseBarrier, WriteBarrier},
    model::{ids::WriteId, runtime::GenerationCloseReason},
};

/// Logical shutdown state retained for one connection generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrainState {
    /// No generation-close request has been accepted.
    Open,
    /// The first request waits for one exact outbound write to terminate.
    WaitingForExactWrite {
        /// Immutable first request whose reason and barrier remain authoritative.
        first_request: DrainRequest,
        /// Exact operation-to-write mapping that alone may release shutdown.
        barrier: WriteBarrier,
    },
    /// The unique transport-close request has already been emitted.
    TransportCloseRequested {
        /// Immutable first request whose reason remains authoritative.
        first_request: DrainRequest,
    },
    /// Runtime confirmed completion of the unique transport-close request.
    Closed {
        /// Immutable first request retained for terminal diagnostics.
        first_request: DrainRequest,
    },
}

impl DrainState {
    /// Returns the immutable first request retained by an active or closed drain.
    ///
    /// The return value is `None` only while the generation remains open.
    pub(crate) const fn first_request(self) -> Option<DrainRequest> {
        match self {
            Self::Open => None,
            Self::WaitingForExactWrite { first_request, .. }
            | Self::TransportCloseRequested { first_request }
            | Self::Closed { first_request } => Some(first_request),
        }
    }
}

/// Cause that made the coordinator emit its unique transport-close request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportCloseTrigger {
    /// The first request required transport closure without waiting for a write.
    InitialImmediateRequest,
    /// The exact frozen write reached a scheduler or writer terminal outcome.
    ExactWriteTerminal,
    /// A later immediate or fatal request superseded waiting, but not its reason.
    EscalatingRequest,
}

/// Structured classification for an input that intentionally caused no action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IgnoredDrainInput {
    /// Another close request arrived after the first request was frozen.
    AdditionalRequest {
        /// Logical state that rejected the additional request.
        state: DrainState,
    },
    /// A write terminal did not match the frozen exact-write barrier.
    WrongWriteTerminal {
        /// Exact write that remains authoritative.
        expected: WriteId,
        /// Stale or unrelated write supplied by the caller.
        actual: WriteId,
    },
    /// No live exact-write barrier could consume this write terminal.
    WriteTerminalWithoutBarrier {
        /// Logical state that made the terminal stale or irrelevant.
        state: DrainState,
    },
    /// Transport-close completion arrived before a close request was emitted.
    PrematureTransportCloseCompletion {
        /// Open or waiting state deliberately left unchanged.
        state: DrainState,
    },
    /// Runtime repeated completion after the coordinator was already closed.
    DuplicateTransportCloseCompletion,
}

/// Pure decision produced for one serialized drain input.
#[must_use = "Core must apply or deliberately classify every drain decision"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrainDecision {
    /// The first request was frozen and now waits for one exact write.
    WaitForExactWrite {
        /// Stable first reason retained for eventual transport closure.
        reason: GenerationCloseReason,
        /// Exact write boundary that must terminate before closure.
        barrier: WriteBarrier,
    },
    /// Runtime must execute the generation's one transport-close request.
    RequestTransportClose {
        /// Stable first reason published with the close request.
        reason: GenerationCloseReason,
        /// Transition that caused the unique request.
        trigger: TransportCloseTrigger,
    },
    /// Runtime completion changed the unique requested close to `Closed`.
    TransportCloseCompleted {
        /// Stable first reason retained through close completion.
        reason: GenerationCloseReason,
    },
    /// The input was stale, duplicate, premature, or otherwise irrelevant.
    Ignored {
        /// Structured no-action classification for diagnostics and tests.
        input: IgnoredDrainInput,
    },
}

/// Generation-local coordinator for ordered, idempotent transport shutdown.
#[derive(Debug)]
pub(crate) struct DrainCoordinator {
    /// Complete logical state for the current connection generation.
    state: DrainState,
}

impl DrainCoordinator {
    /// Creates an allocation-free coordinator in the open state.
    ///
    /// The returned coordinator has no active reason or write barrier.
    pub(crate) const fn new() -> Self {
        Self {
            state: DrainState::Open,
        }
    }

    /// Returns a copy of the current logical state for Core diagnostics.
    pub(crate) const fn state(&self) -> DrainState {
        self.state
    }

    /// Accepts or classifies one resolved generation-close `request`.
    ///
    /// The first request permanently owns the published close reason. An
    /// `AfterWrite` request may wait only for local Separate; every immediate
    /// request and every non-Separate reason closes now. A later such request
    /// escalates a wait to prevent an indefinitely stalled writer from
    /// deadlocking shutdown, while retaining the original reason.
    ///
    /// Returns a pure decision that either freezes the barrier, requests the
    /// transport close exactly once, or classifies an ignored later request.
    pub(crate) fn begin(&mut self, request: DrainRequest) -> DrainDecision {
        match self.state {
            DrainState::Open => self.begin_first(request),
            DrainState::WaitingForExactWrite { first_request, .. }
                if request.requires_immediate_close() =>
            {
                self.state = DrainState::TransportCloseRequested { first_request };
                DrainDecision::RequestTransportClose {
                    reason: first_request.reason(),
                    trigger: TransportCloseTrigger::EscalatingRequest,
                }
            }
            state @ (DrainState::WaitingForExactWrite { .. }
            | DrainState::TransportCloseRequested { .. }
            | DrainState::Closed { .. }) => DrainDecision::Ignored {
                input: IgnoredDrainInput::AdditionalRequest { state },
            },
        }
    }

    /// Handles one exact scheduler or writer terminal for `write_id`.
    ///
    /// Only the `WriteId` frozen in the first request releases a waiting
    /// barrier. Semantic operation completion is intentionally not an input to
    /// this coordinator. Wrong, stale, and duplicate write terminals preserve
    /// state and never emit another transport-close request.
    ///
    /// Returns the unique close request for the exact terminal, or a structured
    /// ignored classification for every other terminal.
    pub(crate) fn on_write_terminal(&mut self, write_id: WriteId) -> DrainDecision {
        let DrainState::WaitingForExactWrite {
            first_request,
            barrier,
        } = self.state
        else {
            return DrainDecision::Ignored {
                input: IgnoredDrainInput::WriteTerminalWithoutBarrier { state: self.state },
            };
        };

        if write_id != barrier.write_id() {
            return DrainDecision::Ignored {
                input: IgnoredDrainInput::WrongWriteTerminal {
                    expected: barrier.write_id(),
                    actual: write_id,
                },
            };
        }

        self.state = DrainState::TransportCloseRequested { first_request };
        DrainDecision::RequestTransportClose {
            reason: first_request.reason(),
            trigger: TransportCloseTrigger::ExactWriteTerminal,
        }
    }

    /// Handles runtime completion of the transport-close request.
    ///
    /// Completion advances only `TransportCloseRequested` to `Closed`.
    /// Premature or duplicate completion leaves state unchanged and produces no
    /// repeated runtime action.
    ///
    /// Returns a terminal completion decision or a structured ignored
    /// classification.
    pub(crate) fn on_transport_close_completed(&mut self) -> DrainDecision {
        match self.state {
            DrainState::TransportCloseRequested { first_request } => {
                self.state = DrainState::Closed { first_request };
                DrainDecision::TransportCloseCompleted {
                    reason: first_request.reason(),
                }
            }
            state @ (DrainState::Open | DrainState::WaitingForExactWrite { .. }) => {
                DrainDecision::Ignored {
                    input: IgnoredDrainInput::PrematureTransportCloseCompletion { state },
                }
            }
            DrainState::Closed { .. } => DrainDecision::Ignored {
                input: IgnoredDrainInput::DuplicateTransportCloseCompletion,
            },
        }
    }

    /// Commits the first request from the open state.
    ///
    /// Immediate and fatal requests transition directly to a unique close
    /// request. A valid local-Separate write barrier transitions to waiting.
    ///
    /// Returns the first externally meaningful drain decision.
    fn begin_first(&mut self, request: DrainRequest) -> DrainDecision {
        if request.requires_immediate_close() {
            self.state = DrainState::TransportCloseRequested {
                first_request: request,
            };
            return DrainDecision::RequestTransportClose {
                reason: request.reason(),
                trigger: TransportCloseTrigger::InitialImmediateRequest,
            };
        }

        let ResolvedCloseBarrier::AfterWrite(barrier) = request.barrier() else {
            unreachable!("non-immediate requests always carry an exact-write barrier");
        };
        self.state = DrainState::WaitingForExactWrite {
            first_request: request,
            barrier,
        };
        DrainDecision::WaitForExactWrite {
            reason: request.reason(),
            barrier,
        }
    }
}

impl Default for DrainCoordinator {
    /// Creates the default open coordinator without allocating resources.
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        core::drain::{
            DrainCoordinator, DrainDecision, DrainRequest, DrainState, IgnoredDrainInput,
            ResolvedCloseBarrier, TransportCloseTrigger, WriteBarrier,
        },
        model::{
            ids::{OperationId, WriteId},
            runtime::GenerationCloseReason,
        },
    };

    /// Creates a deterministic operation identifier for focused state tests.
    const fn operation(value: u64) -> OperationId {
        OperationId::new(value)
    }

    /// Creates a deterministic write identifier for focused state tests.
    const fn write(value: u64) -> WriteId {
        WriteId::new(value)
    }

    /// Creates an exact operation-to-write barrier for focused state tests.
    const fn barrier(operation_value: u64, write_value: u64) -> WriteBarrier {
        WriteBarrier::new(operation(operation_value), write(write_value))
    }

    /// Creates a local-Separate request that waits for an exact write.
    const fn after_write(
        reason: GenerationCloseReason,
        operation_value: u64,
        write_value: u64,
    ) -> DrainRequest {
        DrainRequest::new(
            reason,
            ResolvedCloseBarrier::AfterWrite(barrier(operation_value, write_value)),
        )
    }

    /// Creates a close request with no outbound-write boundary.
    const fn immediate(reason: GenerationCloseReason) -> DrainRequest {
        DrainRequest::new(reason, ResolvedCloseBarrier::Immediate)
    }

    /// Confirms an immediate first request emits one close request and repeats
    /// cannot emit a second request.
    #[test]
    fn immediate_request_emits_exactly_one_transport_close_request() {
        let request = immediate(GenerationCloseReason::LocalDisconnect);
        let mut coordinator = DrainCoordinator::new();

        assert_eq!(
            coordinator.begin(request),
            DrainDecision::RequestTransportClose {
                reason: GenerationCloseReason::LocalDisconnect,
                trigger: TransportCloseTrigger::InitialImmediateRequest,
            }
        );
        let requested_state = coordinator.state();
        assert_eq!(
            coordinator.begin(immediate(GenerationCloseReason::ProtocolViolation)),
            DrainDecision::Ignored {
                input: IgnoredDrainInput::AdditionalRequest {
                    state: requested_state,
                },
            }
        );
        assert_eq!(coordinator.state(), requested_state);
    }

    /// Confirms only the exact frozen write terminal releases a waiting close.
    #[test]
    fn exact_write_terminal_releases_the_frozen_barrier() {
        let request = after_write(GenerationCloseReason::LocalSeparate, 3, 5);
        let mut coordinator = DrainCoordinator::new();

        assert_eq!(
            coordinator.begin(request),
            DrainDecision::WaitForExactWrite {
                reason: GenerationCloseReason::LocalSeparate,
                barrier: barrier(3, 5),
            }
        );
        assert_eq!(
            coordinator.on_write_terminal(write(5)),
            DrainDecision::RequestTransportClose {
                reason: GenerationCloseReason::LocalSeparate,
                trigger: TransportCloseTrigger::ExactWriteTerminal,
            }
        );
        assert!(matches!(
            coordinator.state(),
            DrainState::TransportCloseRequested {
                first_request
            } if first_request == request
        ));
    }

    /// Confirms operation identity alone cannot release or replace the exact
    /// write barrier because the coordinator has no operation-terminal input.
    #[test]
    fn operation_identity_cannot_release_or_replace_write_barrier() {
        let first = after_write(GenerationCloseReason::LocalSeparate, 7, 11);
        let same_operation_other_write = after_write(GenerationCloseReason::LocalSeparate, 7, 12);
        let mut coordinator = DrainCoordinator::new();
        let _ = coordinator.begin(first);
        let waiting_state = coordinator.state();

        assert_eq!(
            coordinator.begin(same_operation_other_write),
            DrainDecision::Ignored {
                input: IgnoredDrainInput::AdditionalRequest {
                    state: waiting_state,
                },
            }
        );
        assert_eq!(coordinator.state(), waiting_state);
        assert_eq!(
            coordinator.state().first_request(),
            Some(first),
            "the first operation-to-write mapping must remain frozen"
        );
    }

    /// Confirms wrong and post-release write terminals are zero-mutation inputs.
    #[test]
    fn wrong_stale_and_duplicate_write_terminals_do_not_mutate_state() {
        let mut coordinator = DrainCoordinator::new();
        let open_state = coordinator.state();
        assert_eq!(
            coordinator.on_write_terminal(write(1)),
            DrainDecision::Ignored {
                input: IgnoredDrainInput::WriteTerminalWithoutBarrier { state: open_state },
            }
        );
        assert_eq!(coordinator.state(), open_state);

        let _ = coordinator.begin(after_write(GenerationCloseReason::LocalSeparate, 13, 17));
        let waiting_state = coordinator.state();
        assert_eq!(
            coordinator.on_write_terminal(write(16)),
            DrainDecision::Ignored {
                input: IgnoredDrainInput::WrongWriteTerminal {
                    expected: write(17),
                    actual: write(16),
                },
            }
        );
        assert_eq!(coordinator.state(), waiting_state);

        let _ = coordinator.on_write_terminal(write(17));
        let requested_state = coordinator.state();
        assert_eq!(
            coordinator.on_write_terminal(write(17)),
            DrainDecision::Ignored {
                input: IgnoredDrainInput::WriteTerminalWithoutBarrier {
                    state: requested_state,
                },
            }
        );
        assert_eq!(coordinator.state(), requested_state);
    }

    /// Confirms a later immediate request breaks a stalled write wait while
    /// retaining the first request's close reason.
    #[test]
    fn later_immediate_request_escalates_wait_with_first_reason() {
        let first = after_write(GenerationCloseReason::LocalSeparate, 19, 23);
        let mut coordinator = DrainCoordinator::new();
        let _ = coordinator.begin(first);

        assert_eq!(
            coordinator.begin(immediate(GenerationCloseReason::LocalStop)),
            DrainDecision::RequestTransportClose {
                reason: GenerationCloseReason::LocalSeparate,
                trigger: TransportCloseTrigger::EscalatingRequest,
            }
        );
        assert_eq!(coordinator.state().first_request(), Some(first));
    }

    /// Confirms a fatal reason escalates waiting even if its caller supplied
    /// another write barrier instead of the immediate barrier.
    #[test]
    fn later_fatal_request_escalates_even_with_after_write_barrier() {
        let first = after_write(GenerationCloseReason::LocalSeparate, 29, 31);
        let mut coordinator = DrainCoordinator::new();
        let _ = coordinator.begin(first);

        assert_eq!(
            coordinator.begin(after_write(
                GenerationCloseReason::ProtocolViolation,
                37,
                41,
            )),
            DrainDecision::RequestTransportClose {
                reason: GenerationCloseReason::LocalSeparate,
                trigger: TransportCloseTrigger::EscalatingRequest,
            }
        );
        assert_eq!(coordinator.state().first_request(), Some(first));
    }

    /// Confirms a second non-urgent barrier cannot replace the first exact
    /// operation or write identity.
    #[test]
    fn later_after_write_request_does_not_replace_first_barrier() {
        let first = after_write(GenerationCloseReason::LocalSeparate, 43, 47);
        let mut coordinator = DrainCoordinator::new();
        let _ = coordinator.begin(first);
        let waiting_state = coordinator.state();

        assert_eq!(
            coordinator.begin(after_write(GenerationCloseReason::LocalSeparate, 53, 59,)),
            DrainDecision::Ignored {
                input: IgnoredDrainInput::AdditionalRequest {
                    state: waiting_state,
                },
            }
        );
        assert_eq!(coordinator.state(), waiting_state);
        assert_eq!(
            coordinator.on_write_terminal(write(59)),
            DrainDecision::Ignored {
                input: IgnoredDrainInput::WrongWriteTerminal {
                    expected: write(47),
                    actual: write(59),
                },
            }
        );
    }

    /// Confirms only an emitted close request may complete and repeated
    /// completion is classified without repeated terminal action.
    #[test]
    fn close_completion_requires_unique_request_and_is_idempotent() {
        let mut coordinator = DrainCoordinator::new();
        let open_state = coordinator.state();
        assert_eq!(
            coordinator.on_transport_close_completed(),
            DrainDecision::Ignored {
                input: IgnoredDrainInput::PrematureTransportCloseCompletion { state: open_state },
            }
        );
        assert_eq!(coordinator.state(), open_state);

        let first = after_write(GenerationCloseReason::LocalSeparate, 61, 67);
        let _ = coordinator.begin(first);
        let waiting_state = coordinator.state();
        assert_eq!(
            coordinator.on_transport_close_completed(),
            DrainDecision::Ignored {
                input: IgnoredDrainInput::PrematureTransportCloseCompletion {
                    state: waiting_state,
                },
            }
        );
        assert_eq!(coordinator.state(), waiting_state);

        let _ = coordinator.on_write_terminal(write(67));
        assert_eq!(
            coordinator.on_transport_close_completed(),
            DrainDecision::TransportCloseCompleted {
                reason: GenerationCloseReason::LocalSeparate,
            }
        );
        let closed_state = coordinator.state();
        assert_eq!(
            coordinator.on_transport_close_completed(),
            DrainDecision::Ignored {
                input: IgnoredDrainInput::DuplicateTransportCloseCompletion,
            }
        );
        assert_eq!(coordinator.state(), closed_state);
    }
}
