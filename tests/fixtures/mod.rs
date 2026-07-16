//! Legacy-derived vectors checked against the local E37-0298 and E5-0301
//! message-format sections. Wave 1 will turn these fixed inputs into codec
//! conformance tests.

pub const SECS2_ASCII_HELLO: &[u8] = &[0x41, 0x05, b'H', b'E', b'L', b'L', b'O'];

pub const HSMS_DATA_S1F1_ASCII_HELLO: &[u8] = &[
    0x00, 0x00, 0x00, 0x11, // message length: 10-byte header + 7-byte text
    0x00, 0x00, // session id
    0x01, // W=0, stream=1
    0x01, // function=1
    0x00, // PType=0
    0x00, // SType=Data
    0x00, 0x00, 0x00, 0x01, // system bytes
    0x41, 0x05, b'H', b'E', b'L', b'L', b'O',
];

pub const HSMS_REJECT_REQUEST: &[u8] = &[
    0x00, 0x00, 0x00, 0x0A, // control messages have a header only
    0x00, 0x00, // same session id as the rejected Data message
    0x00, // rejected SType=Data
    0x04, // reason=Entity Not Selected
    0x00, // PType
    0x07, // Reject.req
    0x00, 0x00, 0x00, 0x11, // system bytes
];

pub const PARTIAL_LENGTH_PREFIX: &[u8] = &[0x00, 0x00];
pub const PARTIAL_FRAME: &[u8] = &[0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x01, 0x01];
pub const OVERSIZED_LENGTH_PREFIX: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFF];
