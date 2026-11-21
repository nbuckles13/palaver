//! Variable-length integers (section 8.1).
//!
//! The number of leading 1 bits in the first byte gives the number of bytes that follow; the
//! remaining bits hold the value in network byte order. One to nine bytes, any `u64`.

use crate::{Reader, Result};

/// Bytes the shortest encoding of `v` takes.
#[must_use]
pub const fn vi64_len(v: u64) -> usize {
    match v {
        0..=0x7f => 1,
        0x80..=0x3fff => 2,
        0x4000..=0x1f_ffff => 3,
        0x20_0000..=0x0fff_ffff => 4,
        0x1000_0000..=0x7_ffff_ffff => 5,
        0x8_0000_0000..=0x3ff_ffff_ffff => 6,
        0x400_0000_0000..=0x1_ffff_ffff_ffff => 7,
        0x2_0000_0000_0000..=0xff_ffff_ffff_ffff => 8,
        _ => 9,
    }
}

/// Append the shortest encoding of `v`.
pub fn put_vi64(out: &mut Vec<u8>, v: u64) {
    let n = vi64_len(v);
    let be = v.to_be_bytes();
    if n == 9 {
        out.push(0xff);
        out.extend_from_slice(&be);
        return;
    }
    // n - 1 leading ones, then a zero: 0x00, 0x80, 0xc0, ... 0xfe.
    let prefix = !(0xffu8 >> (n - 1));
    let start = out.len();
    out.extend_from_slice(&be[8 - n..]);
    if let Some(first) = out.get_mut(start) {
        *first |= prefix;
    }
}

impl Reader<'_> {
    /// Decode one variable-length integer. Longer-than-needed encodings are valid.
    ///
    /// # Errors
    ///
    /// Fails, consuming nothing, if the integer runs past the end of the reader.
    pub fn vi64(&mut self) -> Result<u64> {
        let mut probe = self.clone();
        let first = probe.u8()?;
        let extra = first.leading_ones() as usize; // 0..=8 bytes after the first
        let tail = probe.take(extra)?;
        let mut be = [0u8; 8];
        be[8 - extra..].copy_from_slice(tail);
        let mut v = u64::from_be_bytes(be);
        if extra < 8 {
            // The first byte keeps 7 - extra value bits (none when extra is 7).
            let mask = 0x7fu8 >> extra;
            v |= u64::from(first & mask) << (8 * extra);
        }
        *self = probe;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    fn enc(v: u64) -> Vec<u8> {
        let mut out = vec![];
        put_vi64(&mut out, v);
        out
    }

    fn dec(bytes: &[u8]) -> Result<u64> {
        Reader::new(bytes).vi64()
    }

    /// Table 4 of the draft.
    const VECTORS: &[(&[u8], u64)] = &[
        (&[0x25], 37),
        (&[0x80, 0x25], 37),
        (&[0xbb, 0xbd], 15_293),
        (&[0xed, 0x7f, 0x3e, 0x7d], 226_442_877),
        (&[0xfa, 0xa1, 0xa0, 0xe4, 0x03, 0xd8], 2_893_212_287_960),
        (
            &[0xfc, 0x89, 0x98, 0xab, 0xc6, 0x6b, 0xc0],
            151_288_809_941_952,
        ),
        (
            &[0xfe, 0xfa, 0x31, 0x8f, 0xa8, 0xe3, 0xca, 0x11],
            70_423_237_261_249_041,
        ),
        (&[0xff; 9], u64::MAX),
    ];

    #[test]
    fn decodes_the_draft_examples() {
        for (bytes, value) in VECTORS {
            assert_eq!(dec(bytes), Ok(*value), "{bytes:x?}");
        }
    }

    #[test]
    fn encodes_the_draft_examples_that_are_minimal() {
        for (bytes, value) in VECTORS.iter().filter(|(b, _)| *b != [0x80, 0x25]) {
            assert_eq!(enc(*value), *bytes, "{value}");
        }
    }

    #[test]
    fn zero_decodes_from_any_length() {
        for n in 1..=9usize {
            let mut bytes = vec![0u8; n];
            bytes[0] = if n == 9 { 0xff } else { !(0xffu8 >> (n - 1)) };
            assert_eq!(dec(&bytes), Ok(0), "{n} bytes");
        }
    }

    #[test]
    fn every_length_boundary_round_trips_at_the_shortest_length() {
        for bits in 0..=64u32 {
            for v in [(1u128 << bits) - 1, 1u128 << bits] {
                let Ok(v) = u64::try_from(v) else { continue };
                let bytes = enc(v);
                assert_eq!(bytes.len(), vi64_len(v));
                assert_eq!(dec(&bytes), Ok(v));
                // shortest: the value needs more bits than one byte fewer can hold
                let fewer_bits = 7 * (bytes.len() - 1);
                assert!(
                    bytes.len() == 1 || v >> fewer_bits != 0,
                    "{v} in {} bytes",
                    bytes.len()
                );
            }
        }
    }

    #[test]
    fn a_truncated_integer_asks_for_more_and_consumes_nothing() {
        for (bytes, _) in VECTORS {
            for cut in 0..bytes.len() {
                let mut r = Reader::new(&bytes[..cut]);
                assert_eq!(r.vi64(), Err(Error::NeedMore));
                assert_eq!(r.remaining(), cut);
            }
        }
    }

    #[test]
    fn decoding_leaves_the_following_bytes() {
        let mut r = Reader::new(&[0xbb, 0xbd, 0x07]);
        assert_eq!(r.vi64(), Ok(15_293));
        assert_eq!(r.rest(), [0x07]);
    }
}
