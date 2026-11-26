//! Two sessions joined back to back.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test may stop where it stands"
)]

use palaver_khef_core::session::{NOT_SUPPORTED, PREFIX_OVERLAP, UNINTERESTED};
use palaver_khef_core::{
    Action, CloseCode, Event, GroupReader, GroupWriter, Role, Session, StreamId, StreamKind,
};
use palaver_moq_wire::{
    Frame, FullTrackName, GoAway, Message, Namespace, PalaverExtensions, Params, Publish, Reader,
    Setup, SubscribeOk, SubscribeTracks,
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

/// Stands for the alias every khef derives from a track's name: the same for the same name.
fn alias_of(track: &FullTrackName) -> u64 {
    let mut name = vec![];
    track.encode(&mut name);
    name.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    }) >> 2
}

/// Announce `track` from `end` under its alias, which is returned.
fn publish(end: &mut End, track: FullTrackName) -> u64 {
    let alias = alias_of(&track);
    assert!(end.session.publish(track, alias));
    alias
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
    const fn new(role: Role, setup: Setup) -> Self {
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
                        StreamId::Ours(n) => self.kinds[usize::try_from(n).unwrap()],
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
    let first = publish(&mut b, plan.clone());
    let second = publish(&mut b, report.clone());
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
    let from_a = publish(&mut a, track(&["m", "ctl", "plan"], "0"));
    let from_b = publish(&mut b, track(&["m", "ctl", "report", "p2"], "0"));
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
fn the_khef_chooses_the_alias_and_it_is_what_the_peer_is_told() {
    let (mut a, mut b, _) = subscribed();
    let plan = track(&["m", "ctl", "plan"], "0");
    assert!(b.session.publish(plan.clone(), 0x1234_5678_9abc));
    run(&mut a, &mut b);
    assert_eq!(
        a.take_events(),
        [Event::TrackOffered {
            track: plan,
            alias: 0x1234_5678_9abc
        }]
    );
    // The same alias cannot stand for a second track on this link.
    let report = track(&["m", "ctl", "report", "p7"], "0");
    assert!(!b.session.publish(report.clone(), 0x1234_5678_9abc));
    assert_eq!(b.flush(), []);
    // Once the first track has ended it can.
    b.session.end_track(0x1234_5678_9abc);
    assert!(b.session.publish(report, 0x1234_5678_9abc));
    run(&mut a, &mut b);
    assert_eq!((a.closed, b.closed), (None, None));
}

#[test]
fn both_ends_may_use_the_same_alias_for_the_same_track() {
    // Each end announces the plan track to the other under the alias they both derive.
    let (mut a, mut b) = pair();
    a.session.subscribe_tracks(ns(&["m", "ctl"])).unwrap();
    b.session.subscribe_tracks(ns(&["m", "ctl"])).unwrap();
    run(&mut a, &mut b);
    a.session.accept_tracks(StreamId::Theirs(1));
    b.session.accept_tracks(StreamId::Theirs(1));
    let plan = track(&["m", "ctl", "plan"], "0");
    let alias = publish(&mut a, plan.clone());
    assert_eq!(publish(&mut b, plan), alias);
    run(&mut a, &mut b);
    assert_eq!((a.closed, b.closed), (None, None));
    a.take_events();
    b.take_events();
    // Objects flow both ways under it.
    assert!(a.session.send_object(alias, 1, b"from a"));
    assert!(b.session.send_object(alias, 2, b"from b"));
    run(&mut a, &mut b);
    assert_eq!(b.take_events(), [object(alias, 1, b"from a")]);
    assert_eq!(a.take_events(), [object(alias, 2, b"from b")]);
}

#[test]
fn a_track_nobody_asked_for_is_refused() {
    let (mut a, mut b, _) = subscribed();
    let alias = publish(&mut b, track(&["m", "video", "main", "p7"], "360p"));
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
    let alias = publish(&mut b, track(&["m", "ctl", "report", "p7"], "0"));
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
    let alias = publish(&mut b, track(&["m", "ctl", "plan"], "0"));
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
    assert!(!b.session.publish(track(&["m", "ctl", "plan"], "0"), 1));
    b.feed(9, StreamKind::Bidi, &[1, 2, 3]);
    assert_eq!(b.flush(), []);
}

/// `a` has asked `b` for one track, and `b` has accepted it under an alias. The request
/// as each end knows it, and the alias.
fn watching() -> (End, End, StreamId, StreamId, u64) {
    let (mut a, mut b) = pair();
    let wanted = track(&["m", "video", "p12"], "360p");
    let request = a.session.subscribe(wanted.clone(), Some(0)).unwrap();
    run(&mut a, &mut b);
    let events = b.take_events();
    let [
        Event::Wanted {
            request: at_b,
            track: asked,
            new_group: Some(0),
        },
    ] = &events[..]
    else {
        panic!("{events:?}")
    };
    assert_eq!(asked, &wanted);
    let alias = 20_000;
    assert!(b.session.accept_subscription(*at_b, alias));
    let at_b = *at_b;
    run(&mut a, &mut b);
    assert_eq!(a.take_events(), [Event::Subscribed { request, alias }]);
    (a, b, request, at_b, alias)
}

/// One group, written: its header and a frame for each of `frames`.
fn group_bytes(alias: u64, group: u64, frames: &[&[u8]]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut writer = GroupWriter::begin(alias, group, &mut bytes);
    for frame in frames {
        assert!(writer.object(frame, &mut bytes));
    }
    bytes
}

/// The frames in the `Group` events among `events`, read out as an end of the line would.
fn frames_in(events: &[Event]) -> Vec<(u64, u64, Vec<u8>)> {
    let mut reader = GroupReader::new();
    let mut out = Vec::new();
    for event in events {
        if let Event::Group { bytes, .. } = event {
            reader.feed(bytes, &mut out).unwrap();
        }
    }
    out.into_iter()
        .map(|object| (object.group, object.object, object.payload))
        .collect()
}

#[test]
fn one_track_is_asked_for_by_name_and_answered_with_its_alias() {
    let (_, mut b, _, at_b, alias) = watching();
    // The alias is now taken on this session: a second track cannot have it.
    let (mut c, mut d) = pair();
    let first = c
        .session
        .subscribe(track(&["m", "video", "p1"], "360p"), None)
        .unwrap();
    let second = c
        .session
        .subscribe(track(&["m", "video", "p2"], "360p"), None)
        .unwrap();
    run(&mut c, &mut d);
    let asked: Vec<StreamId> = d
        .take_events()
        .iter()
        .map(|event| match event {
            Event::Wanted {
                request,
                new_group: None,
                ..
            } => *request,
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(d.session.accept_subscription(asked[0], 7));
    assert!(!d.session.accept_subscription(asked[1], 7));
    // It can still be refused instead.
    d.session.refuse(asked[1], NOT_SUPPORTED, "no");
    run(&mut c, &mut d);
    assert_eq!(
        c.take_events(),
        [
            Event::Subscribed {
                request: first,
                alias: 7
            },
            Event::SubscribeRefused {
                request: second,
                code: NOT_SUPPORTED
            },
        ]
    );
    // Nothing is sent on a group for a track nobody subscribed to.
    assert!(b.session.open_group(alias + 1).is_none());
    assert!(b.session.open_group(alias).is_some());
    let _ = at_b;
}

#[test]
fn a_group_is_passed_on_as_it_arrives_and_read_out_at_the_end_of_the_line() {
    let (mut a, mut b, _, _, alias) = watching();
    let key = vec![7u8; 5000];
    let bytes = group_bytes(alias, 3, &[&key, b"delta one", b"delta two"]);
    let stream = b.session.open_group(alias).unwrap();
    // In three pieces, as a network would: each goes up at once, nothing is kept back.
    let (first, rest) = bytes.split_at(100);
    let (second, third) = rest.split_at(3000);
    let mut events = Vec::new();
    for (piece, fin) in [(first, false), (second, false), (third, true)] {
        b.session.pass(stream, piece.to_vec(), fin);
        run(&mut a, &mut b);
        let came = a.take_events();
        assert_eq!(came.len(), 1, "{} events", came.len());
        events.extend(came);
    }
    let shape: Vec<(bool, bool)> = events
        .iter()
        .map(|event| match event {
            Event::Group {
                alias: of,
                group: 3,
                start,
                fin,
                ..
            } if *of == alias => (*start, *fin),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(shape, [(true, false), (false, false), (false, true)]);
    // What went up is what was sent, byte for byte: a khef in the middle passes it on.
    let passed: Vec<u8> = events
        .iter()
        .flat_map(|event| match event {
            Event::Group { bytes, .. } => bytes.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert!(passed == bytes);
    let frames = frames_in(&events);
    assert_eq!(frames.len(), 3);
    assert!(frames[0] == (3, 0, key));
    assert_eq!(frames[2], (3, 2, b"delta two".to_vec()));
}

#[test]
fn a_group_that_overtakes_the_answer_to_the_subscription_waits_for_it() {
    let (mut a, mut b) = pair();
    let request = a
        .session
        .subscribe(track(&["m", "video", "p12"], "360p"), None)
        .unwrap();
    let to_b = a.flush();
    for wire in to_b {
        b.receive(wire);
    }
    let events = b.take_events();
    let [Event::Wanted { request: at_b, .. }] = events[..] else {
        panic!("{events:?}")
    };
    assert!(b.session.accept_subscription(at_b, 9));
    let stream = b.session.open_group(9).unwrap();
    b.session.pass(stream, group_bytes(9, 0, &[b"key"]), true);
    // The group's stream arrives first, the answer after.
    let mut to_a = b.flush();
    to_a.reverse();
    for wire in to_a {
        a.receive(wire);
    }
    let events = a.take_events();
    assert_eq!(events[0], Event::Subscribed { request, alias: 9 });
    assert_eq!(frames_in(&events[1..]), [(0, 0, b"key".to_vec())]);
    assert_eq!(a.closed, None);
}

#[test]
fn a_new_group_can_be_asked_for_on_a_subscription() {
    let (mut a, mut b, request, at_b, _) = watching();
    a.session.ask_for_group(request, 8);
    run(&mut a, &mut b);
    assert_eq!(
        b.take_events(),
        [Event::GroupWanted {
            request: at_b,
            group: 8
        }]
    );
    // The answer changes nothing for the one who asked.
    assert_eq!(a.take_events(), []);
    assert_eq!((a.closed, b.closed), (None, None));
}

#[test]
fn a_group_abandoned_part_way_is_reported_and_costs_nothing_else() {
    let (mut a, mut b, _, _, alias) = watching();
    let bytes = group_bytes(alias, 1, &[&[1u8; 2000]]);
    let stream = b.session.open_group(alias).unwrap();
    b.session.pass(stream, bytes[..500].to_vec(), false);
    run(&mut a, &mut b);
    let events = a.take_events();
    let [
        Event::Group {
            stream: at_a,
            start: true,
            fin: false,
            ..
        },
    ] = &events[..]
    else {
        panic!("{} events", events.len())
    };
    b.session.reset_group(stream);
    run(&mut a, &mut b);
    assert_eq!(a.take_events(), [Event::GroupReset { stream: *at_a }]);
    // Nothing more goes on the stream, and the next group is unaffected.
    b.session.pass(stream, bytes[500..].to_vec(), true);
    let next = b.session.open_group(alias).unwrap();
    b.session.pass(next, group_bytes(alias, 2, &[b"key"]), true);
    run(&mut a, &mut b);
    assert_eq!(frames_in(&a.take_events()), [(2, 0, b"key".to_vec())]);
}

#[test]
fn giving_up_a_subscription_tells_the_peer_and_stops_its_groups() {
    let (mut a, mut b, request, at_b, alias) = watching();
    let stream = b.session.open_group(alias).unwrap();
    let bytes = group_bytes(alias, 1, &[&[1u8; 2000]]);
    b.session.pass(stream, bytes[..500].to_vec(), false);
    run(&mut a, &mut b);
    a.take_events();
    a.session.unsubscribe(request);
    run(&mut a, &mut b);
    assert_eq!(b.take_events(), [Event::Unwanted { request: at_b }]);
    // The publisher has nothing left to send it on.
    assert!(b.session.open_group(alias).is_none());
    b.session.pass(stream, bytes[500..].to_vec(), true);
    run(&mut a, &mut b);
    assert_eq!(a.take_events(), []);
    assert_eq!((a.closed, b.closed), (None, None));
}

#[test]
fn the_publisher_can_end_a_subscription() {
    let (mut a, mut b, request, at_b, alias) = watching();
    b.session.end_subscription(at_b);
    run(&mut a, &mut b);
    assert_eq!(a.take_events(), [Event::SubscriptionEnded { request }]);
    assert!(b.session.open_group(alias).is_none());
    // The alias is free again for another track.
    let again = a
        .session
        .subscribe(track(&["m", "video", "p13"], "360p"), None)
        .unwrap();
    run(&mut a, &mut b);
    let events = b.take_events();
    let [Event::Wanted { request: at_b, .. }] = events[..] else {
        panic!("{events:?}")
    };
    assert!(b.session.accept_subscription(at_b, alias));
    run(&mut a, &mut b);
    assert_eq!(
        a.take_events(),
        [Event::Subscribed {
            request: again,
            alias
        }]
    );
}

#[test]
fn an_alias_given_for_a_subscription_must_be_free() {
    let (mut a, mut b, _, _, alias) = watching();
    // The peer answers a second subscription with the alias it already gave.
    let second = a
        .session
        .subscribe(track(&["m", "video", "p13"], "360p"), None)
        .unwrap();
    a.flush();
    let ok = Message::SubscribeOk(SubscribeOk {
        track_alias: alias,
        params: Params::default(),
        track_properties: Vec::new(),
    });
    a.session
        .on_data(second, StreamKind::Bidi, &encoded(&ok), false);
    assert_eq!(a.close_code(), Some(CloseCode::DuplicateTrackAlias));
    let _ = &mut b;
}

#[test]
fn a_padding_stream_is_read_and_thrown_away() {
    let (_, mut b) = pair();
    let mut padding = vec![];
    palaver_moq_wire::put_vi64(&mut padding, palaver_moq_wire::PADDING_STREAM);
    padding.extend([0; 100]);
    b.feed(10, StreamKind::Uni, &padding);
    b.feed(10, StreamKind::Uni, &vec![7; 100_000]);
    assert_eq!(b.flush(), []);
    assert_eq!(b.closed, None);
}

// --- Objects ---

/// `a` receives the control tracks of "m" from `b`, which has announced the plan track.
/// Returns the alias.
fn publishing() -> (End, End, u64) {
    let (mut a, mut b, _) = subscribed();
    let alias = publish(&mut b, track(&["m", "ctl", "plan"], "0"));
    run(&mut a, &mut b);
    a.take_events();
    b.take_events();
    (a, b, alias)
}

fn object(alias: u64, group: u64, payload: &[u8]) -> Event {
    Event::Object {
        alias,
        group,
        object: 0,
        payload: payload.to_vec(),
    }
}

/// The audio level as an object's properties.
fn level(level: u64) -> Vec<u8> {
    let mut out = vec![];
    palaver_moq_wire::encode_kvps(
        &[palaver_moq_wire::Kvp {
            ty: palaver_moq_wire::PROPERTY_AUDIO_LEVEL,
            value: palaver_moq_wire::Value::Int(level),
        }],
        &mut out,
    )
    .unwrap();
    out
}

#[test]
fn a_whole_object_is_sent_on_its_own_stream_and_arrives() {
    let (mut a, mut b, alias) = publishing();
    let plan = vec![0xab; 9000];
    assert!(b.session.send_object(alias, 7, &plan));
    assert!(b.session.send_object(alias, 8, b"next"));
    let wires = b.flush();
    // Two streams, each opened, written once and finished.
    assert_eq!(wires.len(), 2);
    assert!(wires.iter().all(|w| matches!(
        w,
        Wire::Data {
            kind: StreamKind::Uni,
            fin: true,
            ..
        }
    )));
    for wire in wires {
        a.receive(wire);
    }
    assert_eq!(
        a.take_events(),
        [object(alias, 7, &plan), object(alias, 8, b"next")]
    );
    assert_eq!((a.closed, b.closed), (None, None));
}

#[test]
fn an_object_may_arrive_a_byte_at_a_time() {
    let (mut a, mut b, alias) = publishing();
    b.session.send_object(alias, 3, b"a report, in pieces");
    let wires = b.flush();
    let [
        Wire::Data {
            stream,
            kind,
            bytes,
            ..
        },
    ] = &wires[..]
    else {
        panic!("{wires:?}")
    };
    for (i, byte) in bytes.iter().enumerate() {
        assert_eq!(a.take_events(), [], "after {i} bytes");
        a.session
            .on_data(*stream, *kind, &[*byte], i + 1 == bytes.len());
    }
    assert_eq!(a.take_events(), [object(alias, 3, b"a report, in pieces")]);
}

#[test]
fn an_object_that_overtakes_its_tracks_announcement_is_held_for_it() {
    let (mut a, mut b, _) = subscribed();
    let alias = publish(&mut b, track(&["m", "ctl", "plan"], "0"));
    b.session.send_object(alias, 5, b"plan five");
    let wires = b.flush();
    assert_eq!(wires.len(), 2, "the PUBLISH, then the object");
    // The object's stream arrives first.
    a.receive(wires[1].clone());
    assert_eq!(a.take_events(), []);
    a.receive(wires[0].clone());
    assert_eq!(
        a.take_events(),
        [
            Event::TrackOffered {
                track: track(&["m", "ctl", "plan"], "0"),
                alias
            },
            object(alias, 5, b"plan five")
        ]
    );
}

#[test]
fn objects_that_arrive_before_the_peers_setup_wait_for_it() {
    let mut a = End::new(Role::Client, setup(Some(1)));
    let mut b = End::new(Role::Server, setup(Some(1)));
    a.session.start();
    b.session.start();
    a.session.subscribe_tracks(ns(&["m", "ctl"])).unwrap();
    // Let b see the request and answer, but hold b's control stream back from a.
    for wire in a.flush() {
        b.receive(wire);
    }
    let mut wires = b.flush();
    let wanted = core::mem::take(&mut b.events);
    let Some(Event::TracksWanted { request, .. }) = wanted.last() else {
        panic!("{wanted:?}")
    };
    b.session.accept_tracks(*request);
    let alias = publish(&mut b, track(&["m", "ctl", "plan"], "0"));
    b.session.send_object(alias, 1, b"early");
    wires.extend(b.flush());
    let setup_wire = wires.remove(0);
    for wire in wires {
        a.receive(wire);
    }
    assert_eq!(a.take_events(), []);
    a.receive(setup_wire);
    let events = a.take_events();
    assert!(matches!(events[0], Event::Ready { .. }), "{events:?}");
    assert_eq!(events.last(), Some(&object(alias, 1, b"early")));
}

#[test]
fn only_so_many_streams_are_held_for_tracks_never_announced() {
    let (mut a, mut b, alias) = publishing();
    b.session.send_object(alias, 1, b"x");
    let wires = b.flush();
    let [Wire::Data { bytes, .. }] = &wires[..] else {
        panic!()
    };
    // The same object under aliases nobody announced: the alias is the second byte.
    let mut resets = vec![];
    for n in 0..40u64 {
        let mut stray = bytes.clone();
        stray[1] = 50;
        a.session
            .on_data(StreamId::Theirs(1000 + n), StreamKind::Uni, &stray, true);
        resets.extend(a.flush());
    }
    assert_eq!(a.take_events(), []);
    assert_eq!(a.closed, None);
    // All but the newest sixteen were given up, oldest first.
    assert_eq!(resets.len(), 40 - 16);
    assert_eq!(
        resets[0],
        Wire::Reset {
            stream: StreamId::Ours(1000)
        }
    );
}

#[test]
fn an_object_too_large_to_take_whole_costs_the_stream_not_the_session() {
    let (mut a, mut b, alias) = publishing();
    // Our own side will not send one.
    assert!(!b.session.send_object(alias, 1, &vec![0; 300_000]));
    assert_eq!(b.flush(), []);
    // One that arrives anyway: a header promising 300,000 bytes, then the bytes.
    b.session.send_object(alias, 1, b"x");
    let wires = b.flush();
    let [Wire::Data { bytes, .. }] = &wires[..] else {
        panic!()
    };
    let header = &bytes[..bytes.len() - 2];
    let mut large = header.to_vec();
    palaver_moq_wire::put_vi64(&mut large, 300_000);
    a.feed(77, StreamKind::Uni, &large);
    let mut reset = vec![];
    for _ in 0..300 {
        a.feed(77, StreamKind::Uni, &[0; 1000]);
        reset.extend(a.flush());
    }
    assert!(reset.contains(&Wire::Reset {
        stream: StreamId::Ours(77)
    }));
    assert_eq!(a.take_events(), []);
    assert_eq!(a.closed, None);
}

#[test]
fn a_stream_given_up_part_way_through_an_object_is_dropped_quietly() {
    let (mut a, mut b, alias) = publishing();
    b.session.send_object(alias, 1, b"cut short in transit");
    let wires = b.flush();
    let [Wire::Data { bytes, .. }] = &wires[..] else {
        panic!()
    };
    a.session.on_data(
        StreamId::Theirs(9),
        StreamKind::Uni,
        &bytes[..bytes.len() - 5],
        true,
    );
    assert_eq!(a.take_events(), []);
    assert_eq!(a.closed, None);
    // And one reset before anything of it was read.
    a.feed(10, StreamKind::Uni, &bytes[..1]);
    a.session.on_reset(StreamId::Theirs(10));
    assert_eq!(a.close_code(), None);
}

#[test]
fn objects_are_sent_only_on_tracks_being_published() {
    let (mut a, mut b, alias) = publishing();
    assert!(!b.session.send_object(alias + 1, 1, b"x"), "no such track");
    assert!(
        !a.session.send_object(alias, 1, b"x"),
        "the peer's alias, not ours"
    );
    let mut out = vec![];
    assert!(!b.session.encode_datagram(alias + 1, 1, &[], b"x", &mut out));
    assert!(out.is_empty());
    // Once the track has ended, nothing more goes out on it.
    b.session.end_track(alias);
    b.flush();
    assert!(!b.session.send_object(alias, 2, b"x"));
    assert!(!b.session.encode_datagram(alias, 2, &[], b"x", &mut out));
    assert_eq!(b.flush(), []);
    // Nor on one the peer refused.
    let refused = publish(&mut b, track(&["m", "video", "main", "p7"], "360p"));
    run(&mut a, &mut b);
    assert!(!b.session.send_object(refused, 1, b"x"));
}

#[test]
fn an_audio_frame_crosses_in_a_datagram_with_its_level_and_nothing_is_copied() {
    let (mut a, mut b) = pair();
    a.session.subscribe_tracks(ns(&["m", "audio"])).unwrap();
    run(&mut a, &mut b);
    b.session.accept_tracks(StreamId::Theirs(1));
    // A small alias, as media tracks should have: it is in every packet.
    let alias2 = 9;
    assert!(
        b.session
            .publish(track(&["m", "audio", "main", "p8"], "opus"), alias2)
    );
    run(&mut a, &mut b);

    let frame = [0x5a; 80];
    let mut datagram = vec![];
    assert!(
        b.session
            .encode_datagram(alias2, 4321, &level(57), &frame, &mut datagram)
    );
    // Seven bytes of header for an 80-byte frame, the level included.
    assert_eq!(datagram.len(), 80 + 7);
    // The alias is in every packet, so its size matters: one drawn from 62 bits takes nine
    // bytes instead of one.
    let wide = alias_of(&track(&["m", "audio", "main", "p9"], "opus"));
    assert!(
        b.session
            .publish(track(&["m", "audio", "main", "p9"], "opus"), wide)
    );
    let mut larger = vec![];
    assert!(
        b.session
            .encode_datagram(wide, 4321, &level(57), &frame, &mut larger)
    );
    assert_eq!(larger.len(), 80 + 15);
    let palaver_khef_core::session::DatagramIn::Object {
        alias: got,
        group,
        object,
        properties,
        payload,
    } = a.session.on_datagram(&datagram)
    else {
        panic!("{:?}", a.close_code())
    };
    assert_eq!((got, group, object), (alias2, 4321, 0));
    assert_eq!(payload, frame);
    assert_eq!(
        palaver_moq_wire::int_property(properties, palaver_moq_wire::PROPERTY_AUDIO_LEVEL),
        Ok(Some(57))
    );
    // The payload handed back is the caller's own bytes, not a copy.
    assert!(core::ptr::eq(
        payload.as_ptr(),
        datagram[datagram.len() - 80..].as_ptr()
    ));
}

#[test]
fn a_filler_is_told_apart_and_stray_datagrams_are_dropped() {
    use palaver_khef_core::session::DatagramIn;
    let (mut a, mut b, alias) = publishing();
    let mut filler = vec![];
    Session::encode_filler(&mut filler);
    assert_eq!(filler.len(), 5);
    assert_eq!(a.session.on_datagram(&filler), DatagramIn::Filler);

    // A datagram for the plan track is fine by the session (which tracks go in datagrams is
    // the khef's business); one for an alias nobody announced is dropped.
    let mut known = vec![];
    assert!(b.session.encode_datagram(alias, 1, &[], b"x", &mut known));
    assert!(matches!(
        a.session.on_datagram(&known),
        DatagramIn::Object { .. }
    ));
    let mut stray = known.clone();
    stray[1] = 60; // the alias
    assert_eq!(a.session.on_datagram(&stray), DatagramIn::Dropped);
    assert_eq!(a.close_code(), None);

    // After the track ends its datagrams are dropped too.
    b.session.end_track(alias);
    run(&mut a, &mut b);
    assert_eq!(a.session.on_datagram(&known), DatagramIn::Dropped);
    assert_eq!(a.close_code(), None);
}

#[test]
fn a_malformed_datagram_closes_the_session_and_one_before_setup_is_dropped() {
    use palaver_khef_core::session::DatagramIn;
    let (mut a, _, _) = publishing();
    // Flags with bits no datagram type has.
    assert_eq!(a.session.on_datagram(&[0x3f, 1, 2, 3]), DatagramIn::Dropped);
    assert_eq!(a.close_code(), Some(CloseCode::ProtocolViolation));

    let mut early = End::new(Role::Client, setup(Some(1)));
    early.session.start();
    assert_eq!(
        early.session.on_datagram(&[0x3f, 1, 2, 3]),
        DatagramIn::Dropped
    );
    assert_eq!(early.close_code(), None);
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
            #[expect(
                clippy::cast_possible_truncation,
                reason = "takes the low byte of a random number"
            )]
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
