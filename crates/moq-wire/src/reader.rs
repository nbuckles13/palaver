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
    #[must_use]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            bounded: false,
        }
    }

    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.buf.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    const fn short(&self) -> Error {
        if self.bounded {
            Error::Violation("value runs past the end of its enclosing length")
        } else {
            Error::NeedMore
        }
    }

    /// The next `n` bytes.
    ///
    /// # Errors
    ///
    /// Fails, consuming nothing, if fewer than `n` bytes are left: [`Error::NeedMore`] on a
    /// stream buffer, [`Error::Violation`] inside a region made by [`Reader::sub`].
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let (head, tail) = self.buf.split_at_checked(n).ok_or_else(|| self.short())?;
        self.buf = tail;
        Ok(head)
    }

    /// The next byte.
    ///
    /// # Errors
    ///
    /// Fails if the reader is empty; see [`Reader::take`].
    pub fn u8(&mut self) -> Result<u8> {
        self.take(1)?.first().copied().ok_or_else(|| self.short())
    }

    /// A reader over the next `n` bytes, which the caller's message declared as one region.
    ///
    /// # Errors
    ///
    /// Fails if fewer than `n` bytes are left; see [`Reader::take`].
    pub fn sub(&mut self, n: usize) -> Result<Self> {
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
