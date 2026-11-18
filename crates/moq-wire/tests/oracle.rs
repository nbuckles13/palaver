//! Cross-checks against an independent MoQ codec, moqtap-codec (draft 22).
//!
//! For each primitive: our bytes equal theirs, and each side decodes the other's bytes.

use bytes::BytesMut;
use moqtap_codec::draft22::message::{ControlMessage, Setup};
use moqtap_codec::kvp::{KeyValuePair as TapKvp, KvpValue};
use moqtap_codec::object_properties::{PropertyValue, parse_object_properties};
use moqtap_codec::types::TrackNamespace;
use moqtap_codec::varint::{Moqt18, VarInt};
use moqtap_codec::version::DraftVersion;
use palaver_moq_wire::{Kvp, Namespace, Reader, Value, decode_kvps, encode_kvps, put_vi64};

/// Boundary values of every encoded length, plus a deterministic pseudo-random spread.
fn interesting_integers() -> Vec<u64> {
    let mut v = vec![0, 1, 37, u64::MAX];
    for bits in 1..64 {
        v.extend([(1u64 << bits) - 1, 1u64 << bits, (1u64 << bits) + 1]);
    }
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for i in 0..2000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push(x >> (i % 64));
    }
    v
}

#[test]
fn integers_match_the_oracle() {
    let values = interesting_integers();
    assert!(values.len() > 2000);
    for v in values {
        let mut ours = vec![];
        put_vi64(&mut ours, v);

        let mut theirs = BytesMut::new();
        VarInt::from_u64_moqt(v).encode_moqt::<Moqt18>(&mut theirs);
        assert_eq!(ours, theirs, "moqtap-codec encodes {v} differently");

        assert_eq!(Reader::new(&theirs).vi64(), Ok(v));
        assert_eq!(
            VarInt::decode_moqt::<Moqt18>(&mut &ours[..])
                .unwrap()
                .into_inner(),
            v
        );
    }
}

#[test]
fn namespaces_match_the_oracle() {
    let cases: &[&[&str]] = &[
        &["m"],
        &["m-7f3a", "audio", "main"],
        &["m-7f3a", "video", "main", "p12"],
        &["a"; 32],
    ];
    for fields in cases {
        let raw: Vec<Vec<u8>> = fields.iter().map(|f| f.as_bytes().to_vec()).collect();
        let ours_value = Namespace::new(raw.clone()).unwrap();
        let mut ours = vec![];
        ours_value.encode(&mut ours);

        let mut theirs = BytesMut::new();
        TrackNamespace(raw.clone()).encode(&mut theirs);
        assert_eq!(ours, theirs, "moqtap-codec encodes {fields:?} differently");

        assert_eq!(Namespace::decode(&mut Reader::new(&theirs)), Ok(ours_value));
        assert_eq!(TrackNamespace::decode(&mut &ours[..]).unwrap().0, raw);
    }
}

/// Pairs with types the oracle treats as unknown, so it carries them without interpreting.
/// No type repeats: the oracle refuses to write a repeated setup option.
fn sample_pairs(blob: &[u8]) -> Vec<Kvp<'_>> {
    vec![
        Kvp {
            ty: 0x40,
            value: Value::Int(7),
        },
        Kvp {
            ty: 0x41,
            value: Value::Bytes(b"ab"),
        },
        Kvp {
            ty: 0x43,
            value: Value::Bytes(b""),
        },
        Kvp {
            ty: 0x3f02,
            value: Value::Int(87),
        },
        Kvp {
            ty: 0x3f03,
            value: Value::Bytes(blob),
        },
        Kvp {
            ty: u64::MAX - 1,
            value: Value::Int(u64::MAX),
        },
    ]
}

fn to_oracle(pair: &Kvp<'_>) -> TapKvp {
    let value = match pair.value {
        Value::Int(v) => KvpValue::Varint(VarInt::from_u64_moqt(v)),
        Value::Bytes(b) => KvpValue::Bytes(b.to_vec()),
    };
    TapKvp {
        key: VarInt::from_u64_moqt(pair.ty),
        value,
    }
}

/// The oracle exposes draft 22's delta-coded key-value pairs through the SETUP message, whose
/// payload is exactly a run of them. Compare that payload with ours, in both directions.
#[test]
fn key_value_pairs_match_the_oracle_via_setup_options() {
    let blob = vec![0xabu8; 300];
    let pairs = sample_pairs(&blob);
    let mut ours = vec![];
    encode_kvps(&pairs, &mut ours).unwrap();

    let mut message = BytesMut::new();
    ControlMessage::Setup(Setup {
        options: pairs.iter().map(to_oracle).collect(),
    })
    .encode(&mut message)
    .unwrap();

    // The message is: type, 16-bit payload length, payload.
    let mut r = Reader::new(&message);
    r.vi64().unwrap();
    let len = u16::from_be_bytes(r.take(2).unwrap().try_into().unwrap());
    let payload = r.rest();
    assert_eq!(usize::from(len), payload.len());
    assert_eq!(ours, payload, "moqtap-codec encodes the pairs differently");

    // We read their bytes.
    assert_eq!(decode_kvps(&mut Reader::new(payload)).unwrap(), pairs);

    // They read ours: the same header followed by our bytes.
    let header_len = message.len() - payload.len();
    let mut rebuilt = message[..header_len].to_vec();
    rebuilt.extend_from_slice(&ours);
    let ControlMessage::Setup(decoded) = ControlMessage::decode(&mut &rebuilt[..]).unwrap() else {
        panic!("not a SETUP message");
    };
    assert_eq!(
        decoded.options,
        pairs.iter().map(to_oracle).collect::<Vec<_>>()
    );
}

/// The same encoding carries object properties; the oracle has a separate parser for those.
#[test]
fn the_oracle_parses_our_pairs_as_object_properties() {
    let blob = vec![0xabu8; 300];
    let mut pairs = sample_pairs(&blob);
    // Properties may repeat a type, which is written as a delta of zero.
    pairs.insert(
        2,
        Kvp {
            ty: 0x41,
            value: Value::Bytes(b"again"),
        },
    );
    let mut ours = vec![];
    encode_kvps(&pairs, &mut ours).unwrap();

    let parsed = parse_object_properties(DraftVersion::Draft22, &ours).unwrap();
    assert_eq!(parsed.len(), pairs.len());
    for (theirs, pair) in parsed.iter().zip(&pairs) {
        assert_eq!(theirs.property_type, pair.ty);
        match (&theirs.value, pair.value) {
            (PropertyValue::Varint(a), Value::Int(b)) => assert_eq!(*a, b),
            (PropertyValue::Bytes(a), Value::Bytes(b)) => assert_eq!(*a, b),
            (other, _) => panic!("type {} parsed as {other:?}", pair.ty),
        }
    }
}
