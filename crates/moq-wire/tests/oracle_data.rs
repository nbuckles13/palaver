//! Cross-checks of object datagrams against moqtap-codec (draft 22): identical bytes, and each
//! side decodes the other's output.

// Helpers in an integration test file are test code, but clippy only exempts `#[test]` functions.
#![allow(clippy::unwrap_used)]

use bytes::BytesMut;
use moqtap_codec::draft22::data_stream::DatagramHeader;
use moqtap_codec::draft22::types::ObjectStatus as TapStatus;
use moqtap_codec::varint::VarInt;
use palaver_moq_wire::{
    Datagram, Kvp, ObjectBody, ObjectStatus, PROPERTY_AUDIO_LEVEL, Value, encode_kvps,
};

fn properties() -> Vec<u8> {
    let mut out = vec![];
    let pairs = [
        Kvp {
            ty: PROPERTY_AUDIO_LEVEL,
            value: Value::Int(87),
        },
        Kvp {
            ty: 0x3803,
            value: Value::Bytes(b"x"),
        },
    ];
    encode_kvps(&pairs, &mut out).unwrap();
    out
}

fn tap_status(status: ObjectStatus) -> TapStatus {
    match status {
        ObjectStatus::Normal => TapStatus::Normal,
        ObjectStatus::EndOfGroup => TapStatus::EndOfGroup,
        ObjectStatus::EndOfTrack => TapStatus::EndOfTrack,
    }
}

/// Build the oracle's header for the same object, with the type flags our encoder must choose.
fn tap_header(d: &Datagram<'_>) -> DatagramHeader {
    let mut flags = 0u8;
    if !d.properties.is_empty() {
        flags |= 0x01;
    }
    if d.end_of_group {
        flags |= 0x02;
    }
    if d.object == 0 {
        flags |= 0x04;
    }
    if d.priority.is_none() {
        flags |= 0x08;
    }
    let object_status = match d.body {
        ObjectBody::Status(s) => {
            flags |= 0x20;
            Some(tap_status(s))
        }
        ObjectBody::Payload(_) => None,
    };
    DatagramHeader {
        datagram_type: flags,
        track_alias: VarInt::from_u64_moqt(d.track_alias),
        group_id: VarInt::from_u64_moqt(d.group),
        object_id: VarInt::from_u64_moqt(d.object),
        publisher_priority: d.priority,
        properties: d.properties.to_vec(),
        object_status,
    }
}

fn assert_same(d: Datagram<'_>) {
    let mut ours = vec![];
    d.encode(&mut ours).unwrap();

    let header = tap_header(&d);
    let mut theirs = BytesMut::new();
    header.encode_checked(&mut theirs).unwrap();
    let payload: &[u8] = match d.body {
        ObjectBody::Payload(p) => p,
        ObjectBody::Status(_) => &[],
    };
    theirs.extend_from_slice(payload);
    assert_eq!(ours, theirs, "moqtap-codec encodes {d:?} differently");

    assert_eq!(Datagram::decode(&theirs), Ok(d));
    let (decoded, decoded_payload) = DatagramHeader::decode_object(&mut &ours[..]).unwrap();
    // The oracle's header has no equality, so compare it field by field.
    assert_eq!(decoded.datagram_type, header.datagram_type);
    assert_eq!(decoded.track_alias, header.track_alias);
    assert_eq!(decoded.group_id, header.group_id);
    assert_eq!(decoded.object_id, header.object_id);
    assert_eq!(decoded.publisher_priority, header.publisher_priority);
    assert_eq!(decoded.properties, header.properties);
    assert_eq!(
        decoded.object_status.map(|s| s as u64),
        header.object_status.map(|s| s as u64)
    );
    assert_eq!(decoded_payload, payload);
}

#[test]
fn payload_datagrams_match_the_oracle_for_every_combination_of_optional_fields() {
    let props = properties();
    let payload = [0x5au8; 80];
    for bits in 0..16u8 {
        assert_same(Datagram {
            track_alias: 41,
            group: 12_345,
            object: if bits & 1 != 0 { 700 } else { 0 },
            priority: (bits & 2 != 0).then_some(200),
            end_of_group: bits & 4 != 0,
            properties: if bits & 8 != 0 { &props } else { &[] },
            body: ObjectBody::Payload(&payload),
        });
    }
}

#[test]
fn status_datagrams_match_the_oracle() {
    let props = properties();
    for status in [
        ObjectStatus::Normal,
        ObjectStatus::EndOfGroup,
        ObjectStatus::EndOfTrack,
    ] {
        for object in [0, 5] {
            assert_same(Datagram {
                track_alias: 1,
                group: u64::MAX,
                object,
                priority: Some(0),
                end_of_group: false,
                properties: if status == ObjectStatus::Normal {
                    &props
                } else {
                    &[]
                },
                body: ObjectBody::Status(status),
            });
        }
    }
}

#[test]
fn an_empty_payload_matches_the_oracle() {
    assert_same(Datagram {
        track_alias: 1,
        group: 2,
        object: 0,
        priority: None,
        end_of_group: true,
        properties: &[],
        body: ObjectBody::Payload(&[]),
    });
}

// ---- subgroup streams ----

use moqtap_codec::draft22::data_stream::{
    SubgroupHeader as TapHeader, SubgroupObject as TapObject, SubgroupObjectReader as TapStream,
};
use palaver_moq_wire::{
    Reader, SubgroupHeader, SubgroupId, SubgroupObject, SubgroupReader, SubgroupWriter,
};

fn tap_subgroup_header(h: &SubgroupHeader) -> TapHeader {
    let (mode, subgroup_id) = match h.subgroup {
        SubgroupId::Zero => (0u8, 0),
        SubgroupId::FirstObject => (1, 0),
        SubgroupId::Explicit(id) => (2, id),
    };
    let mut flags = 0x10 | mode << 1;
    if h.has_properties {
        flags |= 0x01;
    }
    if h.end_of_group {
        flags |= 0x08;
    }
    if h.priority.is_none() {
        flags |= 0x20;
    }
    if h.first_object {
        flags |= 0x40;
    }
    TapHeader {
        header_type: flags,
        track_alias: VarInt::from_u64_moqt(h.track_alias),
        group_id: VarInt::from_u64_moqt(h.group),
        subgroup_id: VarInt::from_u64_moqt(subgroup_id),
        publisher_priority: h.priority,
    }
}

fn tap_object(o: &SubgroupObject<'_>) -> TapObject {
    let (payload, status): (&[u8], _) = match o.body {
        ObjectBody::Payload(p) if p.is_empty() => (p, Some(TapStatus::Normal)),
        ObjectBody::Payload(p) => (p, None),
        ObjectBody::Status(s) => (&[], Some(tap_status(s))),
    };
    TapObject {
        object_id: VarInt::from_u64_moqt(o.object),
        extension_headers: o.properties.to_vec(),
        payload_length: VarInt::from_u64_moqt(payload.len() as u64),
        object_status: status,
        payload: payload.to_vec(),
    }
}

#[test]
fn subgroup_headers_match_the_oracle_in_every_shape() {
    for bits in 0..16u8 {
        for subgroup in [
            SubgroupId::Zero,
            SubgroupId::FirstObject,
            SubgroupId::Explicit(700),
        ] {
            let ours_value = SubgroupHeader {
                track_alias: 41,
                group: 12_345,
                subgroup,
                priority: (bits & 1 != 0).then_some(200),
                has_properties: bits & 2 != 0,
                end_of_group: bits & 4 != 0,
                first_object: bits & 8 != 0,
            };
            let mut ours = vec![];
            ours_value.encode(&mut ours);
            let theirs_value = tap_subgroup_header(&ours_value);
            let mut theirs = BytesMut::new();
            theirs_value.encode_checked(&mut theirs).unwrap();
            assert_eq!(
                ours, theirs,
                "moqtap-codec encodes {ours_value:?} differently"
            );

            assert_eq!(
                SubgroupHeader::decode(&mut Reader::new(&theirs)),
                Ok(ours_value)
            );
            let decoded = TapHeader::decode(&mut &ours[..]).unwrap();
            assert_eq!(decoded.header_type, theirs_value.header_type);
            assert_eq!(decoded.track_alias, theirs_value.track_alias);
            assert_eq!(decoded.group_id, theirs_value.group_id);
            assert_eq!(decoded.publisher_priority, theirs_value.publisher_priority);
            if matches!(subgroup, SubgroupId::Explicit(_)) {
                assert_eq!(decoded.subgroup_id, theirs_value.subgroup_id);
            }
        }
    }
}

#[test]
fn subgroup_objects_match_the_oracle_with_and_without_properties() {
    let props = properties();
    for has_properties in [true, false] {
        let p: &[u8] = if has_properties { &props } else { &[] };
        let header = SubgroupHeader {
            track_alias: 41,
            group: 9,
            subgroup: SubgroupId::Zero,
            priority: None,
            has_properties,
            end_of_group: true,
            first_object: true,
        };
        let objects = [
            SubgroupObject {
                object: 0,
                properties: p,
                body: ObjectBody::Payload(b"key frame"),
            },
            SubgroupObject {
                object: 1,
                properties: &[],
                body: ObjectBody::Payload(&[0x5a; 300]),
            },
            SubgroupObject {
                object: 5,
                properties: &[],
                body: ObjectBody::Payload(b"after a gap"),
            },
            SubgroupObject {
                object: 6,
                properties: p,
                body: ObjectBody::Status(ObjectStatus::Normal),
            },
            SubgroupObject {
                object: 7,
                properties: &[],
                body: ObjectBody::Status(ObjectStatus::EndOfGroup),
            },
        ];

        let mut ours = vec![];
        let mut writer = SubgroupWriter::new(&header);
        for o in &objects {
            writer.write(o, &mut ours).unwrap();
        }
        let tap_header = tap_subgroup_header(&header);
        let mut theirs = BytesMut::new();
        let mut tap_writer = TapStream::new(&tap_header);
        for o in &objects {
            tap_writer
                .write_object(&tap_object(o), &mut theirs)
                .unwrap();
        }
        assert_eq!(
            ours, theirs,
            "moqtap-codec encodes the objects differently (properties: {has_properties})"
        );

        // We read theirs.
        let mut r = Reader::new(&theirs);
        let mut reader = SubgroupReader::new(&header);
        for o in &objects {
            assert_eq!(reader.read(&mut r), Ok(*o));
        }
        assert!(r.is_empty());
        // They read ours.
        let mut tap_reader = TapStream::new(&tap_header);
        let mut buf = &ours[..];
        for o in &objects {
            let got = tap_reader.read_object(&mut buf).unwrap();
            let want = tap_object(o);
            assert_eq!(got.object_id, want.object_id);
            assert_eq!(got.extension_headers, want.extension_headers);
            assert_eq!(got.payload, want.payload);
            assert_eq!(
                got.object_status.map(|s| s as u64),
                want.object_status.map(|s| s as u64)
            );
        }
        assert!(buf.is_empty());
    }
}
