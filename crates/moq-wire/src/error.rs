//! Decode and encode errors.

/// Why a value could not be decoded or encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The input ended before the value did. On a stream, wait for more bytes and retry.
    NeedMore,
    /// The bytes break a rule of the draft; the session is closed with PROTOCOL_VIOLATION.
    Violation(&'static str),
    /// A known key-value type carried the wrong kind of value (KEY_VALUE_FORMATTING_ERROR).
    KeyValueFormatting(&'static str),
}

pub type Result<T> = core::result::Result<T, Error>;
