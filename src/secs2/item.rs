//! Immutable SECS-II item value types used by both the binary codec and SML.
//!
//! This module validates representation-level invariants such as seven-bit
//! ASCII and localized-string encoding codes, but performs no I/O.

use std::fmt;

use super::SecsItemError;

/// The 16-bit encoding code in a SEMI E5 Localized String Header (LSH).
///
/// This code is used only by item Format Code `22` (octal), named
/// "2-byte character" in the E5 format table and defined as a localized
/// character string. It occupies the first two bytes of that item's data
/// area. It is neither the item's six-bit Format Code nor a Length Byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LocalizedEncodingCode(
    /// Non-zero 16-bit LSH encoding code preserved from or written to the wire.
    u16,
);

impl LocalizedEncodingCode {
    /// Validates `value` as the 16-bit LSH encoding code for an E5 Format Code
    /// `22` (octal) localized character string and returns its value object.
    ///
    /// Returns [`SecsItemError::ReservedLocalizedEncodingCode`] when `value`
    /// is zero because E5 reserves encoding code zero.
    pub fn new(value: u16) -> Result<Self, SecsItemError> {
        if value == 0 {
            return Err(SecsItemError::ReservedLocalizedEncodingCode);
        }
        Ok(Self(value))
    }

    #[must_use]
    /// Returns the raw 16-bit encoding code carried in the two-byte LSH.
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// A Format Code `22` (octal) localized string whose two-byte LSH encoding
/// code is preserved separately from its encoded character content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalizedString {
    /// LSH encoding code that applications use to interpret `bytes`.
    encoding: LocalizedEncodingCode,
    /// Encoded character payload, excluding the two-byte LSH encoding code.
    bytes: Vec<u8>,
}

impl LocalizedString {
    #[must_use]
    /// Creates a Format Code `22` (octal) localized string from its LSH
    /// `encoding` code and already encoded payload `bytes`, returning the
    /// immutable value without transcoding the payload.
    pub fn new(encoding: LocalizedEncodingCode, bytes: Vec<u8>) -> Self {
        Self { encoding, bytes }
    }

    #[must_use]
    /// Returns the two-byte LSH encoding code associated with the payload.
    pub const fn encoding(&self) -> LocalizedEncodingCode {
        self.encoding
    }

    #[must_use]
    /// Borrows the encoded character payload without its encoding identifier.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    /// Consumes the value and returns its encoded character payload.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// A validated seven-bit ASCII string used by [`SecsItem::Ascii`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct AsciiString(
    /// Validated string containing only seven-bit ASCII bytes.
    String,
);

impl AsciiString {
    /// Validates `value` and returns a seven-bit ASCII value.
    ///
    /// Returns [`SecsItemError::NonAscii`] with the first offending byte when
    /// the supplied string contains non-ASCII UTF-8 data.
    pub fn new(value: impl Into<String>) -> Result<Self, SecsItemError> {
        let value = value.into();
        if let Some((index, byte)) = value
            .as_bytes()
            .iter()
            .copied()
            .enumerate()
            .find(|(_, byte)| !byte.is_ascii())
        {
            return Err(SecsItemError::NonAscii { index, byte });
        }

        Ok(Self(value))
    }

    /// Returns the validated string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper and returns the owned string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl TryFrom<String> for AsciiString {
    type Error = SecsItemError;

    /// Validates an owned string and returns its seven-bit ASCII wrapper.
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for AsciiString {
    type Error = SecsItemError;

    /// Copies and validates a borrowed string as seven-bit ASCII.
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl AsRef<str> for AsciiString {
    /// Returns the validated value as a borrowed string slice.
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for AsciiString {
    /// Writes the validated ASCII text to `formatter` without transformation.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A typed SECS-II item.
///
/// Absence of HSMS message text is represented by `Option<SecsItem>`, not by
/// an `Empty` variant. Every vector variant therefore supports its own valid
/// typed-empty value.
#[derive(Clone, Debug, PartialEq)]
pub enum SecsItem {
    /// Ordered child items; an empty vector is a valid empty List.
    List(Vec<SecsItem>),
    /// Uninterpreted octets.
    Binary(Vec<u8>),
    /// Logical values encoded by the E5 Boolean format.
    Boolean(Vec<bool>),
    /// Validated seven-bit ASCII characters.
    Ascii(AsciiString),
    /// JIS-8 bytes preserved without Unicode transcoding.
    Jis8(Vec<u8>),
    /// Format Code `22` (octal) characters paired with their LSH encoding code.
    Localized(LocalizedString),
    /// Eight-byte signed integers.
    I8(Vec<i64>),
    /// One-byte signed integers.
    I1(Vec<i8>),
    /// Two-byte signed integers.
    I2(Vec<i16>),
    /// Four-byte signed integers.
    I4(Vec<i32>),
    /// Eight-byte IEEE-754 floating-point values.
    F8(Vec<f64>),
    /// Four-byte IEEE-754 floating-point values.
    F4(Vec<f32>),
    /// Eight-byte unsigned integers.
    U8(Vec<u64>),
    /// One-byte unsigned integers.
    U1(Vec<u8>),
    /// Two-byte unsigned integers.
    U2(Vec<u16>),
    /// Four-byte unsigned integers.
    U4(Vec<u32>),
}
