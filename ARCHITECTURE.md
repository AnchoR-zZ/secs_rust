# SECS protocol library implementation contract

Status: the agreed reusable protocol-library implementation and contract audit
are verified. This document records the delivered contract; executable evidence
and supported-scope qualifications are recorded in ACCEPTANCE.md.

The current requirement-by-requirement audit is in [ACCEPTANCE.md](ACCEPTANCE.md).
Standards evidence and scope are in [STANDARD_TRACE.md](STANDARD_TRACE.md).
Chronological notes are preserved locally in docs/hsms-implementation-history.md;
their intermediate status claims are not the current implementation status.

## Scope and ownership

The deliverable is a reusable SECS-II/SML/HSMS library with a real Tokio TCP
runtime, typed public endpoint API, transaction timers, inbound reply capabilities,
bounded resources, deterministic shutdown and supervised reconnection. Migrating
the parent simulator/GEM application is outside this library's scope.

- SECS-II owns stream/function/message content and the strict binary codec.
- SML depends on the SECS-II value model, never the HSMS runtime.
- SessionCore is synchronous and I/O-free. It owns selection, transactions,
  matching, reply capabilities and T3/T6/T7 deadlines.
- One SessionDriver serializes a generation's inputs and applies each complete
  ordered action batch without awaiting inside the batch.
- Reader owns framing progress and T8; Writer owns FIFO order and actual write
  progress. Control capacity is reserved independently from Data capacity.
- Supervisor owns endpoint intent, T5, generation routing and replacement.
- Tokio is an optional runtime dependency. Codec users need no async runtime.

## Corrections incorporated from the architecture review

1. Shutdown closes admission before settlement. Unresolved writes retain their
   completion correlation until an actual outcome or conservative finalization.
   `NotWritten`, `Committed` and `Indeterminate` are distinct. Resource cleanup
   proof is independent of delivery certainty. Never retry an accepted command
   automatically on a replacement generation.
2. Bound command/event/frame counts and bytes. Bound queue residence as well as
   active writes. Graceful shutdown has an absolute deadline; a tail Separate
   cannot extend shutdown indefinitely behind queued Data.
3. Expose immutable inbound/transaction header context for correlation and S9
   construction. This does not allow applications to choose outbound headers or
   System Bytes. Protocol-relevant errors use reliable delivery; lossy diagnostic
   observations must be explicitly identified and counted.
4. Share pure message values across API and Core. Keep runtime capabilities
   separate from message content and do not multiply equivalent DTOs.
5. Exhaustion of 32-bit System Bytes is an expected lifecycle condition. Use
   controlled generation rotation, not identifier wrap or RuntimeInvariant.
6. Snapshot/watch retains the latest consistent endpoint state. Primary delivery
   has a separate bounded single-consumer channel. Drop of a completion receiver
   does not revoke accepted protocol work. Reply tokens are single-use and tied
   to endpoint owner and generation.
7. Preserve the local E37-0298/E5-0301 baseline and record clause/test mappings.
   E37.1 subsidiary requirements need explicit version verification before a
   complete HSMS-SS conformance claim; no unsupported conformance claim is made.

## Timing and failure semantics

T3 and T6 start at actual local full-write completion, not callback processing
time. Input envelopes carry occurrence time separately from monotonic processing
time. A delayed write outcome may immediately expire a deadline in the same turn.
A fast matched response ends its request once; a later commit does not resurrect
the timer. T3 retires only its transaction. T6/T7 end the generation. T7 covers a
continuous NotSelected tenure. T8 covers partial prefixes and partial bodies,
and is reset only by actual byte progress.

Each source is FIFO. At each turn visible terminal inputs precede due deadlines;
ordinary reader/writer/command inputs are serviced fairly. Response/deadline ties
use processing-time deadline priority, with no retrospective network-time replay.
Cleanup continues to consume writer outcomes while joining tasks.

## Completion evidence required

- [x] Shared protocol message model and independently usable pure codecs.
- [x] Core T3/T6/T7, autonomous Linktest and deterministic timer boundary tests.
- [x] Complete inbound classification, reliable error context and reply/abort/abandon.
- [x] Deselect and simultaneous control procedures with finite drain behavior.
- [x] Real bounded Reader/Writer with T8, byte budgets, queue/write deadlines.
- [x] Public Handle/Runtime, snapshots/events, cancellation and typed failures.
- [x] Active/Passive supervisor, T5, generation isolation and identifier rotation.
- [x] Correct settlement, bounded cleanup, Clean/Poisoned and recovery behavior.
- [x] Independent-wire integration tests and runnable public-API loopback examples.
- [x] Fault injection, malformed input, slow consumers, capacity and timer races.
- [x] Remove temporary dead-code exemptions; format/check/clippy/test/doc/release gates.
- [x] Public documentation and requirement-by-requirement final audit.

The existing B1/B2 fake slices are a behavioral foundation, not the final runtime.
Stage-specific tests that asserted an absence of timers or delivery must evolve
when those features are implemented. Existing ordering, matching and ownership
invariants must remain covered.

## Current verified implementation evidence

While a generation owns the Passive endpoint, the listening socket is released
under E37-0298 9.2.4.1(c). The source retains the actual bind address and resumes
listening only when clean retirement permits the next connect call. A public TCP
test attempts sixteen extra connections while the original session answers sixteen
Linktests, then disconnects and selects a fresh replacement. Existing supervisor
round-robin and source error propagation remain; a retained listener in an occupied
slot triggers that error/cleanup path instead of silently accepting extra work.

Stop and Disconnect share one independently reserved endpoint command slot,
retained until close completion. The endpoint queue is bounded by ordinary
command_capacity plus reply_capacity plus one; ordinary operations cannot consume the close reserve.
Public tests verify admission while the ordinary queue is full, rejection of a
second concurrent close, reservation release, and bounded TCP shutdown while the
sole ordinary slot is held by an unanswered request. Reply admission now has
independent count/byte budgets configured by RuntimePolicy::with_reply_budget.
A real TCP test exhausts ordinary count and bytes with an unanswered request,
then completes Reply, Abort and Abandon before matching that original request.
Writer congestion remains bounded and may reject replies without consuming inputs.

Reader wire reservations and the application Primary queue now expose independent
budgets (`inbound_bytes` and `primary_queue_bytes`). Sharing a single reservation
would let an unconsumed Primary prevent reading transaction responses. A TCP test
fills queue bytes before a matched Secondary, verifies successful completion,
consumes and reuses capacity, then verifies byte-only overflow preserves the older
event and closes. Encoded charges do not claim to cap exact heap/RSS: decoded-tree
allocations, retained buffer capacity, metadata and caller-owned results are separate.

The following test and Clippy gates were revalidated after dead-code cleanup and
the pre-Core Primary ownership fix. Production source no longer contains module-level dead-code
exemptions. Unused legacy ApplicationEventPort, generic EndpointEvent/envelope and
SessionLauncher abstractions were removed; actual receiver/diagnostic/watch APIs
remain the supported application boundaries. Existing deterministic test helpers
and reference codec encoding are compiled only in tests. Pre-Core Writer reservation
failure and queued-command shutdown return the original Primary through the public
MessageError path. Tests verify both Send/Request reservation failures preserve the
original body allocation; post-Core delivery uncertainty remains a distinct result.

- `cargo test -p secs_rust --all-features --offline --target-dir target --quiet`:
  509 unit, 109 integration and 2 documentation tests passed (620 total).
- `cargo test -p secs_rust --no-default-features --offline --target-dir target --quiet`:
  447 unit, 84 integration and 2 documentation tests passed (533 total).
- `cargo clippy -p secs_rust --all-features --all-targets --offline --target-dir target -- -D warnings` passed.
- `cargo rustdoc -p secs_rust --all-features --offline --target-dir target --lib -- -D warnings` passed.
  Production module-level dead-code exemptions have been removed. The contract
  audit maps implementation paths and fault tests separately from build gates.
- Driver carries actual write occurrence separately from processing time. Its
  ordinary reader/command inputs apply due deadlines first. Endpoint dispatch
  prioritizes visible terminal facts and due deadlines; contention tests and
  persistent round-robin evidence are recorded in ACCEPTANCE.md.
