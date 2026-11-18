//! Cross-checks of the request messages against moqtap-codec (draft 22): identical bytes, and
//! each side decodes the other's output.

// Helpers in an integration test file are test code, but clippy only exempts `#[test]` functions.
#![allow(clippy::unwrap_used)]

use bytes::BytesMut;
use moqtap_codec::draft22::message::{
    ControlMessage, Redirect as TapRedirect, RequestError as TapRequestError,
    RequestOk as TapRequestOk, Subscribe as TapSubscribe, SubscribeOk as TapSubscribeOk,
};
use moqtap_codec::kvp::{KeyValuePair as TapKvp, KvpValue};
use moqtap_codec::types::TrackNamespace;
use moqtap_codec::varint::VarInt;
use palaver_moq_wire::{
    ERROR_REDIRECT, Frame, FullTrackName, GroupOrder, Kvp, Location, LocationFilter, Message,
    Namespace, Params, Reader, Redirect, RequestError, RequestOk, Subscribe, SubscribeOk, Value,
    encode_kvps, put_vi64,
};

fn tap_int(ty: u64, v: u64) -> TapKvp {
    TapKvp {
        key: VarInt::from_u64_moqt(ty),
        value: KvpValue::Varint(VarInt::from_u64_moqt(v)),
    }
}

/// The oracle holds a Location or Location Filter parameter as its raw encoding.
fn tap_raw(ty: u64, integers: &[u64]) -> TapKvp {
    let mut raw = vec![];
    for v in integers {
        put_vi64(&mut raw, *v);
    }
    TapKvp {
        key: VarInt::from_u64_moqt(ty),
        value: KvpValue::Bytes(raw),
    }
}

const FIELDS: [&str; 4] = ["m-7f3a", "video", "main", "p12"];

fn our_track() -> FullTrackName {
    let ns = Namespace::new(FIELDS.iter().map(|f| f.as_bytes().to_vec()).collect()).unwrap();
    FullTrackName::new(ns, b"360p".to_vec()).unwrap()
}

fn tap_namespace() -> TrackNamespace {
    TrackNamespace(FIELDS.iter().map(|f| f.as_bytes().to_vec()).collect())
}

/// Identical bytes, and each side decodes the other's output to the expected value.
fn assert_same(ours: Message, theirs: ControlMessage) {
    let mut our_bytes = vec![];
    ours.encode(&mut our_bytes).unwrap();
    let mut their_bytes = BytesMut::new();
    theirs.encode(&mut their_bytes).unwrap();
    assert_eq!(
        our_bytes, their_bytes,
        "moqtap-codec encodes {ours:?} differently"
    );
    assert_eq!(
        Message::decode(Frame::decode(&mut Reader::new(&their_bytes)).unwrap()).unwrap(),
        ours
    );
    assert_eq!(ControlMessage::decode(&mut &our_bytes[..]).unwrap(), theirs);
}

#[test]
fn subscribe_matches_the_oracle_with_every_parameter_it_allows() {
    let ours = Message::Subscribe(Subscribe {
        request_id: 4,
        track: our_track(),
        params: Params {
            object_delivery_timeout: Some(500),
            subgroup_delivery_timeout: Some(2000),
            forward: Some(false),
            subscriber_priority: Some(200),
            location_filter: Some(LocationFilter::AbsoluteRange {
                start: Location {
                    group: 5,
                    object: 1,
                },
                end_group_delta: 2,
                end_object: 7,
            }),
            group_order: Some(GroupOrder::Descending),
            new_group_request: Some(10),
            include_properties: Some(true),
            ..Params::default()
        },
    });
    let theirs = ControlMessage::Subscribe(TapSubscribe {
        request_id: VarInt::from_u64_moqt(4),
        track_namespace: tap_namespace(),
        track_name: b"360p".to_vec(),
        parameters: vec![
            tap_int(0x02, 500),
            tap_int(0x06, 2000),
            tap_int(0x10, 0),
            tap_int(0x20, 200),
            tap_raw(0x21, &[4, 5, 1, 2, 7]),
            tap_int(0x22, 2),
            tap_int(0x32, 10),
            tap_int(0x35, 1),
        ],
    });
    assert_same(ours, theirs);
}

#[test]
fn subscribe_matches_the_oracle_for_each_location_filter_form() {
    let start = Location {
        group: 300,
        object: 2,
    };
    let forms: [(LocationFilter, &[u64]); 6] = [
        (LocationFilter::None, &[0]),
        (LocationFilter::RelativeStart { groups_back: 1 }, &[1, 1]),
        (LocationFilter::AbsoluteStart(start), &[2, 300, 2]),
        (
            LocationFilter::AbsoluteStartGroupEnd {
                start,
                end_group_delta: 3,
            },
            &[3, 300, 2, 3],
        ),
        (
            LocationFilter::AbsoluteRange {
                start,
                end_group_delta: 0,
                end_object: 9,
            },
            &[4, 300, 2, 0, 9],
        ),
        (LocationFilter::NextObject, &[5]),
    ];
    for (filter, raw) in forms {
        let ours = Message::Subscribe(Subscribe {
            request_id: 0,
            track: our_track(),
            params: Params {
                location_filter: Some(filter),
                ..Params::default()
            },
        });
        let theirs = ControlMessage::Subscribe(TapSubscribe {
            request_id: VarInt::from_u64_moqt(0),
            track_namespace: tap_namespace(),
            track_name: b"360p".to_vec(),
            parameters: vec![tap_raw(0x21, raw)],
        });
        assert_same(ours, theirs);
    }
}

#[test]
fn subscribe_ok_and_request_ok_match_the_oracle() {
    let mut properties = vec![];
    let pairs = [
        Kvp {
            ty: 0x3802,
            value: Value::Int(7),
        },
        Kvp {
            ty: 0x3803,
            value: Value::Bytes(b"x"),
        },
    ];
    encode_kvps(&pairs, &mut properties).unwrap();
    let tap_properties = vec![
        tap_int(0x3802, 7),
        TapKvp {
            key: VarInt::from_u64_moqt(0x3803),
            value: KvpValue::Bytes(b"x".to_vec()),
        },
    ];
    let params = Params {
        expires: Some(60_000),
        largest_object: Some(Location {
            group: 9,
            object: 2,
        }),
        ..Params::default()
    };
    let tap_params = vec![tap_int(0x08, 60_000), tap_raw(0x09, &[9, 2])];

    assert_same(
        Message::SubscribeOk(SubscribeOk {
            track_alias: 41,
            params: params.clone(),
            track_properties: properties.clone(),
        }),
        ControlMessage::SubscribeOk(TapSubscribeOk {
            track_alias: VarInt::from_u64_moqt(41),
            parameters: tap_params.clone(),
            track_properties: tap_properties.clone(),
        }),
    );
    assert_same(
        Message::RequestOk(RequestOk {
            params,
            track_properties: properties,
        }),
        ControlMessage::RequestOk(TapRequestOk {
            parameters: tap_params,
            track_properties: tap_properties,
        }),
    );
    assert_same(
        Message::RequestOk(RequestOk {
            params: Params::default(),
            track_properties: vec![],
        }),
        ControlMessage::RequestOk(TapRequestOk {
            parameters: vec![],
            track_properties: vec![],
        }),
    );
}

#[test]
fn request_error_matches_the_oracle_with_and_without_a_redirect() {
    assert_same(
        Message::RequestError(RequestError {
            code: 0x10,
            retry_interval: 5001,
            reason: String::from("no such track"),
            redirect: None,
        }),
        ControlMessage::RequestError(TapRequestError {
            error_code: VarInt::from_u64_moqt(0x10),
            retry_interval: VarInt::from_u64_moqt(5001),
            reason_phrase: b"no such track".to_vec(),
            redirect: None,
        }),
    );
    assert_same(
        Message::RequestError(RequestError {
            code: ERROR_REDIRECT,
            retry_interval: 1,
            reason: String::new(),
            redirect: Some(Redirect {
                connect_uri: vec![],
                target: our_track(),
            }),
        }),
        ControlMessage::RequestError(TapRequestError {
            error_code: VarInt::from_u64_moqt(ERROR_REDIRECT),
            retry_interval: VarInt::from_u64_moqt(1),
            reason_phrase: vec![],
            redirect: Some(TapRedirect {
                connect_uri: vec![],
                track_namespace: tap_namespace(),
                track_name: b"360p".to_vec(),
            }),
        }),
    );
}
