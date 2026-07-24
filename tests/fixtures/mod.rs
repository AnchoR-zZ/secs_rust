//! Fixed SECS-II / HSMS byte vectors checked against the local E37-0298 and
//! E5-0301 message-format sections.
//!
//! These constants are deliberately inline byte arrays (not constructed via
//! the encoder) so they double as encoder/decoder conformance evidence: a
//! change in the encoder must not silently change the wire form of any item.
//!
//! Every E5 format byte here is written as `(code << 2) | length_byte_count`
//! to make the byte layout self-documenting and immune to decimal/octal
//! transcription errors.
//!
//! The module is `#[path]`-included by several test binaries, each of which
//! only references the subset of fixtures it needs. The crate-level
//! `#![allow(dead_code)]` below silences per-target dead-code analysis while
//! keeping every fixture visible to whichever test target wants it.

#![allow(dead_code)]

/// Canonical SECS-II ASCII item for the string `HELLO`.
///
/// Format byte `(0o20 << 2) | 1 == 0x41` (ASCII + one length byte), length
/// `0x05`, body `48 45 4C 4C 4F`.
pub const SECS2_ASCII_HELLO: &[u8] = &[(0o20 << 2) | 1, 0x05, b'H', b'E', b'L', b'L', b'O'];

/// E5 §6.5 example a: a single binary octet `0xAA`.
///
/// Format byte `(0o10 << 2) | 1 == 0x21` (Binary + one length byte), length
/// `0x01`, body `0xAA`.
pub const SECS2_BINARY_AA: &[u8] = &[(0o10 << 2) | 1, 0x01, 0xAA];

/// E5 §6.5 example b: ASCII item for the three-character string `ABC`.
///
/// Format byte `(0o20 << 2) | 1 == 0x41`, length `0x03`, body
/// `0x41 0x42 0x43`.
pub const SECS2_ASCII_ABC: &[u8] = &[(0o20 << 2) | 1, 0x03, b'A', b'B', b'C'];

/// E5 §6.5 example c: three two-byte signed integers.
///
/// Format byte `(0o32 << 2) | 1 == 0x69` (I2 + one length byte), length
/// `0x06`, body six bytes. The numeric values of x, y and z are
/// vendor-specific in the standard; this fixture uses `0x0102, 0x0304,
/// 0x0506` so the round-trip can be asserted.
pub const SECS2_I2_THREE_VALUES: &[u8] = &[
    (0o32 << 2) | 1,
    0x06, //
    0x01,
    0x02, // x = 258
    0x03,
    0x04, // y = 772
    0x05,
    0x06, // z = 1286
];

/// E5 §6.5 example d: a single four-byte IEEE-754 floating-point value.
///
/// Format byte `(0o44 << 2) | 1 == 0x91` (F4 + one length byte), length
/// `0x04`, body four bytes holding the big-endian bit pattern for `1.0f32`
/// (`0x3F800000`).
pub const SECS2_F4_ONE: &[u8] = &[(0o44 << 2) | 1, 0x04, 0x3F, 0x80, 0x00, 0x00];

/// Localized Character String (Format Code `22`, octal) carrying the UTF-8
/// bytes for `设备` under LSH encoding code `0x0002` (UTF-8).
///
/// Layout per E5 §6.4: format byte `(0o22 << 2) | 1 == 0x49` (Localized + one
/// length byte), length `0x08` (two LSH bytes + six payload bytes), LSH
/// `0x00 0x02`, payload `E8 AE BE E5 A4 87`.
pub const SECS2_LOCALIZED_UTF8_SHEBEI: &[u8] = &[
    (0o22 << 2) | 1,
    0x08,
    0x00,
    0x02,
    0xE8,
    0xAE,
    0xBE,
    0xE5,
    0xA4,
    0x87,
];

/// A complete HSMS Data frame whose Message Text is
/// [`SECS2_ASCII_HELLO`]: S1F1 W=0 over session 0, system bytes `0x00000001`.
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

/// HSMS `Reject.req` control frame: control messages carry a 10-byte header
/// only, with no Message Text.
pub const HSMS_REJECT_REQUEST: &[u8] = &[
    0x00, 0x00, 0x00, 0x0A, // control messages have a header only
    0x00, 0x00, // same session id as the rejected Data message
    0x00, // rejected SType=Data
    0x04, // reason=Entity Not Selected
    0x00, // PType
    0x07, // Reject.req
    0x00, 0x00, 0x00, 0x11, // system bytes
];

// These three HSMS wire-level fixtures are used by the `wave0_contracts`
// test target; `secs2_conformance` does not reference them.
pub const PARTIAL_LENGTH_PREFIX: &[u8] = &[0x00, 0x00];
pub const PARTIAL_FRAME: &[u8] = &[0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x01, 0x01];
pub const OVERSIZED_LENGTH_PREFIX: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFF];
