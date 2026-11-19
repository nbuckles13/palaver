//! Two sessions joined back to back.
#![allow(clippy::unwrap_used, clippy::panic)]

use palaver_khef_core::session::{NOT_SUPPORTED, PREFIX_OVERLAP, UNINTERESTED};
use palaver_khef_core::{Action, CloseCode, Event, Role, Session, StreamId, StreamKind};
use palaver_moq_wire::{
    Frame, FullTrackName, GoAway, Message, Namespace, PalaverExtensions, Params, Publish, Reader,
    Setup, Subscribe, SubscribeTracks,
};

fn setup(version: Option<u64>) -> Setup {
    Setup {
        implementation: Some("palaver-test".to_owned()),
        palaver: version.map(|version| PalaverExtensions { version }),
        ..Setup::default()
    }
}

fn ns(fields: &[&str]) -> Namespace {
    Namespace::new(fields.iter().map(|f| f.as_bytes().to_vec()).collect()).unwrap()
}

fn track(fields: &[&str], name: &str) -> FullTrackName {
    FullTrackName::new(ns(fields), name.as_bytes().to_vec()).unwrap()
}

fn encoded(message: &Message) -> Vec<u8> {
    let mut out = vec![];
    message.encode(&mut out).unwrap();
    out
}

fn decoded(bytes: &[u8]) -> Message {
    let mut reader = Reader::new(bytes);
    Message::decode(Frame::decode(&mut reader).unwrap()).unwrap()
}

fn subscribe_tracks(request_id: u64, prefix: &[&str]) -> Vec<u8> {
    encoded(&Message::SubscribeTracks(SubscribeTracks {
        request_id,
        prefix: ns(prefix),
        params: Params::default(),
    }))
}

/// Something one end did to the connection, as the other end sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wire {
    Data {
        stream: StreamId,
        kind: StreamKind,
        bytes: Vec<u8>,
        fin: bool,
    },
    Reset {
        stream: StreamId,
    },
}

/// One end: a session, the kinds of the streams it opened, and what it has reported.
struct End {
    session: Session,
    kinds: Vec<StreamKind>,
    events: Vec<Event>,
    closed: Option<(CloseCode, &'static str)>,
}

impl End {
    fn new(role: Role, setup: Setup) -> Self {
        Self {
            session: Session::new(role, setup),
            kinds: vec![],
            events: vec![],
            closed: None,
        }
    }

    /// Collect what the session wants done, turned into what the other end will see.
    fn flush(&mut self) -> Vec<Wire> {
        // A stream we opened is theirs to the peer, and the other way round.
        let flip = |stream| match stream {
            StreamId::Ours(n) => StreamId::Theirs(n),
            StreamId::Theirs(n) => StreamId::Ours(n),
        };
        let mut out = vec![];
        while let Some(action) = self.session.poll_action() {
            match action {
                Action::Open { stream, kind } => {
                    assert_eq!(stream, StreamId::Ours(self.kinds.len() as u64));
                    self.kinds.push(kind);
                }
                Action::Send { stream, bytes, fin } => {
                    let kind = match stream {
                        StreamId::Ours(n) => self.kinds[n as usize],
                        StreamId::Theirs(_) => StreamKind::Bidi,
                    };
                    out.push(Wire::Data {
                        stream: flip(stream),
                        kind,
                        bytes,
                        fin,
                    });
                }
                Action::Reset { stream } => out.push(Wire::Reset {
                    stream: flip(stream),
                }),
                Action::Close { code, reason } => self.closed = Some((code, reason)),
            }
        }
        while let Some(event) = self.session.poll_event() {
            self.events.push(event);
        }
        out
    }

    fn receive(&mut self, wire: Wire) {
        match wire {
            Wire::Data {
                stream,
                kind,
                bytes,
                fin,
            } => self.session.on_data(stream, kind, &bytes, fin),
            Wire::Reset { stream } => self.session.on_reset(stream),
        }
    }

    /// Bytes arrive on a stream the peer opened.
    fn feed(&mut self, stream: u64, kind: StreamKind, bytes: &[u8]) {
        self.session
            .on_data(StreamId::Theirs(stream), kind, bytes, false);
    }

    fn take_events(&mut self) -> Vec<Event> {
        self.flush();
        core::mem::take(&mut self.events)
    }

    /// The code the session closed with.
    fn close_code(&mut self) -> Option<CloseCode> {
        self.flush();
        self.closed.map(|(code, _)| code)
    }
}

/// Carry everything between the two ends until both are quiet.
fn run(a: &mut End, b: &mut End) {
    loop {
        let (from_a, from_b) = (a.flush(), b.flush());
        if from_a.is_empty() && from_b.is_empty() {
            return;
        }
        for wire in from_a {
            b.receive(wire);
        }
        for wire in from_b {
            a.receive(wire);
        }
    }
}

/// A client and a server, both started and ready, with the `Ready` events taken.
fn pair() -> (End, End) {
    let mut a = End::new(Role::Client, setup(Some(1)));
    let mut b = End::new(Role::Server, setup(Some(1)));
    a.session.start();
    b.session.start();
    run(&mut a, &mut b);
    assert!(a.session.is_ready() && b.session.is_ready());
    a.take_events();
    b.take_events();
    (a, b)
}

/// `a` has asked `b` for the control tracks of meeting "m", and `b` has accepted.
fn subscribed() -> (End, End, StreamId) {
    let (mut a, mut b) = pair();
    let request = a.session.subscribe_tracks(ns(&["m", "ctl"])).unwrap();
    run(&mut a, &mut b);
    let wanted = b.take_events();
    let [Event::TracksWanted { request: at_b, .. }] = wanted[..] else {
        panic!("{wanted:?}")
    };
    b.session.accept_tracks(at_b);
    run(&mut a, &mut b);
    assert_eq!(a.take_events(), [Event::TracksAccepted { request }]);
    (a, b, request)
}

#[test]
fn both_ends_become_ready_and_agree_on_the_extension_version() {
    let mut a = End::new(Role::Client, setup(Some(3)));
    let mut b = End::new(Role::Server, setup(Some(2)));
    a.session.start();
    b.session.start();
    assert!(!a.session.is_ready());
    run(&mut a, &mut b);
    let ready = [Event::Ready {
        palaver_version: Some(2),
    }];
    assert_eq!(a.take_events(), ready);
    assert_eq!(b.take_events(), ready);
    assert_eq!(a.session.palaver_version(), Some(2));
    assert_eq!(a.kinds, [StreamKind::Uni], "one control stream each");
}

#[test]
fn a_peer_without_our_extensions_is_ready_with_no_version() {
    let mut a = End::new(Role::Client, setup(Some(1)));
    let mut b = End::new(Role::Server, setup(None));
    a.session.start();
    b.session.start();
    run(&mut a, &mut b);
    assert_eq!(
        a.take_events(),
        [Event::Ready {
            palaver_version: None
        }]
    );
}

#[test]
fn a_request_for_tracks_is_offered_to_the_khef_and_can_be_accepted() {
    let (mut a, mut b) = pair();
    let request = a.session.subscribe_tracks(ns(&["m", "ctl"])).unwrap();
    assert_eq!(request, StreamId::Ours(1));
    run(&mut a, &mut b);
    assert_eq!(
        b.take_events(),
        [Event::TracksWanted {
            request: StreamId::Theirs(1),
            prefix: ns(&["m", "ctl"])
        }]
    );
    assert_eq!(a.take_events(), [], "no answer until the khef decides");
    b.session.accept_tracks(StreamId::Theirs(1));
    run(&mut a, &mut b);
    assert_eq!(a.take_events(), [Event::TracksAccepted { request }]);
}

#[test]
fn a_request_for_tracks_can_be_refused() {
    let (mut a, mut b) = pair();
    let request = a.session.subscribe_tracks(ns(&["other", "ctl"])).unwrap();
    run(&mut a, &mut b);
    b.session
        .refuse(StreamId::Theirs(1), 0x1, "not your meeting");
    run(&mut a, &mut b);
    assert_eq!(
        a.take_events(),
        [Event::TracksRefused { request, code: 0x1 }]
    );
    // Answering a second time does nothing.
    b.session.accept_tracks(StreamId::Theirs(1));
    run(&mut a, &mut b);
    assert_eq!(a.take_events(), []);
    assert_eq!((a.closed, b.closed), (None, None));
}

#[test]
fn a_track_announced_under_a_wanted_prefix_is_taken_with_its_alias() {
    let (mut a, mut b, _) = subscribed();
    let plan = track(&["m", "ctl", "plan"], "0");
    let report = track(&["m", "ctl", "report", "p7"], "0");
    let first = b.session.publish(plan.clone()).unwrap();
    let second = b.session.publish(report.clone()).unwrap();
    assert_ne!(first, second);
    run(&mut a, &mut b);
    assert_eq!(
        a.take_events(),
        [
            Event::TrackOffered {
                track: plan,
                alias: first
            },
            Event::TrackOffered {
                track: report,
                alias: second
            },
        ]
    );
    assert_eq!(
        b.take_events(),
        [
            Event::TrackAccepted { alias: first },
            Event::TrackAccepted { alias: second },
        ]
    );
}

#[test]
fn both_ends_can_ask_and_announce_at_once() {
    let (mut a, mut b) = pair();
    a.session.subscribe_tracks(ns(&["m", "ctl"])).unwrap();
    b.session.subscribe_tracks(ns(&["m", "ctl"])).unwrap();
    run(&mut a, &mut b);
    a.session.accept_tracks(StreamId::Theirs(1));
    b.session.accept_tracks(StreamId::Theirs(1));
    let from_a = a
        .session
        .publish(track(&["m", "ctl", "plan"], "0"))
        .unwrap();
    let from_b = b
        .session
        .publish(track(&["m", "ctl", "report", "p2"], "0"))
        .unwrap();
    run(&mut a, &mut b);
    assert!(a.take_events().contains(&Event::TrackOffered {
        track: track(&["m", "ctl", "report", "p2"], "0"),
        alias: from_b
    }));
    assert!(b.take_events().contains(&Event::TrackOffered {
        track: track(&["m", "ctl", "plan"], "0"),
        alias: from_a
    }));
    assert_eq!((a.closed, b.closed), (None, None));
}

#[test]
fn a_track_nobody_asked_for_is_refused() {
    let (mut a, mut b, _) = subscribed();
    let alias = b
        .session
        .publish(track(&["m", "video", "main", "p7"], "360p"))
        .unwrap();
    run(&mut a, &mut b);
    assert_eq!(a.take_events(), []);
    assert_eq!(
        b.take_events(),
        [Event::TrackRefused {
            alias,
            code: UNINTERESTED
        }]
    );
}

#[test]
fn a_track_that_ends_is_reported_once() {
    let (mut a, mut b, _) = subscribed();
    let alias = b
        .session
        .publish(track(&["m", "ctl", "report", "p7"], "0"))
        .unwrap();
    run(&mut a, &mut b);
    a.take_events();
    b.session.end_track(alias);
    run(&mut a, &mut b);
    assert_eq!(a.take_events(), [Event::TrackEnded { alias }]);
    b.session.end_track(alias);
    run(&mut a, &mut b);
    assert_eq!(a.take_events(), []);
    assert_eq!((a.closed, b.closed), (None, None));
}

#[test]
fn overlapping_prefixes_are_not_asked_for_and_are_refused_if_asked() {
    let (mut a, mut b, _) = subscribed();
    // Our own side will not send one.
    assert_eq!(a.session.subscribe_tracks(ns(&["m"])), None);
    assert_eq!(
        a.session.subscribe_tracks(ns(&["m", "ctl", "report"])),
        None
    );
    assert!(a.session.subscribe_tracks(ns(&["m", "audio"])).is_some());
    run(&mut a, &mut b);
    b.take_events();
    // A peer that does is refused. Request id 4 is the client's third.
    b.feed(
        50,
        StreamKind::Bidi,
        &subscribe_tracks(4, &["m", "ctl", "report"]),
    );
    let out = b.flush();
    let [
        Wire::Data {
            bytes, fin: true, ..
        },
    ] = &out[..]
    else {
        panic!("{out:?}")
    };
    let Message::RequestError(error) = decoded(bytes) else {
        panic!()
    };
    assert_eq!(error.code, PREFIX_OVERLAP);
    assert_eq!(b.take_events(), []);
    assert_eq!(b.closed, None);
}

#[test]
fn giving_up_a_request_for_tracks_tells_the_peer() {
    let (mut a, mut b, request) = subscribed();
    a.session.unsubscribe_tracks(request);
    run(&mut a, &mut b);
    assert_eq!(
        b.take_events(),
        [Event::TracksUnwanted {
            request: StreamId::Theirs(1)
        }]
    );
    // The prefix can be asked for again.
    assert!(a.session.subscribe_tracks(ns(&["m", "ctl"])).is_some());
}

#[test]
fn a_reset_ends_whatever_the_stream_carried() {
    let (mut a, mut b, request) = subscribed();
    let alias = b
        .session
        .publish(track(&["m", "ctl", "plan"], "0"))
        .unwrap();
    run(&mut a, &mut b);
    a.take_events();
    b.take_events();
    // At the subscriber: the stream of the track it receives, and of its own request.
    a.session.on_reset(StreamId::Theirs(1));
    a.session.on_reset(request);
    assert_eq!(
        a.take_events(),
        [
            Event::TrackEnded { alias },
            Event::TracksRefused { request, code: 0 }
        ]
    );
    // At the publisher: the stream of the track it announced.
    b.session.on_reset(StreamId::Ours(1));
    assert_eq!(b.take_events(), [Event::TrackRefused { alias, code: 0 }]);
}

#[test]
fn requests_that_arrive_before_the_peers_setup_wait_for_it() {
    let mut a = End::new(Role::Client, setup(Some(1)));
    let mut b = End::new(Role::Server, setup(Some(1)));
    a.session.start();
    b.session.start();
    a.session.subscribe_tracks(ns(&["m", "ctl"])).unwrap();
    let from_a = a.flush();
    assert_eq!(from_a.len(), 2);
    // The request stream overtakes the control stream.
    b.receive(from_a[1].clone());
    assert_eq!(b.take_events(), []);
    b.receive(from_a[0].clone());
    assert_eq!(
        b.take_events(),
        [
            Event::Ready {
                palaver_version: Some(1)
            },
            Event::TracksWanted {
                request: StreamId::Theirs(1),
                prefix: ns(&["m", "ctl"])
            }
        ]
    );
}

#[test]
fn bytes_may_arrive_one_at_a_time() {
    let mut a = End::new(Role::Client, setup(Some(1)));
    let mut b = End::new(Role::Server, setup(Some(1)));
    a.session.start();
    b.session.start();
    a.session.subscribe_tracks(ns(&["m", "ctl"])).unwrap();
    for wire in a.flush() {
        let Wire::Data {
            stream,
            kind,
            bytes,
            ..
        } = wire
        else {
            panic!()
        };
        for byte in bytes {
            b.session.on_data(stream, kind, &[byte], false);
        }
    }
    assert_eq!(b.take_events().len(), 2, "ready, and the request");
    assert_eq!(b.closed, None);
}

#[test]
fn several_messages_may_arrive_in_one_read() {
    let (_, mut b) = pair();
    let goaway = encoded(&Message::GoAway(GoAway {
        new_session_uri: vec![],
        timeout: core::time::Duration::ZERO,
    }));
    b.feed(0, StreamKind::Uni, &[goaway.clone(), goaway].concat());
    assert_eq!(b.close_code(), None);
}

// --- What closes the session ---

#[test]
fn a_request_id_of_the_wrong_parity_or_used_twice_closes_the_session() {
    // The peer is the client: its ids are even.
    let (_, mut b) = pair();
    b.feed(9, StreamKind::Bidi, &subscribe_tracks(3, &["m", "ctl"]));
    assert_eq!(b.close_code(), Some(CloseCode::InvalidRequestId));

    let (_, mut b) = pair();
    b.feed(9, StreamKind::Bidi, &subscribe_tracks(0, &["m", "ctl"]));
    assert_eq!(b.close_code(), None);
    b.feed(10, StreamKind::Bidi, &subscribe_tracks(0, &["m", "audio"]));
    assert_eq!(b.close_code(), Some(CloseCode::InvalidRequestId));
}

#[test]
fn an_alias_used_for_two_tracks_closes_the_session() {
    let (mut a, _, _) = subscribed();
    for (stream, id, name) in [(20, 1, "p1"), (21, 3, "p2")] {
        let publish = Message::Publish(Publish {
            request_id: id,
            track: track(&["m", "ctl", "report", name], "0"),
            track_alias: 77,
            params: Params::default(),
            track_properties: vec![],
        });
        a.feed(stream, StreamKind::Bidi, &encoded(&publish));
    }
    assert_eq!(a.close_code(), Some(CloseCode::DuplicateTrackAlias));
}

#[test]
fn things_the_draft_forbids_close_the_session() {
    let again = encoded(&Message::Setup(setup(Some(1))));
    let violation = Some(CloseCode::ProtocolViolation);

    // A second SETUP.
    let (_, mut b) = pair();
    b.feed(0, StreamKind::Uni, &again);
    assert_eq!(b.close_code(), violation);

    // A second control stream.
    let (_, mut b) = pair();
    b.feed(40, StreamKind::Uni, &again);
    assert_eq!(b.close_code(), violation);

    // The control stream ends, or is reset.
    let (_, mut b) = pair();
    b.session
        .on_data(StreamId::Theirs(0), StreamKind::Uni, &[], true);
    assert_eq!(b.close_code(), violation);
    let (_, mut b) = pair();
    b.session.on_reset(StreamId::Theirs(0));
    assert_eq!(b.close_code(), violation);

    // A request on the control stream.
    let (_, mut b) = pair();
    b.feed(0, StreamKind::Uni, &subscribe_tracks(0, &["m", "ctl"]));
    assert_eq!(b.close_code(), violation);

    // A request stream that does not begin with a request.
    let (_, mut b) = pair();
    b.feed(9, StreamKind::Bidi, &again);
    assert_eq!(b.close_code(), violation);

    // A stream that ends inside a message.
    let (_, mut b) = pair();
    b.session
        .on_data(StreamId::Theirs(9), StreamKind::Bidi, &again[..3], true);
    assert_eq!(b.close_code(), violation);

    // A unidirectional stream of an unknown type.
    let (_, mut b) = pair();
    b.feed(9, StreamKind::Uni, &[0x05]);
    assert_eq!(b.close_code(), violation);

    // A message type that does not exist.
    let (_, mut b) = pair();
    b.feed(9, StreamKind::Bidi, &[0x3f, 0, 0]);
    assert_eq!(b.close_code(), violation);

    // A response that is not one.
    let (mut a, _) = pair();
    let request = a.session.subscribe_tracks(ns(&["m", "ctl"])).unwrap();
    a.session.on_data(request, StreamKind::Bidi, &again, false);
    assert_eq!(a.close_code(), violation);

    // A message longer than any message can be.
    let (_, mut b) = pair();
    for _ in 0..70 {
        b.feed(9, StreamKind::Bidi, &[0x07, 0xff, 0xff, 0, 0, 0, 0, 0]);
        b.feed(9, StreamKind::Bidi, &[0; 1000]);
    }
    assert_eq!(b.close_code(), violation);
}

#[test]
fn a_closed_session_does_nothing_more() {
    let (_, mut b) = pair();
    b.session.on_reset(StreamId::Theirs(0));
    assert_eq!(b.close_code(), Some(CloseCode::ProtocolViolation));
    assert!(b.session.is_closed() && !b.session.is_ready());
    assert_eq!(b.session.subscribe_tracks(ns(&["m", "ctl"])), None);
    assert_eq!(b.session.publish(track(&["m", "ctl", "plan"], "0")), None);
    b.feed(9, StreamKind::Bidi, &[1, 2, 3]);
    assert_eq!(b.flush(), []);
}

#[test]
fn a_single_track_subscribe_is_answered_not_supported() {
    let (_, mut b) = pair();
    let subscribe = Message::Subscribe(Subscribe {
        request_id: 0,
        track: track(&["m", "ctl", "plan"], "0"),
        params: Params::default(),
    });
    b.feed(9, StreamKind::Bidi, &encoded(&subscribe));
    let out = b.flush();
    let [
        Wire::Data {
            bytes, fin: true, ..
        },
    ] = &out[..]
    else {
        panic!("{out:?}")
    };
    let Message::RequestError(error) = decoded(bytes) else {
        panic!()
    };
    assert_eq!(error.code, NOT_SUPPORTED);
    assert_eq!(b.closed, None);
}

#[test]
fn object_and_padding_streams_are_passed_over_for_now() {
    let (_, mut b) = pair();
    // A subgroup header type, then whatever follows; and a padding stream.
    b.feed(9, StreamKind::Uni, &[0x10, 1, 2, 3, 4, 5]);
    b.session
        .on_data(StreamId::Theirs(9), StreamKind::Uni, &[9; 500], true);
    let mut padding = vec![];
    palaver_moq_wire::put_vi64(&mut padding, palaver_moq_wire::PADDING_STREAM);
    padding.extend([0; 100]);
    b.feed(10, StreamKind::Uni, &padding);
    assert_eq!(b.flush(), []);
    assert_eq!(b.closed, None);
}

#[test]
fn arbitrary_bytes_never_panic_and_a_close_is_always_announced() {
    // A small generator, so the test needs nothing but the standard library.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut closed = 0;
    for round in 0..300 {
        let (_, mut b, _) = subscribed();
        for _ in 0..20 {
            let stream = match next() % 3 {
                0 => StreamId::Theirs(next() % 4),
                1 => StreamId::Theirs(100 + next() % 4),
                _ => StreamId::Ours(next() % 3),
            };
            let kind = if next() % 2 == 0 {
                StreamKind::Uni
            } else {
                StreamKind::Bidi
            };
            let len = (next() % 40) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            b.session.on_data(stream, kind, &bytes, next() % 8 == 0);
            if next() % 16 == 0 {
                b.session.on_reset(stream);
            }
            b.flush();
            assert_eq!(
                b.closed.is_some(),
                b.session.is_closed(),
                "round {round}: closed without saying so"
            );
        }
        closed += usize::from(b.session.is_closed());
    }
    assert!(
        closed > 250,
        "only {closed} of 300 sessions closed on garbage"
    );
}
