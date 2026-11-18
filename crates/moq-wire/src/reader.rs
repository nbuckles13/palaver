//! A cursor over a byte slice.

use crate::{Error, Result};

/// Reads values from a byte slice without copying.
///
/// A reader over a whole stream buffer reports [`Error::NeedMore`] when it runs out, because
/// more bytes may arrive. A reader made by [`Reader::sub`] covers a region whose length the
/// message already stated, so running out there is a [`Error::Violation`].
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    bounded: bool,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            bounded: false,
        }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn short(&self) -> Error {
        if self.bounded {
            Error::Violation("value runs past the end of its enclosing length")
        } else {
            Error::NeedMore
        }
    }

    /// The next `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let (head, tail) = self.buf.split_at_checked(n).ok_or_else(|| self.short())?;
        self.buf = tail;
        Ok(head)
    }

    pub fn u8(&mut self) -> Result<u8> {
        self.take(1)?.first().copied().ok_or_else(|| self.short())
    }

    /// A reader over the next `n` bytes, which the caller's message declared as one region.
    pub fn sub(&mut self, n: usize) -> Result<Reader<'a>> {
        Ok(Reader {
            buf: self.take(n)?,
            bounded: true,
        })
    }

    /// Everything that is left.
    pub fn rest(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.buf)
    }
}
