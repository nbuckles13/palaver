//! Cross-checks against two independent MoQ codecs: MOQtail and moqtap-codec (draft 22).
//!
//! For each primitive: our bytes equal theirs, and each side decodes the other's bytes.

use bytes::{Bytes, BytesMut};
use moqtail::model::common::pair::KeyValuePair as TailKvp;
use moqtail::model::common::tuple::Tuple;
use moqtail::model::common::varint::{BufMutVarIntExt, BufVarIntExt};
use moqtap_codec::types::TrackNamespace;
use moqtap_codec::varint::{Moqt18, VarInt};
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
fn integers_match_both_oracles() {
    let values = interesting_integers();
    assert!(values.len() > 2000);
    for v in values {
        let mut ours = vec![];
        put_vi64(&mut ours, v);

        let mut tail = BytesMut::new();
        tail.put_vi(v).unwrap();
        assert_eq!(ours, tail, "moqtail encodes {v} differently");

        let mut tap = BytesMut::new();
        VarInt::from_u64_moqt(v).encode_moqt::<Moqt18>(&mut tap);
        assert_eq!(ours, tap, "moqtap-codec encodes {v} differently");

        assert_eq!(Reader::new(&tail).vi64(), Ok(v));
        assert_eq!(Bytes::from(ours.clone()).get_vi().unwrap(), v);
        assert_eq!(
            VarInt::decode_moqt::<Moqt18>(&mut &ours[..])
                .unwrap()
                .into_inner(),
            v
        );
    }
}

#[test]
fn namespaces_match_both_oracles() {
    let cases: &[&[&str]] = &[
        &["m"],
        &["m-7f3a", "audio", "main"],
        &["m-7f3a", "video", "main", "p12"],
        &["a"; 32],
    ];
    for fields in cases {
        let ours_value =
            Namespace::new(fields.iter().map(|f| f.as_bytes().to_vec()).collect()).unwrap();
        let mut ours = vec![];
        ours_value.encode(&mut ours);

        let tail = Tuple::from_utf8_path(&fields.join("/"))
            .serialize()
            .unwrap();
        assert_eq!(ours, tail, "moqtail encodes {fields:?} differently");

        let mut tap = BytesMut::new();
        TrackNamespace(fields.iter().map(|f| f.as_bytes().to_vec()).collect()).encode(&mut tap);
        assert_eq!(ours, tap, "moqtap-codec encodes {fields:?} differently");

        assert_eq!(Namespace::decode(&mut Reader::new(&tail)), Ok(ours_value));
    }
}

#[test]
fn key_value_pairs_match_moqtail() {
    let blob = vec![0xabu8; 300];
    let pairs = [
        Kvp {
            ty: 2,
            value: Value::Int(7),
        },
        Kvp {
            ty: 5,
            value: Value::Bytes(b"ab"),
        },
        Kvp {
            ty: 5,
            value: Value::Bytes(b""),
        },
        Kvp {
            ty: 0x3f02,
            value: Value::Int(87),
        },
        Kvp {
            ty: 0x3f03,
            value: Value::Bytes(&blob),
        },
        Kvp {
            ty: u64::MAX - 1,
            value: Value::Int(u64::MAX),
        },
    ];
    let mut ours = vec![];
    encode_kvps(&pairs, &mut ours).unwrap();

    let mut tail = BytesMut::new();
    let mut prev = 0;
    for p in &pairs {
        let kvp = match p.value {
            Value::Int(v) => TailKvp::try_new_varint(p.ty, v).unwrap(),
            Value::Bytes(b) => TailKvp::try_new_bytes(p.ty, Bytes::copy_from_slice(b)).unwrap(),
        };
        tail.extend_from_slice(&kvp.serialize_delta(prev).unwrap());
        prev = p.ty;
    }
    assert_eq!(ours, tail);

    // and each side reads the other's bytes
    assert_eq!(decode_kvps(&mut Reader::new(&tail)).unwrap(), pairs);
    let mut theirs = Bytes::from(ours);
    let mut prev = 0;
    for p in &pairs {
        let got = TailKvp::deserialize_delta(&mut theirs, prev).unwrap();
        assert_eq!(got.get_type(), p.ty);
        prev = p.ty;
    }
    assert!(theirs.is_empty());
}
