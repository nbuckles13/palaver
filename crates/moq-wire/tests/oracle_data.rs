//! Cross-checks of object datagrams against moqtap-codec (draft 22): identical bytes, and each
//! side decodes the other's output.

// Helpers in an integration test file are test code, but clippy only exempts `#[test]` functions.
#![allow(clippy::unwrap_used)]

use bytes::BytesMut;
use moqtap_codec::draft22::data_stream::DatagramHeader;
use moqtap_codec::draft22::types::ObjectStatus as TapStatus;
use moqtap_codec::varint::VarInt;
use palaver_moq_wire::{
    Datagram, DatagramBody, Kvp, ObjectStatus, PROPERTY_AUDIO_LEVEL, Value, encode_kvps,
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
        DatagramBody::Status(s) => {
            flags |= 0x20;
            Some(tap_status(s))
        }
        DatagramBody::Payload(_) => None,
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
        DatagramBody::Payload(p) => p,
        DatagramBody::Status(_) => &[],
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
            body: DatagramBody::Payload(&payload),
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
                body: DatagramBody::Status(status),
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
        body: DatagramBody::Payload(&[]),
    });
}
