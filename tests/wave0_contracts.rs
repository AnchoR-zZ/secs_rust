//! Integration-level checks for the Wave 0 public values and frozen semantics.
//!
//! These tests intentionally construct no async runtime or TCP connection; they
//! prove that application-facing contracts validate and compile independently.

#[path = "fixtures/mod.rs"]
mod fixtures;

use std::net::SocketAddr;

use secs_rust::{
    AsciiString, ConfigError, ConnectionMode, ControlIntent, DecodeLimits, EndpointConfig,
    EndpointLimits, EndpointPhase, EndpointStateSnapshot, Function, GenerationSlotSnapshot,
    LocalizedEncodingCode, LocalizedString, PrimaryMessage, RunningIntent, SecsItem, SecsItemError,
    SessionId, Stream,
};

#[test]
/// Verifies that public configuration and message values require no runtime.
fn public_values_build_without_a_runtime() {
    let session_id = SessionId::new(0).expect("data session id");
    let address = SocketAddr::from(([127, 0, 0, 1], 5000));
    let config = EndpointConfig::active(address, session_id);

    assert_eq!(config.mode(), ConnectionMode::Active);
    assert_eq!(config.address(), address);
    config.validate().expect("default config must be valid");

    let message = PrimaryMessage::new(
        Stream::new(1).expect("stream"),
        Function::new(1),
        Some(SecsItem::Ascii(AsciiString::new("HELLO").expect("ASCII"))),
    );

    assert_eq!(message.stream().get(), 1);
    assert_eq!(message.function().get(), 1);
    assert!(matches!(message.body(), Some(SecsItem::Ascii(_))));
}

#[test]
/// Verifies that absent Message Text differs from each typed empty E5 value.
fn absence_and_typed_empty_items_remain_distinct() {
    let stream = Stream::new(1).expect("stream");
    let function = Function::new(1);

    let absent = PrimaryMessage::new(stream, function, None);
    let empty_ascii = PrimaryMessage::new(
        stream,
        function,
        Some(SecsItem::Ascii(AsciiString::default())),
    );
    let empty_list = PrimaryMessage::new(stream, function, Some(SecsItem::List(Vec::new())));

    assert!(absent.body().is_none());
    assert_ne!(empty_ascii.body(), empty_list.body());
}

#[test]
/// Verifies that the ASCII wrapper rejects the first non-seven-bit byte.
fn ascii_is_strictly_seven_bit() {
    assert_eq!(
        AsciiString::new("设备"),
        Err(SecsItemError::NonAscii {
            index: 0,
            byte: 0xE8,
        })
    );
}

#[test]
/// Verifies preservation of localized-string encoding metadata and payload.
fn localized_strings_keep_the_e5_encoding_header() {
    assert_eq!(
        LocalizedEncodingCode::new(0),
        Err(SecsItemError::ReservedLocalizedEncodingCode)
    );

    let encoding = LocalizedEncodingCode::new(2).expect("non-reserved encoding code");
    let item = SecsItem::Localized(LocalizedString::new(encoding, "设备".as_bytes().to_vec()));
    let SecsItem::Localized(value) = item else {
        panic!("localized item");
    };
    assert_eq!(value.encoding(), encoding);
    assert_eq!(value.as_bytes(), "设备".as_bytes());
}

#[test]
/// Verifies the initial endpoint snapshot is fully stopped and resource-clean.
fn lifecycle_vocabulary_starts_clean() {
    let state = EndpointStateSnapshot::default();
    assert_eq!(state.desired(), RunningIntent::Stopped);
    assert_eq!(state.phase(), EndpointPhase::StoppedClean);
    assert_eq!(state.generation(), GenerationSlotSnapshot::None);
    assert_eq!(state.session(), None);
}

#[test]
/// Verifies that applications express control intent without raw headers.
fn typed_control_intents_do_not_expose_raw_headers() {
    let intents = [
        ControlIntent::Select,
        ControlIntent::Deselect,
        ControlIntent::Linktest,
        ControlIntent::Separate,
    ];
    assert_eq!(intents.len(), 4);
}

#[test]
/// Verifies fixed E37/E5 fixture lengths and selected control-header fields.
fn provisional_vectors_keep_framing_boundaries() {
    let data_length = u32::from_be_bytes(
        fixtures::HSMS_DATA_S1F1_ASCII_HELLO[..4]
            .try_into()
            .expect("length prefix"),
    ) as usize;
    assert_eq!(data_length + 4, fixtures::HSMS_DATA_S1F1_ASCII_HELLO.len());
    assert_eq!(
        &fixtures::HSMS_DATA_S1F1_ASCII_HELLO[14..],
        fixtures::SECS2_ASCII_HELLO
    );

    let reject_length = u32::from_be_bytes(
        fixtures::HSMS_REJECT_REQUEST[..4]
            .try_into()
            .expect("length prefix"),
    ) as usize;
    assert_eq!(reject_length + 4, fixtures::HSMS_REJECT_REQUEST.len());
    assert_eq!(&fixtures::HSMS_REJECT_REQUEST[4..6], &[0x00, 0x00]);
    assert_eq!(fixtures::HSMS_REJECT_REQUEST[6], 0x00);
    assert_eq!(fixtures::HSMS_REJECT_REQUEST[7], 0x04);
    assert_eq!(fixtures::HSMS_REJECT_REQUEST[8], 0x00);
    assert_eq!(fixtures::HSMS_REJECT_REQUEST[9], 0x07);
    assert_eq!(fixtures::PARTIAL_LENGTH_PREFIX.len(), 2);
    assert!(fixtures::PARTIAL_FRAME.len() < 4 + 0x10);
    assert_eq!(fixtures::OVERSIZED_LENGTH_PREFIX, &[0xFF; 4]);
}

#[test]
/// Verifies rejection of zero-valued SECS-II resource limits.
fn decode_limits_reject_invalid_resource_bounds() {
    assert_eq!(
        DecodeLimits::new(0, 1, 1, 1),
        Err(SecsItemError::ZeroLimit { field: "max_depth" })
    );
}

#[test]
/// Verifies endpoint queues and registries are bounded by validated capacities.
fn endpoint_limits_bound_every_long_lived_queue_and_registry() {
    assert_eq!(EndpointLimits::default().reply_capability_capacity(), 256);

    let limits = EndpointLimits::new(10, 1, 2, 3, 4, 5, 6, 7).expect("valid endpoint limits");
    assert_eq!(limits.max_message_length(), 10);
    assert_eq!(limits.command_capacity(), 1);
    assert_eq!(limits.critical_lane_capacity(), 2);
    assert_eq!(limits.data_lane_capacity(), 3);
    assert_eq!(limits.application_event_capacity(), 4);
    assert_eq!(limits.transaction_capacity(), 5);
    assert_eq!(limits.tombstone_capacity(), 6);
    assert_eq!(limits.reply_capability_capacity(), 7);

    assert_eq!(
        EndpointLimits::new(10, 1, 2, 3, 4, 0, 6, 7),
        Err(ConfigError::ZeroCapacity {
            field: "transaction_capacity",
        })
    );
    assert_eq!(
        EndpointLimits::new(10, 1, 2, 3, 4, 5, 6, 0),
        Err(ConfigError::ZeroCapacity {
            field: "reply_capability_capacity",
        })
    );
    assert_eq!(
        EndpointLimits::new(9, 1, 2, 3, 4, 5, 6, 7),
        Err(ConfigError::MessageLengthTooSmall { value: 9 })
    );
}
