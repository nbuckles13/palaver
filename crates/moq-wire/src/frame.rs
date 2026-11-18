//! Control message framing (section 9): a type, a 16-bit body length, then the body.

use crate::{Error, Reader, Result, put_vi64};

/// Longest control message body.
pub const MAX_BODY_BYTES: usize = 65_535;

/// One control message as it sits on a control or request stream, body not yet decoded.
#[derive(Debug, Clone)]
pub struct Frame<'a> {
    pub ty: u64,
    /// Covers exactly the body, so running out of bytes inside it is a violation.
    pub body: Reader<'a>,
}

impl<'a> Frame<'a> {
    /// Read one whole message. Returns [`Error::NeedMore`], consuming nothing, until the full
    /// body has arrived.
    pub fn decode(r: &mut Reader<'a>) -> Result<Self> {
        let mut probe = r.clone();
        let ty = probe.vi64()?;
        let len = probe.take(2)?;
        let len = usize::from(u16::from_be_bytes([
            len.first().copied().unwrap_or(0),
            len.get(1).copied().unwrap_or(0),
        ]));
        let body = probe.sub(len)?;
        *r = probe;
        Ok(Self { ty, body })
    }

    /// Call after decoding a body: bytes left over mean the length did not match the message.
    pub fn finish(self) -> Result<()> {
        if self.body.is_empty() {
            Ok(())
        } else {
            Err(Error::Violation("message length does not match its body"))
        }
    }
}

/// Append a framed message.
pub fn encode_frame(ty: u64, body: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let len = u16::try_from(body.len())
        .map_err(|_| Error::Violation("message body exceeds 65535 bytes"))?;
    put_vi64(out, ty);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_and_leaves_what_follows() {
        let mut wire = vec![];
        encode_frame(0x2f00, b"body", &mut wire).unwrap();
        assert_eq!(wire, [0xaf, 0x00, 0x00, 0x04, b'b', b'o', b'd', b'y']);
        wire.push(0x99);
        let mut r = Reader::new(&wire);
        let mut frame = Frame::decode(&mut r).unwrap();
        assert_eq!(frame.ty, 0x2f00);
        assert_eq!(frame.body.rest(), b"body");
        assert_eq!(r.rest(), [0x99]);
    }

    #[test]
    fn an_incomplete_frame_asks_for_more_and_consumes_nothing() {
        let mut wire = vec![];
        encode_frame(3, b"abcdef", &mut wire).unwrap();
        for cut in 0..wire.len() {
            let mut r = Reader::new(&wire[..cut]);
            assert!(
                matches!(Frame::decode(&mut r), Err(Error::NeedMore)),
                "cut at {cut}"
            );
            assert_eq!(r.remaining(), cut);
        }
    }

    #[test]
    fn reading_past_the_body_is_a_violation_and_leftover_bytes_fail_finish() {
        let mut wire = vec![];
        encode_frame(3, &[0x07], &mut wire).unwrap();
        let mut frame = Frame::decode(&mut Reader::new(&wire)).unwrap();
        assert!(matches!(frame.clone().finish(), Err(Error::Violation(_))));
        assert_eq!(frame.body.vi64(), Ok(7));
        assert!(matches!(frame.body.vi64(), Err(Error::Violation(_))));
        assert_eq!(frame.finish(), Ok(()));
    }

    #[test]
    fn a_body_over_the_limit_is_refused() {
        let mut out = vec![];
        assert!(encode_frame(3, &vec![0; MAX_BODY_BYTES], &mut out).is_ok());
        assert!(matches!(
            encode_frame(3, &vec![0; MAX_BODY_BYTES + 1], &mut vec![]),
            Err(Error::Violation(_))
        ));
    }
}
