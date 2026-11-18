//! Subgroup streams (section 11.3): a header, then objects of one subgroup in order, on a
//! unidirectional stream. Video travels this way, and so do control tracks, which are too
//! large for datagrams.

use crate::datagram::{ObjectBody, ObjectStatus};
use crate::{Error, KvpIter, Reader, Result, SETUP, put_vi64};

const PROPERTIES: u64 = 0x01;
const SUBGROUP_ID_MODE: u64 = 0x06;
const END_OF_GROUP: u64 = 0x08;
const ALWAYS_SET: u64 = 0x10;
const DEFAULT_PRIORITY: u64 = 0x20;
const FIRST_OBJECT: u64 = 0x40;

/// What a unidirectional stream is, from the integer it starts with (section 6.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniStream {
    /// The control stream: it starts with SETUP.
    Control,
    /// A subgroup stream: the integer is the subgroup header's type flags.
    Subgroup,
}

/// Classify a unidirectional stream by its first integer. Fetch and padding streams are not
/// in this profile; they and unknown types close the session.
pub fn uni_stream(first: u64) -> Result<UniStream> {
    if first == SETUP {
        Ok(UniStream::Control)
    } else if first < 0x80 && first & ALWAYS_SET != 0 {
        Ok(UniStream::Subgroup)
    } else {
        Err(Error::Violation("unknown or unsupported stream type"))
    }
}

/// How the subgroup id is conveyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubgroupId {
    /// Subgroup 0; nothing on the wire.
    Zero,
    /// The id of the first object on the stream; nothing on the wire.
    FirstObject,
    /// Written in the header.
    Explicit(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubgroupHeader {
    pub track_alias: u64,
    pub group: u64,
    pub subgroup: SubgroupId,
    /// `None` means the priority set when the subscription was established.
    pub priority: Option<u8>,
    /// Whether every object on the stream carries a properties field (possibly empty).
    pub has_properties: bool,
    /// This subgroup holds the last object of the group, so a clean end of stream means the
    /// group is complete.
    pub end_of_group: bool,
    /// The first object on the stream is the first the original publisher put in the subgroup.
    pub first_object: bool,
}

impl SubgroupHeader {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mode = match self.subgroup {
            SubgroupId::Zero => 0,
            SubgroupId::FirstObject => 1,
            SubgroupId::Explicit(_) => 2,
        };
        let mut flags = ALWAYS_SET | mode << 1;
        if self.has_properties {
            flags |= PROPERTIES;
        }
        if self.end_of_group {
            flags |= END_OF_GROUP;
        }
        if self.priority.is_none() {
            flags |= DEFAULT_PRIORITY;
        }
        if self.first_object {
            flags |= FIRST_OBJECT;
        }
        put_vi64(out, flags);
        put_vi64(out, self.track_alias);
        put_vi64(out, self.group);
        if let SubgroupId::Explicit(id) = self.subgroup {
            put_vi64(out, id);
        }
        if let Some(priority) = self.priority {
            out.push(priority);
        }
    }

    /// Read a header from the start of a stream. [`Error::NeedMore`], consuming nothing, until
    /// all of it has arrived.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let mut probe = r.clone();
        let flags = probe.vi64()?;
        if flags >= 0x80 || flags & ALWAYS_SET == 0 {
            return Err(Error::Violation("not a subgroup header"));
        }
        let has = |bit: u64| flags & bit != 0;
        let track_alias = probe.vi64()?;
        let group = probe.vi64()?;
        let subgroup = match (flags & SUBGROUP_ID_MODE) >> 1 {
            0 => SubgroupId::Zero,
            1 => SubgroupId::FirstObject,
            2 => SubgroupId::Explicit(probe.vi64()?),
            _ => return Err(Error::Violation("reserved subgroup id mode")),
        };
        let priority = if has(DEFAULT_PRIORITY) {
            None
        } else {
            Some(probe.u8()?)
        };
        *r = probe;
        Ok(Self {
            track_alias,
            group,
            subgroup,
            priority,
            has_properties: has(PROPERTIES),
            end_of_group: has(END_OF_GROUP),
            first_object: has(FIRST_OBJECT),
        })
    }
}

/// One object on a subgroup stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubgroupObject<'a> {
    pub object: u64,
    /// Object properties as encoded key-value pairs; empty means none.
    pub properties: &'a [u8],
    /// A payload, or a status when there is none. An empty payload is written as status Normal
    /// and reads back as `Status(Normal)`.
    pub body: ObjectBody<'a>,
}

/// Writes the objects of one subgroup stream. Object ids on the wire are deltas from the
/// previous object, so objects must be written in increasing id order.
#[derive(Debug, Clone)]
pub struct SubgroupWriter {
    has_properties: bool,
    previous: Option<u64>,
}

impl SubgroupWriter {
    pub fn new(header: &SubgroupHeader) -> Self {
        Self {
            has_properties: header.has_properties,
            previous: None,
        }
    }

    /// Append everything except the payload bytes. For an object with no payload this is the
    /// whole object. Lets a khef forward a large payload without copying it into `out`.
    pub fn write_prefix(&mut self, object: &SubgroupObject<'_>, out: &mut Vec<u8>) -> Result<()> {
        let delta = match self.previous {
            None => object.object,
            Some(previous) => object
                .object
                .checked_sub(previous)
                .and_then(|d| d.checked_sub(1))
                .ok_or(Error::Violation(
                    "object ids on a subgroup stream must increase",
                ))?,
        };
        let status = match object.body {
            ObjectBody::Payload([]) => Some(ObjectStatus::Normal),
            ObjectBody::Payload(_) => None,
            ObjectBody::Status(status) => Some(status),
        };
        if !object.properties.is_empty() {
            if !self.has_properties {
                return Err(Error::Violation(
                    "this subgroup's header says objects carry no properties",
                ));
            }
            if status.is_some_and(|s| s != ObjectStatus::Normal) {
                return Err(Error::Violation("only normal objects can have properties"));
            }
        }
        put_vi64(out, delta);
        if self.has_properties {
            put_vi64(out, object.properties.len() as u64);
            out.extend_from_slice(object.properties);
        }
        match (status, object.body) {
            (Some(status), _) => {
                put_vi64(out, 0);
                put_vi64(out, status.code());
            }
            (None, ObjectBody::Payload(payload)) => put_vi64(out, payload.len() as u64),
            (None, ObjectBody::Status(_)) => {} // unreachable: a status body always has a status
        }
        self.previous = Some(object.object);
        Ok(())
    }

    /// Append the whole object.
    pub fn write(&mut self, object: &SubgroupObject<'_>, out: &mut Vec<u8>) -> Result<()> {
        self.write_prefix(object, out)?;
        if let ObjectBody::Payload(payload) = object.body {
            out.extend_from_slice(payload);
        }
        Ok(())
    }
}

/// Reads the objects of one subgroup stream.
#[derive(Debug, Clone)]
pub struct SubgroupReader {
    has_properties: bool,
    previous: Option<u64>,
}

impl SubgroupReader {
    pub fn new(header: &SubgroupHeader) -> Self {
        Self {
            has_properties: header.has_properties,
            previous: None,
        }
    }

    /// Read the next object. [`Error::NeedMore`], consuming nothing and changing nothing,
    /// until the whole object has arrived.
    pub fn read<'a>(&mut self, r: &mut Reader<'a>) -> Result<SubgroupObject<'a>> {
        let mut probe = r.clone();
        let delta = probe.vi64()?;
        let object = match self.previous {
            None => Some(delta),
            Some(previous) => previous.checked_add(delta).and_then(|id| id.checked_add(1)),
        }
        .ok_or(Error::Violation("object id exceeds 2^64 - 1"))?;
        let properties = if self.has_properties {
            let len = probe.vi64()?;
            let bytes = probe.take(usize::try_from(len).unwrap_or(usize::MAX))?;
            let mut outer = Reader::new(bytes);
            for pair in KvpIter::new(outer.sub(bytes.len())?) {
                pair?;
            }
            bytes
        } else {
            &[]
        };
        let len = probe.vi64()?;
        let body = if len == 0 {
            let status = ObjectStatus::from_code(probe.vi64()?)?;
            if status != ObjectStatus::Normal && !properties.is_empty() {
                return Err(Error::Violation("only normal objects can have properties"));
            }
            ObjectBody::Status(status)
        } else {
            ObjectBody::Payload(probe.take(usize::try_from(len).unwrap_or(usize::MAX))?)
        };
        *r = probe;
        self.previous = Some(object);
        Ok(SubgroupObject {
            object,
            properties,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kvp, Value, encode_kvps};

    fn header() -> SubgroupHeader {
        SubgroupHeader {
            track_alias: 41,
            group: 12_345,
            subgroup: SubgroupId::Zero,
            priority: None,
            has_properties: true,
            end_of_group: true,
            first_object: true,
        }
    }

    fn props() -> Vec<u8> {
        let mut out = vec![];
        encode_kvps(
            &[Kvp {
                ty: 0x3802,
                value: Value::Int(7),
            }],
            &mut out,
        )
        .unwrap();
        out
    }

    #[test]
    fn every_header_shape_round_trips() {
        for bits in 0..16u8 {
            for subgroup in [
                SubgroupId::Zero,
                SubgroupId::FirstObject,
                SubgroupId::Explicit(9),
            ] {
                let h = SubgroupHeader {
                    track_alias: 300,
                    group: 7,
                    subgroup,
                    priority: (bits & 1 != 0).then_some(200),
                    has_properties: bits & 2 != 0,
                    end_of_group: bits & 4 != 0,
                    first_object: bits & 8 != 0,
                };
                let mut out = vec![];
                h.encode(&mut out);
                assert_eq!(uni_stream(u64::from(out[0])), Ok(UniStream::Subgroup));
                let mut r = Reader::new(&out);
                assert_eq!(SubgroupHeader::decode(&mut r), Ok(h), "{h:?}");
                assert!(r.is_empty());
            }
        }
    }

    #[test]
    fn a_control_track_object_stream_has_the_expected_bytes() {
        // One object per group, as reports and plans are sent.
        let mut out = vec![];
        header().encode(&mut out);
        let mut w = SubgroupWriter::new(&header());
        w.write(
            &SubgroupObject {
                object: 0,
                properties: &[],
                body: ObjectBody::Payload(b"plan"),
            },
            &mut out,
        )
        .unwrap();
        // flags 0x79 (properties, end of group, always-set, default priority, first object),
        // alias, group (2 bytes) | delta 0, properties length 0, payload length 4, payload
        assert_eq!(out, [0x79, 41, 0xb0, 0x39, 0, 0, 4, b'p', b'l', b'a', b'n']);
    }

    #[test]
    fn objects_round_trip_with_gaps_statuses_and_properties() {
        let p = props();
        let objects = [
            SubgroupObject {
                object: 0,
                properties: &p,
                body: ObjectBody::Payload(b"key frame"),
            },
            SubgroupObject {
                object: 1,
                properties: &[],
                body: ObjectBody::Payload(b"delta"),
            },
            SubgroupObject {
                object: 5,
                properties: &[],
                body: ObjectBody::Payload(b"after a gap"),
            },
            SubgroupObject {
                object: 6,
                properties: &p,
                body: ObjectBody::Status(ObjectStatus::Normal),
            },
            SubgroupObject {
                object: 7,
                properties: &[],
                body: ObjectBody::Status(ObjectStatus::EndOfGroup),
            },
        ];
        let mut out = vec![];
        let mut w = SubgroupWriter::new(&header());
        for o in &objects {
            w.write(o, &mut out).unwrap();
        }
        let mut r = Reader::new(&out);
        let mut reader = SubgroupReader::new(&header());
        for o in &objects {
            assert_eq!(reader.read(&mut r), Ok(*o));
        }
        assert!(r.is_empty());
    }

    #[test]
    fn an_empty_payload_is_sent_as_status_normal() {
        let mut out = vec![];
        let mut w = SubgroupWriter::new(&header());
        w.write(
            &SubgroupObject {
                object: 3,
                properties: &[],
                body: ObjectBody::Payload(&[]),
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(out, [3, 0, 0, 0]);
        let got = SubgroupReader::new(&header())
            .read(&mut Reader::new(&out))
            .unwrap();
        assert_eq!(got.body, ObjectBody::Status(ObjectStatus::Normal));
    }

    #[test]
    fn a_stream_without_properties_omits_the_field() {
        let h = SubgroupHeader {
            has_properties: false,
            ..header()
        };
        let mut out = vec![];
        let mut w = SubgroupWriter::new(&h);
        let o = SubgroupObject {
            object: 2,
            properties: &[],
            body: ObjectBody::Payload(b"x"),
        };
        w.write(&o, &mut out).unwrap();
        assert_eq!(out, [2, 1, b'x']);
        assert_eq!(SubgroupReader::new(&h).read(&mut Reader::new(&out)), Ok(o));
        let p = props();
        let with = SubgroupObject {
            properties: &p,
            ..o
        };
        assert!(matches!(
            SubgroupWriter::new(&h).write(&with, &mut vec![]),
            Err(Error::Violation(_))
        ));
    }

    #[test]
    fn a_partial_object_asks_for_more_and_leaves_the_reader_unchanged() {
        let p = props();
        let objects = [
            SubgroupObject {
                object: 4,
                properties: &p,
                body: ObjectBody::Payload(b"first"),
            },
            SubgroupObject {
                object: 5,
                properties: &[],
                body: ObjectBody::Payload(b"second"),
            },
        ];
        let mut out = vec![];
        let mut w = SubgroupWriter::new(&header());
        w.write(&objects[0], &mut out).unwrap();
        let first_len = out.len();
        w.write(&objects[1], &mut out).unwrap();
        // Feed the stream one byte at a time, as a transport might.
        let mut reader = SubgroupReader::new(&header());
        let mut got = vec![];
        let mut start = 0;
        for end in 0..=out.len() {
            let mut r = Reader::new(&out[start..end]);
            match reader.read(&mut r) {
                Ok(o) => {
                    got.push((o.object, end));
                    start = end;
                }
                Err(e) => assert_eq!(e, Error::NeedMore, "at {end}"),
            }
        }
        assert_eq!(got, [(4, first_len), (5, out.len())]);
    }

    #[test]
    fn a_partial_header_asks_for_more_and_consumes_nothing() {
        let h = SubgroupHeader {
            subgroup: SubgroupId::Explicit(700),
            priority: Some(1),
            ..header()
        };
        let mut out = vec![];
        h.encode(&mut out);
        for cut in 0..out.len() {
            let mut r = Reader::new(&out[..cut]);
            assert_eq!(SubgroupHeader::decode(&mut r), Err(Error::NeedMore));
            assert_eq!(r.remaining(), cut);
        }
    }

    #[test]
    fn invalid_streams_are_violations() {
        // header: bit 4 clear; two-byte type; reserved subgroup id mode
        for bytes in [&[0x01u8, 1, 1][..], &[0x80, 0x90, 1, 1], &[0x16, 1, 1]] {
            assert!(
                matches!(
                    SubgroupHeader::decode(&mut Reader::new(bytes)),
                    Err(Error::Violation(_))
                ),
                "{bytes:x?}"
            );
        }
        fn read(bytes: &[u8]) -> Result<SubgroupObject<'_>> {
            SubgroupReader::new(&header()).read(&mut Reader::new(bytes))
        }
        // unknown status; properties on a non-normal status; malformed properties
        assert_eq!(
            read(&[0, 0, 0, 9]),
            Err(Error::Violation("unknown object status"))
        );
        assert_eq!(
            read(&[0, 2, 0x78, 5, 0, 3]),
            Err(Error::Violation("only normal objects can have properties"))
        );
        assert!(matches!(
            read(&[0, 2, 1, 9, 1, b'x']),
            Err(Error::Violation(_))
        ));
        // object id overflow on the second object
        let mut reader = SubgroupReader::new(&header());
        let mut first = vec![];
        put_vi64(&mut first, u64::MAX);
        first.extend_from_slice(&[0, 1, b'x']);
        assert!(reader.read(&mut Reader::new(&first)).is_ok());
        assert_eq!(
            reader.read(&mut Reader::new(&[0, 0, 1, b'y'])),
            Err(Error::Violation("object id exceeds 2^64 - 1"))
        );
    }

    #[test]
    fn the_writer_refuses_ids_that_do_not_increase_and_properties_on_a_status() {
        let mut w = SubgroupWriter::new(&header());
        let o = |object| SubgroupObject {
            object,
            properties: &[],
            body: ObjectBody::Payload(b"x"),
        };
        w.write(&o(5), &mut vec![]).unwrap();
        assert!(matches!(
            w.write(&o(5), &mut vec![]),
            Err(Error::Violation(_))
        ));
        assert!(matches!(
            w.write(&o(4), &mut vec![]),
            Err(Error::Violation(_))
        ));
        w.write(&o(6), &mut vec![]).unwrap();
        let p = props();
        let bad = SubgroupObject {
            object: 9,
            properties: &p,
            body: ObjectBody::Status(ObjectStatus::EndOfTrack),
        };
        assert!(matches!(
            w.write(&bad, &mut vec![]),
            Err(Error::Violation(_))
        ));
    }

    #[test]
    fn stream_types_are_classified() {
        assert_eq!(uni_stream(0x2f00), Ok(UniStream::Control));
        for flags in [0x10u64, 0x1f, 0x30, 0x79, 0x7f] {
            assert_eq!(uni_stream(flags), Ok(UniStream::Subgroup), "{flags:#x}");
        }
        // fetch header, padding, a type flag without bit 4, a two-byte value with bit 4
        for other in [0x05u64, 0x132b_3e28, 0x01, 0x90, 0] {
            assert!(
                matches!(uni_stream(other), Err(Error::Violation(_))),
                "{other:#x}"
            );
        }
    }
}
