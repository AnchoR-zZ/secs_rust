# Current acceptance audit

Status: the agreed reusable library implementation and local delivery audit are
complete. This record separates executable gates from requirement-level evidence
against ARCHITECTURE.md and the local Stage C/D/E contract.

## Verified executable gates

Final source verification on 2026-09-10 in the library worktree:

- All-feature tests: 509 unit + 109 integration + 2 doc tests, all passed (620 total).
- No-default-feature tests: 447 unit + 84 integration + 2 doc tests, all passed.
- Strict Clippy for both feature configurations and all targets passed.
- Explicit `cargo check --all-targets` passed in both feature configurations.
- `cargo fmt -- --check` and `git diff --check` passed; no `todo!`,
  `unimplemented!` or source-level dead-code allowances remain in `src`.
- All-feature rustdoc with warnings denied passed.
- Release library builds with all features and no default features passed.
- `loopback` ran two public TCP endpoints through request/reply, Stop and task join.
- `codec` ran without default features and round-tripped a 14-byte SECS-II value.
- Pure-feature rustdoc with warnings denied passed.
- Local `cargo package --allow-dirty --all-features --offline` verified compilation
  from the package. The 98-entry inventory excludes agent skills, editor settings
  and local standards PDFs using an explicit Cargo include list. Cargo notes that
  optional documentation/homepage/repository metadata is not configured.

Commands use `-p secs_rust --offline --target-dir target`; parent simulator/GEM
migration is outside scope. No package has been published or pushed.

## Contract evidence

The direct PDF audit is tracked in STANDARD_TRACE.md. It exposed and corrected
T5 start-to-start timing: the local E37 baseline requires end-to-start spacing.
Passive now uses the enumerated stop-listening policy while occupied, resuming
the same address only after cleanup permits another connection attempt.

| Requirement | Current authoritative sources | Verified evidence and qualifications |
|---|---|---|
| Strict SECS-II/SML and runtime independence | `src/secs2`, `src/sml`, `tests/secs2_conformance.rs`, `tests/sml_conformance.rs`, pure release build and codec example | STANDARD_TRACE.md maps the local E37/E5 encoding and transport subset. Independent fixed vectors, malformed/truncated cases and SML canonical/strict tests pass. Normal no-default-feature dependency tree contains no Tokio. |
| T3/T6 commit time; T7 tenure; bounded Deselect | `src/hsms/core/session/timing.rs`, `timing/tests.rs`, `deselect.rs`, Driver tests | STANDARD_TRACE.md records actual-commit deadlines, fast responses, T7 tenure and simultaneous Deselect assertions. Endpoint T7/Stop priority is tested separately. |
| T8 and finite Writer progress | `src/hsms/generation/transport/io.rs`, `bounded_reader.rs`, `bounded_writer.rs`, runtime tests `absolute_shutdown_bounds_partial_data_and_separate_writes` and `full_control_lane_rejects_tail_separate_and_still_cleans_up` | Partial Data/Separate stalls meet the exact total deadline. Full Control-lane tail rejection releases its barrier, preserves LocalStop, sends no rejected frame and cleans up. These named production-path tests form the requirement trace. |
| FIFO and terminal/deadline priority | `src/hsms/generation/transport/tasks.rs`, `src/hsms/generation/runtime.rs`, `src/hsms/scheduling.rs`, endpoint runtime priority tests | Endpoint receive processes visible terminal facts and due deadlines before commands. T7/Stop and actual Reader EOF/framing-fault versus Stop precedence are verified. Writer test proves terminal priority, zero/partial delivery distinction and Clean cleanup through the same priority pass. Persistent round-robin, cancellation and simultaneous-ready inputs are tested; TCP exercises 16 excess candidates concurrently with 16 Linktests. These named production-path tests form the requirement trace. |
| Delivery settlement and resource proof | `src/hsms/core/session.rs`, `src/hsms/generation/runtime.rs`, `transport/tasks.rs`, `supervisor/runtime.rs` | Composed stalled-write test proves exact deadline, Poisoned at expired cleanup, explicit recovery to Clean, original LocalStop reason and DeliveryIndeterminate for partial Data. Committed, not-written and missing-fact cases are additionally covered in Driver settlement tests. |
| Public ownership and bounded admission | `src/hsms/endpoint`, `tests/endpoint_data.rs`, `tests/endpoint_inbound.rs`, `tests/endpoint_lifecycle.rs` | Independent reply count/byte exhaustion has direct TCP evidence: rejected tokens retry, dropping the completion receiver retains accepted work/charges, completion releases capacity, and both replies retain peer correlation. Receiver-drop test verifies independent streams, ApplicationBackpressure only when closed-stream delivery is required, Clean task cleanup and explicit Stop recovery. These named production-path tests form the requirement trace. |
| Active/Passive replacement | `src/hsms/supervisor`, `tests/endpoint_lifecycle.rs`, endpoint test `passive_rebind_failure_faults_after_old_generation_is_clean` | Passive listener is absent for the entire owned generation, including drain/cleanup; only empty-slot connect can rebind. Occupied/poisoned-slot tests prevent premature replacement. Rebind-failure test preserves old Clean evidence, reports failure, faults without a new generation, refuses Start and verifies explicit Stop recovery. These named production-path tests form the requirement trace. |
| Reliable errors and best-effort diagnostics | `src/hsms/api/violation.rs`, `src/hsms/endpoint/diagnostics.rs`, `tests/endpoint_diagnostics.rs` | Automatic and explicit Select Reject attribution is verified by TCP test; public docs clarify command-backed OperationRejected versus Core-owned AutonomousRejected. Reliable and lossy delivery contracts are independently tested. |
| Bounded memory | RuntimePolicy, Reader/Writer permits, Delivery and DecodeLimits | USAGE.md documents per-stage count and byte limits, defaults, aggregate admission formula and decoded-tree/caller-owned overhead. Independent Reader/application budgets and reply reserves are tested. These limits intentionally do not claim an exact heap or RSS cap. |
| Reusable public delivery | README, USAGE, examples, Cargo.toml | Both feature modes pass checks/tests/docs/release; examples run and package verification compiles the extracted source. Obsolete production comments were reviewed, historical implementation notes archived locally, and package-facing links point to included current documents. |

Latest-value reliability is also verified independently of best-effort diagnostics:
`EndpointStateSnapshot::last_exit()` retains generation, original reason and cleanup
status across Stop/Start. The diagnostic saturation test proves the newest report
survives dropped records; `last_exit_retains_poison_and_updates_after_explicit_recovery`
proves false-to-true cleanup updates require a fresh proof for the same generation.
Runtime Drop preserves prior evidence and does not fabricate a Clean report.

## Delivery scope

The delivered scope is the reusable SECS-II item/message codec, SML and single-session
HSMS runtime described in ARCHITECTURE.md. GEM/message business schemas, parent
simulator migration, publication and external device certification are not included.
No verified E37.1 edition was supplied, so this audit makes no full HSMS-SS standards
certification claim. Platform execution evidence is Windows/Tokio in this workspace.
These are explicit scope qualifications, not unimplemented behavior promised by the
public API. No implementation or local delivery gate remains open in this scope.
