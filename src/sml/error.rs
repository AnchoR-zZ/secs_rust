use nom::error::{ContextError, ErrorKind, FromExternalError, ParseError};
use std::fmt;

#[derive(Debug, PartialEq)]
pub enum SmlError<I> {
    Nom(I, ErrorKind),
    Context(I, &'static str),
    InvalidFormat(String),
}

impl<I> ParseError<I> for SmlError<I> {
    fn from_error_kind(input: I, kind: ErrorKind) -> Self {
        SmlError::Nom(input, kind)
    }

    fn append(input: I, kind: ErrorKind, _other: Self) -> Self {
        SmlError::Nom(input, kind)
    }
}

impl<I> ContextError<I> for SmlError<I> {
    fn add_context(input: I, ctx: &'static str, _other: Self) -> Self {
        SmlError::Context(input, ctx)
    }
}

impl<I, E> FromExternalError<I, E> for SmlError<I> {
    fn from_external_error(input: I, kind: ErrorKind, _e: E) -> Self {
        SmlError::Nom(input, kind)
    }
}

impl<I: fmt::Display> fmt::Display for SmlError<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SmlError::Nom(i, kind) => write!(f, "Nom error {:?} at: {}", kind, i),
            SmlError::Context(i, ctx) => write!(f, "Context error '{}' at: {}", ctx, i),
            SmlError::InvalidFormat(msg) => write!(f, "Invalid format: {}", msg),
        }
    }
}

impl<I: fmt::Debug + fmt::Display> std::error::Error for SmlError<I> {}
