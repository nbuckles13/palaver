//! SETUP (section 9.1): the first message each endpoint sends on its control stream.
//!
//! The body is a run of key-value pairs called setup options. Unknown options are ignored,
//! which is what lets an extension declare itself: a peer that does not know the option skips
//! it, and the extension is in force only when both sides sent it (section 6.3.2).

use crate::frame::{Frame, encode_frame};
use crate::{Error, Kvp, Reader, Result, Value, decode_kvps, encode_kvps, put_vi64};

/// Message type of SETUP.
pub const SETUP: u64 = 0x2f00;

const MAX_FILTER_RANGES: u64 = 0x06;
const MOQT_IMPLEMENTATION: u64 = 0x07;
const MAX_REQUEST_UPDATES: u64 = 0x08;
/// Our extension's setup option ("PALW"): odd, so it carries bytes, and not a grease value.
pub const PALAVER_EXTENSIONS: u64 = 0x5041_4c57;

/// What a Palaver endpoint declares about our extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PalaverExtensions {
    /// Version of the extension set the sender implements.
    pub version: u64,
    /// The N the sender uses for top-N audio selection; 0 if it does not use it.
    pub audio_top_n: u64,
}

impl PalaverExtensions {
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![];
        put_vi64(&mut out, self.version);
        put_vi64(&mut out, self.audio_top_n);
        out
    }

    /// Fields a later version appends after the ones known here are ignored.
    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut outer = Reader::new(bytes);
        let mut r = outer.sub(bytes.len())?;
        Ok(Self {
            version: r.vi64()?,
            audio_top_n: r.vi64()?,
        })
    }
}

/// The setup options our profile uses. Options we do not use (PATH, AUTHORITY, authorization
/// tokens) are neither sent nor read; like any unknown option they are ignored on receipt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Setup {
    /// Name and version of the sender's implementation.
    pub implementation: Option<String>,
    /// How many filter ranges the sender accepts per request; 0 when absent.
    pub max_filter_ranges: u64,
    /// How many unanswered REQUEST_UPDATEs the sender accepts per request stream; 0 (no limit)
    /// when absent.
    pub max_request_updates: u64,
    /// Present when the sender is a Palaver endpoint.
    pub palaver: Option<PalaverExtensions>,
}

impl Setup {
    /// Append the whole message, framing included.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let palaver = self.palaver.map(|p| p.encode());
        let mut pairs = vec![];
        if self.max_filter_ranges != 0 {
            pairs.push(Kvp {
                ty: MAX_FILTER_RANGES,
                value: Value::Int(self.max_filter_ranges),
            });
        }
        if let Some(name) = &self.implementation {
            pairs.push(Kvp {
                ty: MOQT_IMPLEMENTATION,
                value: Value::Bytes(name.as_bytes()),
            });
        }
        if self.max_request_updates != 0 {
            pairs.push(Kvp {
                ty: MAX_REQUEST_UPDATES,
                value: Value::Int(self.max_request_updates),
            });
        }
        if let Some(bytes) = &palaver {
            pairs.push(Kvp {
                ty: PALAVER_EXTENSIONS,
                value: Value::Bytes(bytes),
            });
        }
        let mut body = vec![];
        encode_kvps(&pairs, &mut body)?;
        encode_frame(SETUP, &body, out)
    }

    /// Decode the body of a frame whose type is [`SETUP`].
    pub fn decode(mut frame: Frame<'_>) -> Result<Self> {
        if frame.ty != SETUP {
            return Err(Error::Violation("not a SETUP message"));
        }
        let mut setup = Self::default();
        let mut seen = [false; 4];
        for pair in decode_kvps(&mut frame.body)? {
            let slot = match pair.ty {
                MAX_FILTER_RANGES => 0,
                MOQT_IMPLEMENTATION => 1,
                MAX_REQUEST_UPDATES => 2,
                PALAVER_EXTENSIONS => 3,
                _ => continue, // unknown options, repeated or not, are ignored
            };
            if let Some(flag) = seen.get_mut(slot) {
                if *flag {
                    return Err(Error::Violation("setup option repeated"));
                }
                *flag = true;
            }
            match (pair.ty, pair.value) {
                (MAX_FILTER_RANGES, Value::Int(v)) => setup.max_filter_ranges = v,
                (MAX_REQUEST_UPDATES, Value::Int(v)) => setup.max_request_updates = v,
                (MOQT_IMPLEMENTATION, Value::Bytes(b)) => {
                    setup.implementation = Some(String::from_utf8_lossy(b).into_owned());
                }
                (PALAVER_EXTENSIONS, Value::Bytes(b)) => {
                    setup.palaver = Some(PalaverExtensions::decode(b)?)
                }
                _ => {
                    return Err(Error::KeyValueFormatting(
                        "setup option has the wrong kind of value",
                    ));
                }
            }
        }
        frame.finish()?;
        Ok(setup)
    }
}

/// The extension version both sides can use, if both declared our extensions: the lower of
/// the two. `None` means the peer is a plain draft 22 endpoint and none of our request
/// parameters may be sent to it.
pub fn negotiated_palaver_version(ours: &Setup, theirs: &Setup) -> Option<u64> {
    Some(ours.palaver?.version.min(theirs.palaver?.version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Setup {
        Setup {
            implementation: Some(String::from("palaver/0.0.0")),
            max_filter_ranges: 4,
            max_request_updates: 2,
            palaver: Some(PalaverExtensions {
                version: 1,
                audio_top_n: 3,
            }),
        }
    }

    fn wire(setup: &Setup) -> Vec<u8> {
        let mut out = vec![];
        setup.encode(&mut out).unwrap();
        out
    }

    fn parse(bytes: &[u8]) -> Result<Setup> {
        Setup::decode(Frame::decode(&mut Reader::new(bytes))?)
    }

    #[test]
    fn a_full_setup_round_trips() {
        assert_eq!(parse(&wire(&full())), Ok(full()));
    }

    #[test]
    fn an_empty_setup_is_just_the_frame_and_decodes_to_defaults() {
        let bytes = wire(&Setup::default());
        assert_eq!(bytes, [0xaf, 0x00, 0x00, 0x00]);
        assert_eq!(parse(&bytes), Ok(Setup::default()));
    }

    #[test]
    fn unknown_options_are_ignored_even_when_repeated() {
        let mut body = vec![];
        let pairs = [
            Kvp {
                ty: MAX_FILTER_RANGES,
                value: Value::Int(4),
            },
            Kvp {
                ty: 0x9d,
                value: Value::Bytes(b"grease"),
            },
            Kvp {
                ty: 0x9d,
                value: Value::Bytes(b"again"),
            },
            Kvp {
                ty: 0x1000,
                value: Value::Int(9),
            },
        ];
        encode_kvps(&pairs, &mut body).unwrap();
        let mut bytes = vec![];
        encode_frame(SETUP, &body, &mut bytes).unwrap();
        assert_eq!(
            parse(&bytes),
            Ok(Setup {
                max_filter_ranges: 4,
                ..Setup::default()
            })
        );
    }

    #[test]
    fn a_repeated_known_option_is_a_violation() {
        let mut body = vec![];
        let pairs = [
            Kvp {
                ty: MAX_REQUEST_UPDATES,
                value: Value::Int(1),
            },
            Kvp {
                ty: MAX_REQUEST_UPDATES,
                value: Value::Int(2),
            },
        ];
        encode_kvps(&pairs, &mut body).unwrap();
        let mut bytes = vec![];
        encode_frame(SETUP, &body, &mut bytes).unwrap();
        assert!(matches!(parse(&bytes), Err(Error::Violation(_))));
    }

    #[test]
    fn a_later_extension_version_with_extra_fields_still_decodes() {
        let mut value = vec![];
        for v in [2u64, 5, 99, 100] {
            put_vi64(&mut value, v);
        }
        let mut body = vec![];
        encode_kvps(
            &[Kvp {
                ty: PALAVER_EXTENSIONS,
                value: Value::Bytes(&value),
            }],
            &mut body,
        )
        .unwrap();
        let mut bytes = vec![];
        encode_frame(SETUP, &body, &mut bytes).unwrap();
        assert_eq!(
            parse(&bytes).unwrap().palaver,
            Some(PalaverExtensions {
                version: 2,
                audio_top_n: 5
            })
        );
    }

    #[test]
    fn a_truncated_extension_value_is_a_violation() {
        let mut body = vec![];
        encode_kvps(
            &[Kvp {
                ty: PALAVER_EXTENSIONS,
                value: Value::Bytes(&[1]),
            }],
            &mut body,
        )
        .unwrap();
        let mut bytes = vec![];
        encode_frame(SETUP, &body, &mut bytes).unwrap();
        assert!(matches!(parse(&bytes), Err(Error::Violation(_))));
    }

    #[test]
    fn another_message_type_is_not_a_setup() {
        let mut bytes = vec![];
        encode_frame(0x10, &[], &mut bytes).unwrap();
        assert!(matches!(parse(&bytes), Err(Error::Violation(_))));
    }

    #[test]
    fn extensions_are_negotiated_only_when_both_sides_declare_them() {
        let plain = Setup::default();
        let v1 = full();
        let v2 = Setup {
            palaver: Some(PalaverExtensions {
                version: 2,
                audio_top_n: 3,
            }),
            ..Setup::default()
        };
        assert_eq!(negotiated_palaver_version(&v1, &plain), None);
        assert_eq!(negotiated_palaver_version(&plain, &v1), None);
        assert_eq!(negotiated_palaver_version(&v1, &v1), Some(1));
        assert_eq!(negotiated_palaver_version(&v2, &v1), Some(1));
    }

    #[test]
    fn the_extension_option_number_is_odd_and_not_a_grease_value() {
        assert_eq!(PALAVER_EXTENSIONS % 2, 1);
        assert_ne!((PALAVER_EXTENSIONS - 0x9d) % 0x7f, 0);
    }
}
