# Local standards trace

This traces the implemented baseline subset; it is not certification. Sources are the
local `document/SEMI E37-0298 HIGH-SPEED SECS MESSAGE SERVICES (HSMS) GENERIC.pdf`
and `document/SEMI E5-0301 SEMI EQUIPMENT COMMUNICATIONS STANDARD 2 MESSAGE CONTENT.pdf`.
They remain local reference documents and are excluded from the Cargo package.
No verified E37.1 edition is present in this audit.

| Baseline clause | Requirement paraphrase | Implementation and evidence | Status |
|---|---|---|---|
| E37-0298 9.2.1, p.15 | After an active connect attempt ends, wait T5 before beginning another. Attempt duration is additional to T5. | `supervisor/connection.rs`: `ConnectSeparation` records completion time on every exit, including dropped futures. Tests `t5_begins_after_attempt_termination`, `cancelled_connect_attempt_starts_a_full_t5_interval`, and `active_attempt_spacing_survives_cancelled_wait`. | Corrected from start-to-start timing; tests pass. |
| E37-0298 9.2.2, p.15 | A single-connection entity limits uninterrupted NotSelected time with T7. | `core/session/timing.rs`; `t7_is_not_refreshed_by_linktest_requests`; endpoint `due_t7_precedes_queued_endpoint_stop`; successful Deselect test re-arms T7 at the transition. | Continuous tenure and endpoint priority assertions inspected; passing suite evidence recorded in ACCEPTANCE.md. |
| E37-0298 9.2.3 and 10.1, pp.15/18 | T8 limits the gap between successive bytes of an incomplete message; expiry is a communication failure. | `FrameReader::next` updates `last_progress` only after positive reads; `partial_prefix_and_body_have_t8`, `idle_reader_does_not_start_t8`, `byte_progress_refreshes_t8`, `dropped_read_future_retains_partial_frame`. | Partial prefixes/bodies, idle exclusion, per-byte refresh and cancelled waits inspected; interpretation follows the explicit intercharacter description in the parameter table. |
| E37-0298 9.2.4.1(c), pp.15-16 | A single-connection Passive entity may stop listening/accepting extra connections and documents its procedure. | `ConnectionSource::next` releases its listener before yielding a candidate; only cleanup-gated Supervisor connect can rebind the same address. Public tests attempt extra connections while Linktest continues and verify later replacement. | Stop-listening policy implemented and tested on Windows; OS refusal versus timeout is not guaranteed. |
| E37-0298 9.3.1, p.16 | An unanswered Control transaction expires under T6 and causes communication failure. | `select_t6_uses_actual_commit_time`, `idle_probe_uses_control_slot_and_times_out`, `mismatched_response_keeps_deselect_transaction_until_t6`; actual completion timestamps flow through Driver. | Select, Linktest and Deselect timer paths mapped. |
| E37-0298 9.4.1 and 9.4.1.1, pp.16-17 | Match Session ID, Stream, F+1 or F0 and System Bytes; W=1 requests have T3. | `ResponseContract::classify`; `normal_match_requires_full_tuple_and_w_false` varies each field independently; `abort_is_header_only_and_f255_is_abort_only`; `t3_boundary_releases_transaction_and_retains_tombstone`, `delayed_commit_does_not_extend_t3`, `fast_secondary_then_late_commit_has_no_timer`. | Matching and actual-commit timer paths inspected; tests prove mismatches do not become successful responses. |
| E5-0301 6.2/6.2.1, pp.11-12 | Item headers use one to three length bytes; body length excludes header. Zero length-byte count is invalid. | `LengthByteCount::for_declared_length` selects 1/2/3 across 255/65535/16777215; decoder `read_header` separates upper six format bits and lower two count bits and accumulates length big-endian. Tests `length_byte_boundaries_round_trip_through_exact_lengths`, `zero_length_byte_count_is_rejected`. | Byte extraction, representable boundaries and independent vector coverage inspected. |
| E5-0301 6.3, p.12 | List length counts child elements. | Test `list_length_byte_count_is_selected_from_child_count_not_byte_total`; decoder List frames retain expected child count and validate projected nodes before growing child storage. | List semantics and configured resource guards inspected; guard values are local policy. |
| E5-0301 6.4, p.12 | Localized string includes a two-byte encoding header in its body length. | Tests `localized_utf8_fixture_round_trips_with_lsh_preserved`, `missing_localized_lsh_is_rejected`; `LocalizedString`. | Header length and payload preservation inspected. Code zero is rejected; all other codes, including future-reserved 15–32767, are preserved without registry validation/transcoding. This documented forward-compatibility policy is not registry conformance. |
| E5-0301 6.5, p.13 | Examples demonstrate binary, ASCII, integer and float layouts. | Visually compared the rendered page with fixtures: a=`21 01 AA`, b=`41 03 41 42 43`, c header=`69 06`, d header=`91 04`. Standard c/d payloads are placeholders, instantiated by this test suite. Named decoder tests plus `standard_example_layouts_encode_to_independent_vectors` exercise both directions. | Printed layouts and independent fixture bytes verified; no claim that c/d chosen values appear in the standard. |
| E37-0298 8.1.4.5/8.1.4.6 and 8.2, p.11 | SType table defines Data/Select/Deselect/Linktest/Reject/Separate; replies preserve correlation; fixed Control layouts have no text. | Rendered SType and message-summary tables checked against `StrictFrameValidator::validate` and `validate_control`; `unknown_stype_is_classified_before_ptype`, `control_message_text_has_a_stable_violation`, `select_response_preserves_status_and_system_bytes`, `linktest_requires_control_session_id`; `typed_controls_encode_without_raw_header_state`. | Inspected SType list and summary rows agree; per-type and procedural mappings are recorded below. |

SML syntax and local resource/shutdown policies are library contracts, not E37/E5
normative requirements. Control procedures and supported-subset boundaries are
mapped below. Platform test evidence is for this Windows workspace; it does not
certify interoperability with every device, operating system or standards edition.

## Control procedure mapping

The following mappings were checked against complete relevant text on E37 pages
7-9 and 13-14, the implementation branches and test assertions. Test names refer
to the all-feature suite recorded in ACCEPTANCE.md.

| Clauses | Implemented behavior | Direct evidence |
|---|---|---|
| 7.2.1, 8.2.3 | Matched status 0 selects; nonzero status fails without a new state transition and preserves the raw status. | `receive_select_response`; `active_select_success_orders_state_before_completion`, `active_select_rejection_preserves_raw_status`, `select_response_preserves_status_and_system_bytes`. |
| 7.2.2, 8.2.2 | Respond with echoed Session ID/System Bytes and a readiness status; publish Selected only after response admission. | `receive_select_request`; `passive_select_copies_tuple_and_orders_response_before_state`, Driver `passive_select_admission_failure_closes_without_selected_observation`. Control Session ID 0xFFFF is the library's single-session policy, not a generic E37-wide restriction. |
| 7.2.3 | A simultaneous inbound Select does not consume the local Select transaction or publish Selected twice. | `simultaneous_select_retains_local_transaction_and_publishes_selected_once`. |
| 7.4.1, 8.2.4-8.2.5 | Successful Deselect enters NotSelected and retains TCP; nonzero status preserves current state. | `receive_deselect_response`; `successful_deselect_keeps_connection_and_arms_t7_from_transition`, `rejected_deselect_preserves_status_and_selected_state`. |
| 7.4.2 | Peer Deselect returns success when idle, not-established when not selected, and busy for active Data/reply ownership. | `receive_deselect_request`, `data_in_use`; `peer_deselect_busy_preserves_reply_authority`; response Session ID and System Bytes copied from request. |
| 7.4.3 | Accept simultaneous peer Deselect; a later unsuccessful local response cannot restore Selected. | `simultaneous_deselect_failure_does_not_restore_selected_or_reset_t7`; assertions preserve the first NotSelected/T7 transition. |
| 7.5, 8.2.6-8.2.7 | Linktest works in both connected substates, uses 0xFFFF and echoed System Bytes, and peer requests do not occupy the local transaction slot. | `linktest_works_in_both_states_and_unmatched_response_is_rejected`, `peer_linktest_request_bypasses_local_control_slot`, `linktest_requires_control_session_id`. |
| 7.6, 7.6.1-7.6.2 | Local Separate preempts protocol work and waits for its local write barrier; peer Separate sends no response and closes Selected. In NotSelected it is ignored. | `local_separate_orders_preemption_send_state_and_barrier`, `separate_barrier_completes_from_all_terminal_outcomes`, `peer_separate_transitions_selected_and_aborts_local_transaction`. The specific 7.6.2 step 3 is followed for NotSelected; the introductory paragraph has broader wording. |
| 7.3, 7.7.2, 8.2.8 | Data in NotSelected, unsupported SType/PType, and unmatched Control responses produce the appropriate Reject; header byte 2 depends on the reason. | `malformed_header_priority_and_reject_preserve_original_context`, `unmatched_select_response_emits_reject_and_preserves_transaction`, `unmatched_linktest_response_copies_tuple_into_reject`, `mismatched_response_keeps_deselect_transaction_until_t6`; Validator classifies SType before PType. |
| 7.7.1 | Receiving Reject is a local-policy decision: only unambiguous matching work is failed; unsupported extension and ambiguous references leave live work intact. | `receive_reject`; `reject_attribution_uses_reason_specific_header_semantics`, `ambiguous_reject_classification_preserves_live_commands`, `unknown_extension_reject_is_trace_only`. This attribution policy is not claimed as the sole standard-permitted response. |

All Control messages use a ten-byte header with no Message Text. Encoder and
Validator operate separately from selection/transaction policy. No GEM state
machine or automatic business-level S9 behavior is claimed by these mappings.

## Data format and parameter documentation

E37 pages 10, 12 and 17-18 were read directly for these mappings:

| Clauses | Implementation contract and evidence |
|---|---|
| 8.1.2-8.1.4 | `HsmsFrameDecoder::decode` reads a four-byte big-endian length, enforces minimum 10 and configured maximum, and frames exactly header plus text. `HsmsWireEncoder` uses the same envelope convention; fixed wire tests and `bad_length_is_terminal_before_body_read` exercise the boundary independently. |
| 8.2.1 | `validate_data` extracts W from bit 7, Stream from bits 0-6 and Function from header byte 3. Core enforces odd Primary/even Secondary and outgoing request/send W policy. Public endpoint Data tests inspect independent wire bytes. Header-only text is distinct from typed empty SECS-II items (`absence_and_typed_empty_items_remain_distinct`). |
| 9.4.2 | Inbound/Secondary `MessageContext` and T3 `RequestTimeout` retain the ten HSMS header bytes for application-owned MHEAD/SHEAD construction. The library does not automatically send S9 business messages. |
| 10 and 10.1 | USAGE.md documents constructors, validation, role/address setup, all default timer values, supported baseline timer ranges, clock/scheduling limits, send/receive size bounds, concurrent transaction limits and Passive refusal policy. Configuration persistence is the embedding application's responsibility; this library performs no configuration-file I/O. |

The generic E37 association of Data and Control Session IDs is refined by local
single-session policy (Control 0xFFFF and one configured Data ID). It is not evidence
for an unverified E37.1 edition. E5 support here is item representation/encoding and
generic stream/function messages; message-specific equipment/host business schemas
and GEM behavior remain outside the agreed protocol-library scope.
