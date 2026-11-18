//! Key-value pairs (section 8.3), the encoding of properties and setup options.
//!
//! Each pair carries its type as a delta from the previous pair's type. An even type has one
//! variable-length integer as its value; an odd type has a length and that many bytes.

use crate::{Error, Reader, Result, put_vi64};

/// Longest byte value a pair may carry.
const MAX_VALUE_BYTES: u64 = 65_535;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value<'a> {
    /// Carried by even types.
    Int(u64),
    /// Carried by odd types.
    Bytes(&'a [u8]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kvp<'a> {
    pub ty: u64,
    pub value: Value<'a>,
}

/// Decode pairs until `r` is empty. `r` must cover exactly the pairs (use [`Reader::sub`] when
/// a length precedes them), since nothing in the encoding marks the end.
pub fn decode_kvps<'a>(r: &mut Reader<'a>) -> Result<Vec<Kvp<'a>>> {
    let mut out = vec![];
    let mut ty = 0u64;
    while !r.is_empty() {
        ty = ty
            .checked_add(r.vi64()?)
            .ok_or(Error::Violation("key-value type exceeds 2^64 - 1"))?;
        let value = if ty.is_multiple_of(2) {
            Value::Int(r.vi64()?)
        } else {
            let len = r.vi64()?;
            if len > MAX_VALUE_BYTES {
                return Err(Error::Violation("key-value length exceeds 65535"));
            }
            Value::Bytes(r.take(len as usize)?)
        };
        out.push(Kvp { ty, value });
    }
    Ok(out)
}

/// Encode pairs, which must be sorted by type (equal types may repeat).
pub fn encode_kvps(pairs: &[Kvp<'_>], out: &mut Vec<u8>) -> Result<()> {
    let mut prev = 0u64;
    for pair in pairs {
        let delta = pair
            .ty
            .checked_sub(prev)
            .ok_or(Error::Violation("key-value pairs are not sorted by type"))?;
        match (pair.ty.is_multiple_of(2), pair.value) {
            (true, Value::Int(v)) => {
                put_vi64(out, delta);
                put_vi64(out, v);
            }
            (false, Value::Bytes(b)) if b.len() as u64 <= MAX_VALUE_BYTES => {
                put_vi64(out, delta);
                put_vi64(out, b.len() as u64);
                out.extend_from_slice(b);
            }
            (false, Value::Bytes(_)) => {
                return Err(Error::Violation("key-value length exceeds 65535"));
            }
            _ => {
                return Err(Error::KeyValueFormatting(
                    "even types carry an integer, odd types carry bytes",
                ));
            }
        }
        prev = pair.ty;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(ty: u64, v: u64) -> Kvp<'static> {
        Kvp {
            ty,
            value: Value::Int(v),
        }
    }

    fn bytes(ty: u64, v: &'static [u8]) -> Kvp<'static> {
        Kvp {
            ty,
            value: Value::Bytes(v),
        }
    }

    fn enc(pairs: &[Kvp<'_>]) -> Vec<u8> {
        let mut out = vec![];
        encode_kvps(pairs, &mut out).unwrap();
        out
    }

    #[test]
    fn types_are_written_as_deltas() {
        let pairs = [int(2, 7), bytes(5, b"ab"), bytes(5, b"c"), int(0x3f02, 87)];
        let wire = enc(&pairs);
        // 2:7 | +3 len 2 "ab" | +0 len 1 "c" | +0x3efd (two bytes) : 87
        assert_eq!(wire, [2, 7, 3, 2, b'a', b'b', 0, 1, b'c', 0xbe, 0xfd, 87]);
        assert_eq!(decode_kvps(&mut Reader::new(&wire)).unwrap(), pairs);
    }

    #[test]
    fn an_empty_region_is_no_pairs() {
        assert_eq!(decode_kvps(&mut Reader::new(&[])), Ok(vec![]));
        assert_eq!(enc(&[]), Vec::<u8>::new());
    }

    #[test]
    fn encoding_rejects_unsorted_pairs_and_mismatched_values() {
        let mut out = vec![];
        assert!(matches!(
            encode_kvps(&[int(4, 0), int(2, 0)], &mut out),
            Err(Error::Violation(_))
        ));
        assert!(matches!(
            encode_kvps(&[bytes(2, b"x")], &mut out),
            Err(Error::KeyValueFormatting(_))
        ));
        assert!(matches!(
            encode_kvps(&[int(3, 1)], &mut out),
            Err(Error::KeyValueFormatting(_))
        ));
    }

    #[test]
    fn decoding_rejects_a_type_overflow_and_an_oversized_length() {
        // type 2^64-2 (even) with value 0, then a delta of 2
        let mut wire = vec![];
        put_vi64(&mut wire, u64::MAX - 1);
        wire.extend_from_slice(&[0, 2, 0]);
        assert!(matches!(
            decode_kvps(&mut Reader::new(&wire)),
            Err(Error::Violation(_))
        ));
        // type 1 with length 65536
        let mut wire = vec![1];
        put_vi64(&mut wire, 65_536);
        assert!(matches!(
            decode_kvps(&mut Reader::new(&wire)),
            Err(Error::Violation(_))
        ));
    }

    #[test]
    fn a_pair_cut_short_inside_a_declared_region_is_a_violation() {
        let wire = enc(&[bytes(1, b"abcd")]);
        for cut in 1..wire.len() {
            let mut outer = Reader::new(&wire[..cut]);
            let mut region = outer.sub(cut).unwrap();
            assert!(
                matches!(decode_kvps(&mut region), Err(Error::Violation(_))),
                "cut at {cut}"
            );
        }
    }
}
