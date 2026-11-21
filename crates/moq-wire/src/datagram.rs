//! Object datagrams (section 11.2.1): one object per QUIC datagram. Audio travels this way.
//!
//! Decoding borrows from the datagram; nothing is copied. A khef forwards a datagram by
//! decoding it, changing the track alias, and encoding the header again in front of the same
//! payload.

use crate::{Error, KvpIter, Reader, Result, Value, put_vi64};

const PROPERTIES: u64 = 0x01;
const END_OF_GROUP: u64 = 0x02;
const ZERO_OBJECT_ID: u64 = 0x04;
const DEFAULT_PRIORITY: u64 = 0x08;
const STATUS: u64 = 0x20;
const KNOWN_FLAGS: u64 = PROPERTIES | END_OF_GROUP | ZERO_OBJECT_ID | DEFAULT_PRIORITY | STATUS;

/// Our object property carrying the speech level of an audio frame, 0 (silent) to 127.
///
/// It is in the range the draft reserves for applications (0x78 to 0x7F), and even, so its
/// value is one integer. Top-N audio selection reads it.
pub const PROPERTY_AUDIO_LEVEL: u64 = 0x78;

/// Why an object has no payload (section 11.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectStatus {
    /// An ordinary object that happens to be empty.
    Normal,
    /// No object at or after this one exists in the group.
    EndOfGroup,
    /// No object at or after this location exists in the track.
    EndOfTrack,
}

impl ObjectStatus {
    pub(crate) const fn code(self) -> u64 {
        match self {
            Self::Normal => 0x0,
            Self::EndOfGroup => 0x3,
            Self::EndOfTrack => 0x4,
        }
    }

    pub(crate) const fn from_code(code: u64) -> Result<Self> {
        match code {
            0x0 => Ok(Self::Normal),
            0x3 => Ok(Self::EndOfGroup),
            0x4 => Ok(Self::EndOfTrack),
            _ => Err(Error::Violation("unknown object status")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectBody<'a> {
    Payload(&'a [u8]),
    Status(ObjectStatus),
}

/// One object in a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Datagram<'a> {
    pub track_alias: u64,
    pub group: u64,
    pub object: u64,
    /// `None` means the priority set when the subscription was established.
    pub priority: Option<u8>,
    /// No object after this one exists in the group.
    pub end_of_group: bool,
    /// Object properties as encoded key-value pairs; empty means none.
    pub properties: &'a [u8],
    pub body: ObjectBody<'a>,
}

impl<'a> Datagram<'a> {
    fn check(&self) -> Result<()> {
        match self.body {
            ObjectBody::Status(_) if self.end_of_group => Err(Error::Violation(
                "a status datagram cannot also be marked end of group",
            )),
            ObjectBody::Status(status)
                if status != ObjectStatus::Normal && !self.properties.is_empty() =>
            {
                Err(Error::Violation("only normal objects can have properties"))
            }
            _ => Ok(()),
        }
    }

    /// Append everything except the payload. For a status datagram this is the whole thing.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Violation`], appending nothing, if a status datagram is marked end
    /// of group, or a status other than normal has properties.
    pub fn encode_header(&self, out: &mut Vec<u8>) -> Result<()> {
        self.check()?;
        let mut flags = 0;
        if !self.properties.is_empty() {
            flags |= PROPERTIES;
        }
        if self.end_of_group {
            flags |= END_OF_GROUP;
        }
        if self.object == 0 {
            flags |= ZERO_OBJECT_ID;
        }
        if self.priority.is_none() {
            flags |= DEFAULT_PRIORITY;
        }
        if matches!(self.body, ObjectBody::Status(_)) {
            flags |= STATUS;
        }
        put_vi64(out, flags);
        put_vi64(out, self.track_alias);
        put_vi64(out, self.group);
        if self.object != 0 {
            put_vi64(out, self.object);
        }
        if let Some(priority) = self.priority {
            out.push(priority);
        }
        if !self.properties.is_empty() {
            put_vi64(out, self.properties.len() as u64);
            out.extend_from_slice(self.properties);
        }
        if let ObjectBody::Status(status) = self.body {
            put_vi64(out, status.code());
        }
        Ok(())
    }

    /// Append the whole datagram.
    ///
    /// # Errors
    ///
    /// Fails as [`Datagram::encode_header`] does.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        self.encode_header(out)?;
        if let ObjectBody::Payload(payload) = self.body {
            out.extend_from_slice(payload);
        }
        Ok(())
    }

    /// Decode a whole datagram.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Violation`] on an unknown datagram type, a status marked end of
    /// group or followed by a payload, an unknown status, malformed or misplaced properties,
    /// or a datagram that ends early. A datagram is complete by definition, so running out of
    /// bytes is a violation, never "need more".
    pub fn decode(datagram: &'a [u8]) -> Result<Self> {
        let mut outer = Reader::new(datagram);
        let mut r = outer.sub(datagram.len())?;
        let flags = r.vi64()?;
        if flags & !KNOWN_FLAGS != 0 {
            return Err(Error::Violation("unknown datagram type"));
        }
        let has = |bit: u64| flags & bit != 0;
        if has(STATUS) && has(END_OF_GROUP) {
            return Err(Error::Violation(
                "a status datagram cannot also be marked end of group",
            ));
        }
        let track_alias = r.vi64()?;
        let group = r.vi64()?;
        let object = if has(ZERO_OBJECT_ID) { 0 } else { r.vi64()? };
        let priority = if has(DEFAULT_PRIORITY) {
            None
        } else {
            Some(r.u8()?)
        };
        let properties = if has(PROPERTIES) {
            let len = r.vi64()?;
            if len == 0 {
                return Err(Error::Violation("properties flagged present but empty"));
            }
            let region = r.sub(usize::try_from(len).unwrap_or(usize::MAX))?;
            let bytes = region.clone().rest();
            for pair in KvpIter::new(region) {
                pair?;
            }
            bytes
        } else {
            &[]
        };
        let body = if has(STATUS) {
            let status = ObjectStatus::from_code(r.vi64()?)?;
            if !r.is_empty() {
                return Err(Error::Violation("a status datagram carries no payload"));
            }
            ObjectBody::Status(status)
        } else {
            ObjectBody::Payload(r.rest())
        };
        let datagram = Self {
            track_alias,
            group,
            object,
            priority,
            end_of_group: has(END_OF_GROUP),
            properties,
            body,
        };
        datagram.check()?;
        Ok(datagram)
    }
}

/// The value of the first integer property of type `ty` in encoded properties, without
/// allocating. `Ok(None)` if there is none.
///
/// # Errors
///
/// Fails if a malformed pair is reached before the search ends.
pub fn int_property(properties: &[u8], ty: u64) -> Result<Option<u64>> {
    let mut outer = Reader::new(properties);
    for pair in KvpIter::new(outer.sub(properties.len())?) {
        let pair = pair?;
        if pair.ty == ty {
            return match pair.value {
                Value::Int(v) => Ok(Some(v)),
                Value::Bytes(_) => Ok(None),
            };
        }
        if pair.ty > ty {
            break; // types only increase
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kvp, encode_kvps};

    fn level(v: u64) -> Vec<u8> {
        let mut out = vec![];
        encode_kvps(
            &[Kvp {
                ty: PROPERTY_AUDIO_LEVEL,
                value: Value::Int(v),
            }],
            &mut out,
        )
        .unwrap();
        out
    }

    fn wire(d: &Datagram<'_>) -> Vec<u8> {
        let mut out = vec![];
        d.encode(&mut out).unwrap();
        out
    }

    #[test]
    fn an_audio_datagram_has_a_seven_byte_header() {
        let props = level(87);
        let payload = [0xaau8; 80];
        let d = Datagram {
            track_alias: 41,
            group: 12_345,
            object: 0,
            priority: None,
            end_of_group: true,
            properties: &props,
            body: ObjectBody::Payload(&payload),
        };
        let bytes = wire(&d);
        // flags, alias, group (2), properties length, property type, level
        assert_eq!(&bytes[..7], [0x0f, 41, 0xb0, 0x39, 2, 0x78, 87]);
        assert_eq!(bytes.len(), 7 + 80);
        assert_eq!(Datagram::decode(&bytes), Ok(d));
        assert_eq!(
            int_property(d.properties, PROPERTY_AUDIO_LEVEL),
            Ok(Some(87))
        );
    }

    #[test]
    fn every_combination_of_optional_fields_round_trips() {
        let props = level(5);
        for bits in 0..16u8 {
            let d = Datagram {
                track_alias: 300,
                group: 9,
                object: if bits & 1 != 0 { 7 } else { 0 },
                priority: (bits & 2 != 0).then_some(200),
                end_of_group: bits & 4 != 0,
                properties: if bits & 8 != 0 { &props } else { &[] },
                body: ObjectBody::Payload(b"opus"),
            };
            assert_eq!(Datagram::decode(&wire(&d)), Ok(d), "bits {bits:04b}");
        }
    }

    #[test]
    fn status_datagrams_round_trip_and_an_empty_payload_is_allowed() {
        for status in [
            ObjectStatus::Normal,
            ObjectStatus::EndOfGroup,
            ObjectStatus::EndOfTrack,
        ] {
            let d = Datagram {
                track_alias: 1,
                group: 2,
                object: 3,
                priority: Some(0),
                end_of_group: false,
                properties: &[],
                body: ObjectBody::Status(status),
            };
            assert_eq!(Datagram::decode(&wire(&d)), Ok(d));
        }
        let empty = Datagram {
            track_alias: 1,
            group: 2,
            object: 0,
            priority: None,
            end_of_group: false,
            properties: &[],
            body: ObjectBody::Payload(&[]),
        };
        assert_eq!(Datagram::decode(&wire(&empty)), Ok(empty));
    }

    #[test]
    fn forwarding_changes_only_the_alias() {
        let props = level(60);
        let original = Datagram {
            track_alias: 41,
            group: 500,
            object: 0,
            priority: Some(32),
            end_of_group: true,
            properties: &props,
            body: ObjectBody::Payload(b"frame"),
        };
        let bytes = wire(&original);
        let received = Datagram::decode(&bytes).unwrap();
        let mut out = vec![];
        Datagram {
            track_alias: 9,
            ..received
        }
        .encode(&mut out)
        .unwrap();
        assert_eq!(
            Datagram::decode(&out),
            Ok(Datagram {
                track_alias: 9,
                ..original
            })
        );
    }

    #[test]
    fn invalid_datagrams_are_violations() {
        let cases: &[(&[u8], &str)] = &[
            (&[0x10, 1, 1, 1, 0], "unknown datagram type"),
            (&[0x40, 1, 1, 1, 0], "unknown datagram type"),
            (&[0x80, 0x80, 1, 1], "unknown datagram type"),
            (
                &[0x2e, 1, 1, 0],
                "a status datagram cannot also be marked end of group",
            ),
            (&[0x0d, 1, 1, 0], "properties flagged present but empty"),
            (&[0x2c, 1, 1, 9], "unknown object status"),
            (
                &[0x2c, 1, 1, 0, 0xff],
                "a status datagram carries no payload",
            ),
            (
                &[0x2d, 1, 1, 2, 0x78, 5, 3],
                "only normal objects can have properties",
            ),
        ];
        for (bytes, why) in cases {
            assert_eq!(
                Datagram::decode(bytes),
                Err(Error::Violation(why)),
                "{bytes:x?}"
            );
        }
    }

    #[test]
    fn a_truncated_datagram_is_a_violation_not_a_wait() {
        let props = level(87);
        let d = Datagram {
            track_alias: 41,
            group: 12_345,
            object: 6,
            priority: Some(1),
            end_of_group: false,
            properties: &props,
            body: ObjectBody::Payload(&[]),
        };
        let bytes = wire(&d);
        for cut in 0..bytes.len() {
            assert!(
                matches!(Datagram::decode(&bytes[..cut]), Err(Error::Violation(_))),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn encoding_refuses_what_decoding_would_refuse() {
        let props = level(1);
        let base = Datagram {
            track_alias: 1,
            group: 1,
            object: 0,
            priority: None,
            end_of_group: true,
            properties: &[],
            body: ObjectBody::Status(ObjectStatus::Normal),
        };
        assert!(matches!(base.encode(&mut vec![]), Err(Error::Violation(_))));
        let with_props = Datagram {
            end_of_group: false,
            properties: &props,
            body: ObjectBody::Status(ObjectStatus::EndOfTrack),
            ..base
        };
        assert!(matches!(
            with_props.encode(&mut vec![]),
            Err(Error::Violation(_))
        ));
    }

    #[test]
    fn int_property_finds_the_type_or_reports_none() {
        let mut props = vec![];
        let pairs = [
            Kvp {
                ty: 0x02,
                value: Value::Int(9),
            },
            Kvp {
                ty: 0x0b,
                value: Value::Bytes(b"immutable"),
            },
            Kvp {
                ty: PROPERTY_AUDIO_LEVEL,
                value: Value::Int(64),
            },
        ];
        encode_kvps(&pairs, &mut props).unwrap();
        assert_eq!(int_property(&props, PROPERTY_AUDIO_LEVEL), Ok(Some(64)));
        assert_eq!(int_property(&props, 0x02), Ok(Some(9)));
        assert_eq!(int_property(&props, 0x04), Ok(None));
        assert_eq!(int_property(&props, 0x0b), Ok(None));
        assert_eq!(int_property(&[], PROPERTY_AUDIO_LEVEL), Ok(None));
        assert!(matches!(
            int_property(&[1, 9], PROPERTY_AUDIO_LEVEL),
            Err(Error::Violation(_))
        ));
    }
}
