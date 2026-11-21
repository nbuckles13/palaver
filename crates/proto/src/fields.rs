//! The encoding of control-plane structures: numbered fields as `MoQ` key-value pairs.
//!
//! An even field number carries an integer, an odd one carries bytes; a nested structure is a
//! bytes field holding more pairs; a repeated field is the same number several times. A reader
//! skips numbers it does not know, which is what lets nodes running different versions share a
//! meeting. Fields may be added; a number is never reused with another meaning.

use palaver_moq_wire::{Kvp, KvpIter, Reader, Value, encode_kvps};

/// Most entries accepted in any repeated field. A meeting is tens of nodes; this only bounds
/// what a malformed message can make a reader allocate.
pub const MAX_ENTRIES: usize = 1024;

/// Why a control-plane structure could not be decoded or accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The bytes are not well-formed key-value pairs.
    Malformed,
    /// A required field is absent.
    Missing(&'static str),
    /// A field is present with a value that is not allowed.
    Invalid(&'static str),
    /// The signature does not verify, or does not belong to who it should.
    BadSignature(&'static str),
}

/// Collects fields and encodes them in ascending number order, keeping repeats in the order
/// they were added.
#[derive(Default)]
pub struct Writer<'a> {
    pairs: Vec<Kvp<'a>>,
}

impl<'a> Writer<'a> {
    pub(crate) fn int(&mut self, number: u64, value: u64) {
        debug_assert!(number.is_multiple_of(2));
        self.pairs.push(Kvp {
            ty: number,
            value: Value::Int(value),
        });
    }

    pub(crate) fn bytes(&mut self, number: u64, value: &'a [u8]) {
        debug_assert!(!number.is_multiple_of(2));
        self.pairs.push(Kvp {
            ty: number,
            value: Value::Bytes(value),
        });
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.pairs.sort_by_key(|pair| pair.ty);
        let mut out = vec![];
        // Sorted, and each value matches its number's parity, so this cannot fail; a value over
        // the 65,535-byte limit would, and yields an empty encoding the reader then rejects.
        if encode_kvps(&self.pairs, &mut out).is_err() {
            out.clear();
        }
        out
    }
}

/// Iterate the fields of an encoded structure.
pub fn fields(bytes: &[u8]) -> impl Iterator<Item = Result<Kvp<'_>, DecodeError>> {
    let mut outer = Reader::new(bytes);
    let region = outer.sub(bytes.len()).unwrap_or_else(|_| Reader::new(&[]));
    KvpIter::new(region).map(|pair| pair.map_err(|_| DecodeError::Malformed))
}

/// Push onto a repeated field, refusing more than [`MAX_ENTRIES`].
pub fn push<T>(list: &mut Vec<T>, item: T, what: &'static str) -> Result<(), DecodeError> {
    if list.len() >= MAX_ENTRIES {
        return Err(DecodeError::Invalid(what));
    }
    list.push(item);
    Ok(())
}
