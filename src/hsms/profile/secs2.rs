//! PType=0 mapping between HSMS Message Text and optional SECS-II content.
//!
//! This profile owns only presentation semantics: empty Message Text means no
//! item, while non-empty text must be exactly one complete SECS-II item. It
//! neither parses HSMS headers nor creates Core/runtime events.

use crate::secs2::{
    codec::{DecodeError, EncodeError, EncodedItemPlan, Secs2Decoder},
    SecsItem,
};

/// Prepared optional SECS-II body that can append directly to a final frame.
#[derive(Debug)]
pub(crate) enum Secs2BodyPlan<'a> {
    /// Absent HSMS Message Text, which appends zero bytes.
    Absent,
    /// One measured SECS-II item to append as Message Text.
    Item {
        /// Validated, immutable SECS-II encoding plan.
        plan: EncodedItemPlan<'a>,
    },
}

impl Secs2BodyPlan<'_> {
    /// Returns the exact number of Message Text bytes this plan appends.
    pub(crate) const fn encoded_length(&self) -> usize {
        match self {
            Self::Absent => 0,
            Self::Item { plan } => plan.encoded_length(),
        }
    }

    /// Appends Message Text to an existing final protocol buffer.
    ///
    /// `Absent` writes nothing. `Item` writes the already measured SECS-II
    /// tree without allocating an intermediate payload vector.
    ///
    /// # Errors
    ///
    /// Returns an [`EncodeError`] only if the SECS-II write pass detects an
    /// invariant mismatch after its successful immutable measurement pass.
    pub(crate) fn write_into(&self, output: &mut Vec<u8>) -> Result<(), EncodeError> {
        match self {
            Self::Absent => Ok(()),
            Self::Item { plan } => plan.write_into(output),
        }
    }
}

/// Pure presentation-profile seam used by the HSMS-SS composition codec.
pub(crate) trait Secs2Profile {
    /// Decodes absent text or exactly one complete SECS-II item.
    ///
    /// Empty `text` returns `Ok(None)`. Non-empty input returns
    /// `Ok(Some(item))` or the strict decoder's structured error.
    fn decode_text(&self, text: &[u8]) -> Result<Option<SecsItem>, DecodeError>;

    /// Validates and measures an optional semantic body for direct appending.
    ///
    /// `None` produces a zero-byte plan. `Some(item)` returns its immutable
    /// measured plan or the precise SECS-II encoding failure.
    fn prepare_body<'a>(
        &self,
        body: Option<&'a SecsItem>,
    ) -> Result<Secs2BodyPlan<'a>, EncodeError>;
}

/// Strict built-in HSMS-SS profile backed by the Wave 1 SECS-II codec.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StrictSecs2Profile {
    /// Resource-bounded decoder applied to every non-empty Message Text.
    decoder: Secs2Decoder,
}

impl StrictSecs2Profile {
    /// Creates a strict profile using the configured `decoder`.
    pub(crate) const fn new(decoder: Secs2Decoder) -> Self {
        Self { decoder }
    }
}

impl Secs2Profile for StrictSecs2Profile {
    /// Implements the HSMS distinction between absent and typed-empty text.
    fn decode_text(&self, text: &[u8]) -> Result<Option<SecsItem>, DecodeError> {
        if text.is_empty() {
            Ok(None)
        } else {
            self.decoder.decode_item(text).map(Some)
        }
    }

    /// Measures a present item once or returns the zero-byte absent plan.
    fn prepare_body<'a>(
        &self,
        body: Option<&'a SecsItem>,
    ) -> Result<Secs2BodyPlan<'a>, EncodeError> {
        match body {
            None => Ok(Secs2BodyPlan::Absent),
            Some(item) => Ok(Secs2BodyPlan::Item {
                plan: EncodedItemPlan::new(item)?,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Profile tests focus on presence semantics and direct-buffer encoding.

    use crate::secs2::{AsciiString, DecodeLimits};

    use super::*;

    /// Confirms empty Message Text means absence, not a typed-empty item.
    #[test]
    fn empty_text_decodes_to_absence() {
        let profile = StrictSecs2Profile::new(Secs2Decoder::default());
        assert_eq!(profile.decode_text(&[]), Ok(None));
    }

    /// Confirms a typed-empty ASCII item remains present.
    #[test]
    fn typed_empty_ascii_remains_present() {
        let profile = StrictSecs2Profile::new(Secs2Decoder::default());
        assert_eq!(
            profile.decode_text(&[0x41, 0]),
            Ok(Some(SecsItem::Ascii(AsciiString::default())))
        );
    }

    /// Confirms strict trailing-byte errors remain structured.
    #[test]
    fn trailing_item_is_rejected_by_strict_decoder() {
        let profile = StrictSecs2Profile::new(Secs2Decoder::default());
        let error = profile
            .decode_text(&[0x21, 0, 0x21, 0])
            .expect_err("two items are not one Message Text item");
        assert_eq!(
            error,
            DecodeError::TrailingBytes {
                consumed: 2,
                total: 4,
            }
        );
    }

    /// Confirms a prepared body appends to an existing enclosing buffer.
    #[test]
    fn prepared_body_appends_without_replacing_envelope_bytes() {
        let profile = StrictSecs2Profile::new(Secs2Decoder::default());
        let body = SecsItem::U1(vec![1, 2, 3]);
        let plan = profile.prepare_body(Some(&body)).expect("body plan");
        let mut output = vec![0xAA];
        plan.write_into(&mut output).expect("append");
        assert_eq!(plan.encoded_length(), 5);
        assert_eq!(output, &[0xAA, 0xA5, 3, 1, 2, 3]);
    }

    /// Confirms configured decode limits are enforced by the profile.
    #[test]
    fn configured_decoder_limits_are_used() {
        let limits = DecodeLimits::new(2, 4, 2, 2).expect("valid limits");
        let profile = StrictSecs2Profile::new(Secs2Decoder::new(limits));
        assert_eq!(profile.decoder.limits(), limits);
        let error = profile
            .decode_text(&[0x21, 3, 1, 2, 3])
            .expect_err("payload exceeds max_item_bytes");
        assert!(matches!(error, DecodeError::ItemBytesExceeded { .. }));
    }
}
