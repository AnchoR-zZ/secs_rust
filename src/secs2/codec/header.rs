//! Pure SECS-II item-header helpers: format codes, length-byte counts and the
//! single-byte mapping between an [`crate::secs2::SecsItem`] variant and its
//! E5 format byte.
//!
//! Everything in this module is allocation-free and side-effect free. It is
//! shared by both the encoder and the decoder so that the wire form of every
//! item type lives in exactly one place.
//!
//! E5 reference (`document/SEMI E5-0301 ...pdf`, Section 6.2 / Table 1):
//!
//! - The wire *format byte* is eight bits whose upper six bits (bits 8..=3)
//!   are the *Format Code* and whose lower two bits (bits 2..=1) are the
//!   *number of length bytes* (1, 2 or 3). A zero length-byte count is
//!   illegal.
//! - The *Format Code* is the six-bit number E5 prints in octal (for example
//!   `20` octal for ASCII). Equivalently, `format_code = format_byte >> 2`
//!   and `format_byte = (format_code << 2) | length_byte_count`.

/// Six-bit SECS-II format code taken from E5 Table 1, expressed in octal as
/// the standard prints it.
///
/// The discriminant value is the bare six-bit code in the range 0..=63. It is
/// *not* the upper six bits of an already-shifted wire byte: combine it with
/// a [`LengthByteCount`] via [`LengthByteCount::format_byte`] to assemble the
/// wire byte, or recover it from a wire byte via `format_byte >> 2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
#[allow(non_camel_case_types)]
pub enum FormatCode {
    /// `00` — List whose length counts direct child items, not bytes.
    List = 0o00,
    /// `10` — Unspecified binary octets.
    Binary = 0o10,
    /// `11` — Boolean, one byte per value.
    Boolean = 0o11,
    /// `20` — Seven-bit ASCII character string.
    Ascii = 0o20,
    /// `21` — JIS-8 character string, preserved byte-for-byte.
    Jis8 = 0o21,
    /// `22` — Localized character string preceded by a two-byte LSH.
    Localized = 0o22,
    /// `30` — Eight-byte signed integer.
    I8 = 0o30,
    /// `31` — One-byte signed integer.
    I1 = 0o31,
    /// `32` — Two-byte signed integer.
    I2 = 0o32,
    /// `34` — Four-byte signed integer.
    I4 = 0o34,
    /// `40` — Eight-byte IEEE-754 floating point.
    F8 = 0o40,
    /// `44` — Four-byte IEEE-754 floating point.
    F4 = 0o44,
    /// `50` — Eight-byte unsigned integer.
    U8 = 0o50,
    /// `51` — One-byte unsigned integer.
    U1 = 0o51,
    /// `52` — Two-byte unsigned integer.
    U2 = 0o52,
    /// `54` — Four-byte unsigned integer.
    U4 = 0o54,
}

impl FormatCode {
    /// Returns the bare six-bit format code in the range 0..=63.
    ///
    /// E5 prints this value in octal; for ASCII it is `20` octal (decimal
    /// 16). Combine with a length-byte count via
    /// [`LengthByteCount::format_byte`] to obtain the wire format byte.
    #[must_use]
    pub const fn six_bit_value(self) -> u8 {
        self as u8
    }

    /// Attempts to interpret `format_byte >> 2` as a known E5 format code.
    ///
    /// Callers must pass `format_byte >> 2`, that is, the upper six bits
    /// already shifted down to a 0..=63 value. Inputs above `0x3F` are outside
    /// that domain and are rejected rather than truncated or aliased.
    ///
    /// Returns `Some(code)` when the six-bit value is one of the sixteen
    /// codes defined by E5 Table 1 (List plus fifteen non-List formats),
    /// otherwise `None` for unknown or reserved codes that the decoder must
    /// reject.
    #[must_use]
    pub fn from_six_bit(six_bit: u8) -> Option<Self> {
        if six_bit > 0b0011_1111 {
            return None;
        }
        Some(match six_bit {
            0o00 => Self::List,
            0o10 => Self::Binary,
            0o11 => Self::Boolean,
            0o20 => Self::Ascii,
            0o21 => Self::Jis8,
            0o22 => Self::Localized,
            0o30 => Self::I8,
            0o31 => Self::I1,
            0o32 => Self::I2,
            0o34 => Self::I4,
            0o40 => Self::F8,
            0o44 => Self::F4,
            0o50 => Self::U8,
            0o51 => Self::U1,
            0o52 => Self::U2,
            0o54 => Self::U4,
            _ => return None,
        })
    }

    /// Returns the number of bytes occupied by a single element of this
    /// numeric format.
    ///
    /// Returns `None` for variable-width formats (Binary, Boolean, ASCII,
    /// JIS-8, Localized, List) whose payload is a byte sequence rather than a
    /// fixed-width numeric array; the decoder uses `None` to skip alignment
    /// checks for those formats.
    #[must_use]
    pub const fn element_width(self) -> Option<usize> {
        Some(match self {
            Self::I8 | Self::F8 | Self::U8 => 8,
            Self::I4 | Self::F4 | Self::U4 => 4,
            Self::I2 | Self::U2 => 2,
            Self::I1 | Self::U1 => 1,
            // Variable-width formats have no single element width.
            Self::List
            | Self::Binary
            | Self::Boolean
            | Self::Ascii
            | Self::Jis8
            | Self::Localized => {
                return None;
            }
        })
    }
}

/// Number of length bytes (1, 2 or 3) following a SECS-II format byte.
///
/// The low two bits of a wire format byte can only encode `0..=3`, so the
/// only illegal value on the wire is `0`; the decoder rejects it via
/// [`LengthByteCount::from_low_bits`]. There is no other illegal value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum LengthByteCount {
    /// One Length Byte, covering 0..=255 List children or body bytes.
    One = 1,
    /// Two Length Bytes, covering 0..=65,535 List children or body bytes.
    Two = 2,
    /// Three Length Bytes, covering 0..=16,777,215 List children or body bytes.
    Three = 3,
}

impl LengthByteCount {
    /// Interprets the low two bits of a wire format byte as a length-byte
    /// count.
    ///
    /// Returns `Some(count)` for the legal values `1`, `2` and `3`, and
    /// `None` for the illegal value `0`.
    #[must_use]
    pub fn from_low_bits(low_bits: u8) -> Option<Self> {
        match low_bits & 0b0000_0011 {
            1 => Some(Self::One),
            2 => Some(Self::Two),
            3 => Some(Self::Three),
            // Zero is explicitly illegal per E5 §6.2.1 / §6.3.1.
            _ => None,
        }
    }

    /// Returns the numeric length-byte count (1, 2 or 3).
    #[must_use]
    pub const fn as_count(self) -> usize {
        self as usize
    }

    /// Chooses the smallest length-byte count that can represent
    /// `declared_length`.
    ///
    /// The declared value is a direct-child count for Lists and a payload byte
    /// count for all other formats. Returns `None` when it exceeds the 24-bit
    /// E5 length field (`0x00FF_FFFF`).
    #[must_use]
    pub fn for_declared_length(declared_length: usize) -> Option<Self> {
        if declared_length <= 0xFF {
            Some(Self::One)
        } else if declared_length <= 0xFFFF {
            Some(Self::Two)
        } else if declared_length <= crate::secs2::MAX_ENCODED_ITEM_LENGTH {
            Some(Self::Three)
        } else {
            None
        }
    }

    /// Combines the six-bit format code with this count to form the wire
    /// format byte (`(code << 2) | length_byte_count`).
    #[must_use]
    pub const fn format_byte(self, code: FormatCode) -> u8 {
        ((code.six_bit_value()) << 2) | ((self as u8) & 0b0000_0011)
    }
}

#[cfg(test)]
mod tests {
    //! Pure header-logic unit tests.
    use super::*;

    /// Verifies that every supported format code survives raw-value conversion.
    #[test]
    fn format_code_round_trips_through_six_bits() {
        for code in [
            FormatCode::List,
            FormatCode::Binary,
            FormatCode::Boolean,
            FormatCode::Ascii,
            FormatCode::Jis8,
            FormatCode::Localized,
            FormatCode::I8,
            FormatCode::I1,
            FormatCode::I2,
            FormatCode::I4,
            FormatCode::F8,
            FormatCode::F4,
            FormatCode::U8,
            FormatCode::U1,
            FormatCode::U2,
            FormatCode::U4,
        ] {
            assert_eq!(
                FormatCode::from_six_bit(code.six_bit_value()),
                Some(code),
                "{code:?} must round-trip"
            );
        }
    }

    /// Verifies rejection of reserved values inside the six-bit domain.
    #[test]
    fn unknown_format_codes_are_rejected() {
        // Reserved slots inside the six-bit space must return None.
        assert_eq!(FormatCode::from_six_bit(0o23), None);
        assert_eq!(FormatCode::from_six_bit(0o33), None);
        assert_eq!(FormatCode::from_six_bit(0o77), None);
    }

    /// Verifies that values outside the six-bit input domain are rejected
    /// instead of being masked into unrelated valid format codes.
    #[test]
    fn out_of_range_six_bit_values_are_rejected_without_aliasing() {
        assert_eq!(FormatCode::from_six_bit(0x40), None);
        assert_eq!(FormatCode::from_six_bit(0x50), None);
        assert_eq!(FormatCode::from_six_bit(0xFF), None);
    }

    /// Verifies that zero is the only illegal low-bit Length Byte count.
    #[test]
    fn length_byte_count_rejects_zero() {
        assert_eq!(LengthByteCount::from_low_bits(0b00), None);
        assert_eq!(
            LengthByteCount::from_low_bits(0b01),
            Some(LengthByteCount::One)
        );
        assert_eq!(
            LengthByteCount::from_low_bits(0b10),
            Some(LengthByteCount::Two)
        );
        assert_eq!(
            LengthByteCount::from_low_bits(0b11),
            Some(LengthByteCount::Three)
        );
    }

    /// Verifies that every possible Format Byte has either zero or a legal
    /// one-to-three Length Byte Count.
    ///
    /// Because the low two bits can only encode `0..=3`, the only illegal
    /// wire value is `00`; there is no fourth `UnsupportedLengthByteCount`
    /// case to report. This sweeps all 256 possible format bytes to lock that
    /// invariant in.
    #[test]
    fn every_format_byte_has_no_unsupported_length_count() {
        for format_byte in 0u8..=u8::MAX {
            match format_byte & 0b11 {
                0 => assert_eq!(LengthByteCount::from_low_bits(format_byte), None),
                1..=3 => assert!(LengthByteCount::from_low_bits(format_byte).is_some()),
                _ => unreachable!(),
            }
        }
    }

    /// Verifies format-byte assembly against the fixed E5 examples.
    #[test]
    fn format_byte_assembly_matches_e5_examples() {
        // E5 §6.5 example b: ASCII "ABC" starts with 0b0100_0001.
        let byte = LengthByteCount::One.format_byte(FormatCode::Ascii);
        assert_eq!(byte, 0b0100_0001);
        // E5 §6.5 example a: binary 0xAA starts with 0b0010_0001.
        assert_eq!(
            LengthByteCount::One.format_byte(FormatCode::Binary),
            0b0010_0001
        );
        // E5 §6.5 example d: 4-byte float starts with 0b1001_0001.
        assert_eq!(
            LengthByteCount::One.format_byte(FormatCode::F4),
            0b1001_0001
        );
    }

    /// Verifies minimal Length Byte selection at every size boundary.
    #[test]
    fn for_declared_length_picks_minimum_representable_count() {
        assert_eq!(
            LengthByteCount::for_declared_length(0),
            Some(LengthByteCount::One)
        );
        assert_eq!(
            LengthByteCount::for_declared_length(0xFF),
            Some(LengthByteCount::One)
        );
        assert_eq!(
            LengthByteCount::for_declared_length(0x100),
            Some(LengthByteCount::Two)
        );
        assert_eq!(
            LengthByteCount::for_declared_length(0xFFFF),
            Some(LengthByteCount::Two)
        );
        assert_eq!(
            LengthByteCount::for_declared_length(0x1_0000),
            Some(LengthByteCount::Three)
        );
        assert_eq!(
            LengthByteCount::for_declared_length(crate::secs2::MAX_ENCODED_ITEM_LENGTH),
            Some(LengthByteCount::Three)
        );
        assert_eq!(
            LengthByteCount::for_declared_length(crate::secs2::MAX_ENCODED_ITEM_LENGTH + 1),
            None
        );
    }

    /// Verifies fixed-width numeric sizes and variable-width exclusions.
    #[test]
    fn numeric_element_widths_match_e5_table() {
        assert_eq!(FormatCode::I8.element_width(), Some(8));
        assert_eq!(FormatCode::I4.element_width(), Some(4));
        assert_eq!(FormatCode::I2.element_width(), Some(2));
        assert_eq!(FormatCode::I1.element_width(), Some(1));
        assert_eq!(FormatCode::U8.element_width(), Some(8));
        assert_eq!(FormatCode::U4.element_width(), Some(4));
        assert_eq!(FormatCode::U2.element_width(), Some(2));
        assert_eq!(FormatCode::U1.element_width(), Some(1));
        assert_eq!(FormatCode::F8.element_width(), Some(8));
        assert_eq!(FormatCode::F4.element_width(), Some(4));

        // Variable-width formats have no fixed element width.
        for variable in [
            FormatCode::List,
            FormatCode::Binary,
            FormatCode::Boolean,
            FormatCode::Ascii,
            FormatCode::Jis8,
            FormatCode::Localized,
        ] {
            assert_eq!(variable.element_width(), None);
        }
    }
}
