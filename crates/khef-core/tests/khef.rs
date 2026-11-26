//! Several khefs joined by in-memory links.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test may stop where it stands"
)]

use palaver_khef_core::{
    Action, AudioOut, AudioPlan, GroupReader, GroupWriter, Khef, KhefConfig, LinkWatchConfig,
    Output, Role, StreamId, StreamKind, TopNConfig, VideoConfig, VideoFrom, VideoPlan, VideoRoute,
};
use palaver_moq_wire::{FullTrackName, PalaverExtensions, Setup};
use palaver_proto::{MeetingTag, ParticipantId, Source, Timestamp, Track, TrackName};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

type Id = u8;

const fn at(ms: u64) -> Timestamp {
    Timestamp::from_millis(ms)
}

fn meeting() -> MeetingTag {
    MeetingTag::new(b"m-7f3a").unwrap()
}

fn participant(node: Id) -> ParticipantId {
    ParticipantId::new(&[node; 16]).unwrap()
}

fn report_of(node: Id) -> Track {
    Track::Report {
        participant: participant(node),
    }
}

/// The name of `node`'s microphone track. Its alias in these tests is the node's number.
fn mic(node: Id) -> FullTrackName {
    TrackName {
        meeting: meeting(),
        track: Track::Audio {
            source: Source::Main,
            participant: participant(node),
            rendition: b"opus".to_vec(),
        },
    }
    .to_full_name()
    .unwrap()
}

fn config() -> KhefConfig {
    KhefConfig {
        meeting: meeting(),
        setup: Setup {
            palaver: Some(PalaverExtensions { version: 1 }),
            ..Setup::default()
        },
        link_watch: LinkWatchConfig::default(),
        control_lifetime: |track| match track {
            Track::Report { .. } => Some(Duration::from_secs(100)),
            _ => None,
        },
        max_control_tracks: 256,
        video: VideoConfig::default(),
    }
}

/// The alias of `node`'s camera track in these tests.
fn cam(node: Id) -> u64 {
    100 + u64::from(node)
}

/// The name of `node`'s camera track.
fn camera(node: Id) -> FullTrackName {
    TrackName {
        meeting: meeting(),
        track: Track::Video {
            source: Source::Main,
            participant: participant(node),
            rendition: b"360p".to_vec(),
        },
    }
    .to_full_name()
    .unwrap()
}

/// The neighbour of `from` on the way to `to`, over `edges`.
fn toward(edges: &[(Id, Id)], from: Id, to: Id) -> Option<Id> {
    let next = |at: Id| {
        edges
            .iter()
            .filter_map(move |(a, b)| match (*a == at, *b == at) {
                (true, _) => Some(*b),
                (_, true) => Some(*a),
                _ => None,
            })
    };
    // Walk out from `to`: the first time `from` is reached, it is from its next hop.
    let mut seen = BTreeSet::from([to]);
    let mut edge = vec![to];
    while !edge.is_empty() {
        let mut further = vec![];
        for at in edge {
            for beyond in next(at) {
                if beyond == from {
                    return Some(at);
                }
                if seen.insert(beyond) {
                    further.push(beyond);
                }
            }
        }
        edge = further;
    }
    None
}

/// A control object as a node's control logic saw it arrive.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Arrival {
    from: Id,
    track: Track,
    group: u64,
    payload: Vec<u8>,
}

struct Node {
    khef: Khef<Id>,
    /// The kind of each stream this node opened towards each peer.
    kinds: BTreeMap<(Id, u64), StreamKind>,
    /// Control objects handed up, in order.
    arrivals: Vec<Arrival>,
    dead: Vec<Id>,
    /// Audio played by this node's client: (alias, group).
    played: Vec<(u64, u64)>,
    /// What the khef said of this node's camera: (alias, wanted).
    produce: Vec<(u64, bool)>,
    /// The key frames it asked of the camera.
    keys: Vec<u64>,
    /// Frames of video given to this node's client: (alias, group, frame).
    frames: Vec<(u64, u64, Vec<u8>)>,
    readers: BTreeMap<(u64, u64), GroupReader>,
    /// Groups the client was told were given up: (alias, group).
    resets: Vec<(u64, u64)>,
}

impl Node {
    /// Do what the khef said about video.
    fn video(&mut self, output: Output<Id>) {
        match output {
            Output::Produce { alias, wanted } => self.produce.push((alias, wanted)),
            Output::KeyFrame { alias } => self.keys.push(alias),
            Output::Video {
                alias,
                group,
                start,
                bytes,
                fin,
            } => {
                if start {
                    self.readers.insert((alias, group), GroupReader::new());
                }
                let mut whole = vec![];
                self.readers
                    .get_mut(&(alias, group))
                    .unwrap()
                    .feed(&bytes, &mut whole)
                    .unwrap();
                for object in whole {
                    assert_eq!(object.group, group);
                    self.frames.push((alias, group, object.payload));
                }
                if fin {
                    self.readers.remove(&(alias, group));
                }
            }
            Output::VideoReset { alias, group } => {
                self.readers.remove(&(alias, group));
                self.resets.push((alias, group));
            }
            _ => panic!("not about video"),
        }
    }
}

/// Something in flight from one node to another.
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
    Datagram(Vec<u8>),
}

struct Net {
    nodes: BTreeMap<Id, Node>,
    /// Nodes that have stopped: nothing reaches them and they send nothing.
    stopped: BTreeSet<Id>,
    /// Datagrams sent, by (from, to): audio and fillers.
    datagrams: BTreeMap<(Id, Id), Vec<Vec<u8>>>,
    /// Control objects sent on streams, counted per (from, to).
    streams_opened: BTreeMap<(Id, Id), u64>,
    /// How many frames of each camera's groups have been made: (node, group).
    filmed: BTreeMap<(Id, u64), usize>,
    now: Timestamp,
}

impl Net {
    fn new(nodes: &[Id]) -> Self {
        Self {
            nodes: nodes
                .iter()
                .map(|id| {
                    let node = Node {
                        khef: Khef::new(config()),
                        kinds: BTreeMap::new(),
                        arrivals: vec![],
                        dead: vec![],
                        played: vec![],
                        produce: vec![],
                        keys: vec![],
                        frames: vec![],
                        readers: BTreeMap::new(),
                        resets: vec![],
                    };
                    (*id, node)
                })
                .collect(),
            stopped: BTreeSet::new(),
            datagrams: BTreeMap::new(),
            streams_opened: BTreeMap::new(),
            filmed: BTreeMap::new(),
            now: at(0),
        }
    }

    fn khef(&mut self, node: Id) -> &mut Khef<Id> {
        &mut self.nodes.get_mut(&node).unwrap().khef
    }

    /// Join `a` and `b`; `a` dialled.
    fn link(&mut self, a: Id, b: Id) {
        let now = self.now;
        self.khef(a).link_up(b, Role::Client, now);
        self.khef(b).link_up(a, Role::Server, now);
        self.settle();
    }

    /// Carry everything between the nodes until all are quiet.
    fn settle(&mut self) {
        loop {
            let mut wires: Vec<(Id, Id, Wire)> = vec![];
            let ids: Vec<Id> = self.nodes.keys().copied().collect();
            for id in ids {
                if self.stopped.contains(&id) {
                    continue;
                }
                let now = self.now;
                let node = self.nodes.get_mut(&id).unwrap();
                while let Some(output) = node.khef.poll_output() {
                    match output {
                        Output::Stream { peer, action } => {
                            let flip = |stream| match stream {
                                StreamId::Ours(n) => StreamId::Theirs(n),
                                StreamId::Theirs(n) => StreamId::Ours(n),
                            };
                            match action {
                                Action::Open {
                                    stream: StreamId::Ours(n),
                                    kind,
                                } => {
                                    node.kinds.insert((peer, n), kind);
                                    *self.streams_opened.entry((id, peer)).or_default() += 1;
                                }
                                Action::Open { .. } => panic!("opened a stream of the peer's"),
                                Action::Send { stream, bytes, fin } => {
                                    let kind = match stream {
                                        StreamId::Ours(n) => node.kinds[&(peer, n)],
                                        StreamId::Theirs(_) => StreamKind::Bidi,
                                    };
                                    let wire = Wire::Data {
                                        stream: flip(stream),
                                        kind,
                                        bytes,
                                        fin,
                                    };
                                    wires.push((id, peer, wire));
                                }
                                Action::Reset { stream } => {
                                    let wire = Wire::Reset {
                                        stream: flip(stream),
                                    };
                                    wires.push((id, peer, wire));
                                }
                                Action::Close { code, reason } => {
                                    panic!(
                                        "node {id} closed its session with {peer}: {code:?} {reason}"
                                    )
                                }
                            }
                        }
                        Output::Datagram { peer, bytes } => {
                            wires.push((id, peer, Wire::Datagram(bytes)));
                        }
                        // The control logic: take anything newer than what is held.
                        Output::Control {
                            from,
                            track,
                            group,
                            payload,
                        } => {
                            node.arrivals.push(Arrival {
                                from,
                                track: track.clone(),
                                group,
                                payload: payload.clone(),
                            });
                            node.khef
                                .accept_control(Some(&from), &track, group, &payload, now);
                        }
                        Output::LinkDead { peer } => node.dead.push(peer),
                        video => node.video(video),
                    }
                }
            }
            if wires.is_empty() {
                return;
            }
            for (from, to, wire) in wires {
                self.deliver(from, to, wire);
            }
        }
    }

    fn deliver(&mut self, from: Id, to: Id, wire: Wire) {
        if self.stopped.contains(&to) {
            return;
        }
        let now = self.now;
        match wire {
            Wire::Data {
                stream,
                kind,
                bytes,
                fin,
            } => self.khef(to).on_data(&from, stream, kind, &bytes, fin, now),
            Wire::Reset { stream } => self.khef(to).on_reset(&from, stream, now),
            Wire::Datagram(bytes) => {
                self.datagrams
                    .entry((from, to))
                    .or_default()
                    .push(bytes.clone());
                let out = self.khef(to).on_datagram(&from, &bytes, now);
                self.carry_audio(to, out, &bytes);
            }
        }
    }

    /// Do what a khef said to do with an audio datagram.
    fn carry_audio(&mut self, at_node: Id, out: Option<AudioOut<Id>>, bytes: &[u8]) {
        let Some(out) = out else {
            return;
        };
        // Wherever a packet is, the khef says where the frame lies in it: every speaker's
        // frame is 80 bytes of its own number.
        assert_eq!(
            bytes.get(out.payload.clone()),
            Some(&[u8::try_from(out.alias).unwrap(); 80][..]),
            "at node {at_node}"
        );
        if out.for_client {
            self.nodes
                .get_mut(&at_node)
                .unwrap()
                .played
                .push((out.alias, out.group));
        }
        for peer in out.forward_to {
            self.deliver(at_node, peer, Wire::Datagram(bytes.to_vec()));
        }
    }

    /// `node` makes a control object and publishes it.
    fn publish(&mut self, node: Id, track: &Track, group: u64, payload: &[u8]) {
        let now = self.now;
        assert!(
            self.khef(node)
                .accept_control(None, track, group, payload, now)
        );
        self.settle();
    }

    /// `node`'s microphone produces a frame. Returns the datagram.
    fn speak(&mut self, node: Id, group: u64, level: u8) -> Vec<u8> {
        let now = self.now;
        let mut bytes = vec![];
        let out = self.khef(node).local_audio(
            u64::from(node),
            group,
            level,
            &[node; 80],
            &mut bytes,
            now,
        );
        assert!(out.is_some(), "node {node} has no audio track in its plan");
        assert_eq!(out.as_ref().map(|o| o.level), Some(level));
        self.carry_audio(node, out, &bytes);
        bytes
    }

    /// Give every node the audio plan for a tree with these edges, over all nodes' mics.
    fn audio_tree(&mut self, edges: &[(Id, Id)], n: usize) {
        let ids: Vec<Id> = self.nodes.keys().copied().collect();
        let tracks: BTreeMap<u64, FullTrackName> =
            ids.iter().map(|id| (u64::from(*id), mic(*id))).collect();
        for id in &ids {
            let links = edges
                .iter()
                .filter_map(|(a, b)| match (a == id, b == id) {
                    (true, _) => Some(*b),
                    (_, true) => Some(*a),
                    _ => None,
                })
                .collect();
            let plan = AudioPlan {
                links,
                tracks: tracks.clone(),
                own: BTreeSet::from([u64::from(*id)]),
                top_n: TopNConfig {
                    n,
                    ..TopNConfig::default()
                },
            };
            let now = self.now;
            self.khef(*id).set_audio_plan(plan, now);
        }
        self.settle();
    }

    /// Move the clock on by `ms`, polling every khef each 50 ms.
    fn run(&mut self, ms: u64) {
        for _ in 0..ms / 50 {
            self.now = self.now.saturating_add(Duration::from_millis(50));
            let ids: Vec<Id> = self.nodes.keys().copied().collect();
            for id in ids {
                if !self.stopped.contains(&id) {
                    let now = self.now;
                    self.khef(id).poll(now);
                }
            }
            self.settle();
        }
    }

    /// Give every node the video plan for these edges, over all nodes' cameras.
    fn video_tree(&mut self, edges: &[(Id, Id)]) {
        let ids: Vec<Id> = self.nodes.keys().copied().collect();
        for id in &ids {
            self.video_plan_at(*id, edges);
        }
        self.settle();
    }

    /// Give one node the video plan for these edges; the rest are not carried yet.
    fn video_plan_at(&mut self, node: Id, edges: &[(Id, Id)]) {
        let ids: Vec<Id> = self.nodes.keys().copied().collect();
        let tracks = ids
            .iter()
            .filter_map(|of| {
                let from = if *of == node {
                    VideoFrom::Here
                } else {
                    VideoFrom::Link(toward(edges, node, *of)?)
                };
                let route = VideoRoute {
                    name: camera(*of),
                    from,
                };
                Some((cam(*of), route))
            })
            .collect();
        let now = self.now;
        self.khef(node).set_video_plan(VideoPlan { tracks }, now);
    }

    /// `node`'s client begins to watch `of`'s camera.
    fn watch(&mut self, node: Id, of: Id) {
        let now = self.now;
        self.khef(node).watch(cam(of), now);
        self.settle();
    }

    fn unwatch(&mut self, node: Id, of: Id) {
        let now = self.now;
        self.khef(node).unwatch(cam(of), now);
        self.settle();
    }

    /// `node`'s camera makes these frames of `group`, the first of them beginning it and,
    /// if `whole`, the last ending it.
    fn film(&mut self, node: Id, group: u64, begin: bool, frames: &[&[u8]], whole: bool) {
        let now = self.now;
        let mut bytes = vec![];
        // A writer begun again writes the header again: keep only the objects' bytes.
        let mut writer = GroupWriter::begin(cam(node), group, &mut bytes);
        if !begin {
            bytes.clear();
            // The objects written before are counted so that these follow them.
            for _ in 0..self.filmed.get(&(node, group)).copied().unwrap_or(0) {
                assert!(writer.object(b"", &mut vec![]));
            }
        }
        for (i, frame) in frames.iter().enumerate() {
            assert!(writer.object(frame, &mut bytes));
            let fin = whole && i + 1 == frames.len();
            self.khef(node)
                .local_video(cam(node), group, begin && i == 0, &bytes, fin, now);
            bytes.clear();
        }
        *self.filmed.entry((node, group)).or_default() += frames.len();
        self.settle();
    }

    fn frames(&self, node: Id) -> Vec<(u64, u64, &[u8])> {
        self.nodes[&node]
            .frames
            .iter()
            .map(|(alias, group, frame)| (*alias, *group, &frame[..]))
            .collect()
    }

    fn produce(&self, node: Id) -> &[(u64, bool)] {
        &self.nodes[&node].produce
    }

    fn arrivals(&self, node: Id) -> &[Arrival] {
        &self.nodes[&node].arrivals
    }

    fn played(&self, node: Id) -> &[(u64, u64)] {
        &self.nodes[&node].played
    }

    fn sent(&self, from: Id, to: Id) -> usize {
        self.datagrams.get(&(from, to)).map_or(0, Vec::len)
    }
}

// --- Control tracks ---

#[test]
fn a_control_object_reaches_the_neighbour_once() {
    let mut net = Net::new(&[1, 2]);
    net.link(1, 2);
    net.publish(1, &Track::Plan, 7, b"plan seven");
    assert_eq!(
        net.arrivals(2),
        [Arrival {
            from: 1,
            track: Track::Plan,
            group: 7,
            payload: b"plan seven".to_vec()
        }]
    );
    assert_eq!(net.arrivals(1), [], "nothing comes back");
    assert_eq!(
        net.khef(2).latest_control(&Track::Plan, at(0)),
        Some((7, &b"plan seven"[..]))
    );
}

#[test]
fn a_control_object_crosses_a_line_of_khefs_and_is_not_sent_back() {
    let mut net = Net::new(&[1, 2, 3]);
    net.link(1, 2);
    net.link(2, 3);
    net.publish(1, &report_of(1), 100, b"report from 1");
    assert_eq!(net.arrivals(3).len(), 1);
    assert_eq!(net.arrivals(3)[0].from, 2);
    assert_eq!(net.arrivals(3)[0].payload, b"report from 1");
    assert_eq!(net.arrivals(1), []);
    // And the other way.
    net.publish(3, &report_of(3), 5, b"report from 3");
    assert_eq!(net.arrivals(1).len(), 1);
    assert_eq!(net.arrivals(1)[0].track, report_of(3));
}

#[test]
fn a_newer_object_follows_on_the_same_track_and_an_older_one_goes_nowhere() {
    let mut net = Net::new(&[1, 2]);
    net.link(1, 2);
    net.publish(1, &Track::Plan, 7, b"seven");
    net.publish(1, &Track::Plan, 8, b"eight");
    let groups: Vec<u64> = net.arrivals(2).iter().map(|a| a.group).collect();
    assert_eq!(groups, [7, 8]);
    let now = net.now;
    assert!(
        !net.khef(1)
            .accept_control(None, &Track::Plan, 6, b"six", now)
    );
    net.settle();
    assert_eq!(net.arrivals(2).len(), 2);
    // The track was announced once; each object had a stream of its own.
    let streams = net.streams_opened[&(1, 2)];
    net.publish(1, &Track::Plan, 9, b"nine");
    assert_eq!(net.streams_opened[&(1, 2)], streams + 1);
}

#[test]
fn a_khef_that_connects_later_is_given_the_latest_of_every_control_track() {
    let mut net = Net::new(&[1, 2, 3]);
    net.link(1, 2);
    net.publish(1, &Track::Plan, 7, b"old plan");
    net.publish(1, &Track::Plan, 8, b"plan");
    net.publish(1, &report_of(1), 40, b"report 1");
    net.publish(2, &report_of(2), 90, b"report 2");
    // 3 joins through 2.
    net.link(2, 3);
    let mut got: Vec<(Track, u64)> = net
        .arrivals(3)
        .iter()
        .map(|a| (a.track.clone(), a.group))
        .collect();
    got.sort();
    let mut want = vec![(Track::Plan, 8), (report_of(1), 40), (report_of(2), 90)];
    want.sort();
    assert_eq!(got, want);
    // 3 held nothing, so 2 learnt nothing from it.
    assert_eq!(net.arrivals(2).len(), 3);
}

#[test]
fn in_a_triangle_everyone_gets_the_object_and_the_flood_stops() {
    let mut net = Net::new(&[1, 2, 3]);
    net.link(1, 2);
    net.link(2, 3);
    net.link(1, 3);
    net.publish(1, &Track::Plan, 7, b"plan");
    for node in [2, 3] {
        // Once from 1, and once more from the other neighbour, which the control logic
        // turns away as not new.
        let from: Vec<Id> = net.arrivals(node).iter().map(|a| a.from).collect();
        assert_eq!(from.len(), 2, "node {node}: {from:?}");
        assert!(from.contains(&1));
    }
    assert_eq!(net.arrivals(1), []);
}

#[test]
fn a_control_object_of_another_meeting_is_not_handed_up() {
    // The peer is a bare session that announces a plan track under another meeting's name.
    use palaver_khef_core::Session;
    let mut khef: Khef<Id> = Khef::new(config());
    khef.link_up(9, Role::Server, at(0));
    let mut peer = Session::new(Role::Client, config().setup);
    peer.start();
    let other = MeetingTag::new(b"another").unwrap();
    let mut asked = None;
    // Carry bytes both ways until quiet.
    let mut kinds: BTreeMap<u64, StreamKind> = BTreeMap::new();
    let mut peer_kinds: BTreeMap<u64, StreamKind> = BTreeMap::new();
    for round in 0..20 {
        let flip = |stream| match stream {
            StreamId::Ours(n) => StreamId::Theirs(n),
            StreamId::Theirs(n) => StreamId::Ours(n),
        };
        while let Some(output) = khef.poll_output() {
            match output {
                Output::Stream {
                    action:
                        Action::Open {
                            stream: StreamId::Ours(n),
                            kind,
                        },
                    ..
                } => {
                    kinds.insert(n, kind);
                }
                Output::Stream {
                    action: Action::Send { stream, bytes, fin },
                    ..
                } => {
                    let kind = match stream {
                        StreamId::Ours(n) => kinds[&n],
                        StreamId::Theirs(_) => StreamKind::Bidi,
                    };
                    peer.on_data(flip(stream), kind, &bytes, fin);
                }
                Output::Control { .. } => panic!("handed up a stranger's plan"),
                _ => {}
            }
        }
        while let Some(event) = peer.poll_event() {
            if let palaver_khef_core::Event::TracksWanted { request, .. } = event {
                asked = Some(request);
            }
        }
        if round == 5 {
            // The khef asked for control tracks; give it one from elsewhere, and an object.
            peer.accept_tracks(asked.unwrap());
            let name = TrackName {
                meeting: other.clone(),
                track: Track::Plan,
            };
            assert!(peer.publish(name.to_full_name().unwrap(), 5));
            assert!(peer.send_object(5, 1, b"not yours"));
        }
        while let Some(action) = peer.poll_action() {
            match action {
                Action::Open {
                    stream: StreamId::Ours(n),
                    kind,
                } => {
                    peer_kinds.insert(n, kind);
                }
                Action::Send { stream, bytes, fin } => {
                    let kind = match stream {
                        StreamId::Ours(n) => peer_kinds[&n],
                        StreamId::Theirs(_) => StreamKind::Bidi,
                    };
                    khef.on_data(&9, flip(stream), kind, &bytes, fin, at(0));
                }
                _ => {}
            }
        }
    }
    assert!(asked.is_some());
}

// --- Audio ---

/// 1 - 2 - 3 in a line, linked and wired for audio.
fn line() -> Net {
    let mut net = Net::new(&[1, 2, 3]);
    net.link(1, 2);
    net.link(2, 3);
    net.audio_tree(&[(1, 2), (2, 3)], 3);
    net
}

#[test]
fn audio_crosses_the_tree_exactly_as_it_was_sent_and_is_played_by_everyone_else() {
    let mut net = line();
    let sent = net.speak(1, 500, 100);
    assert_eq!(sent.len(), 80 + 7, "one byte of alias");
    assert_eq!(net.played(1), [], "nobody hears themselves");
    assert_eq!(net.played(2), [(1, 500)]);
    assert_eq!(net.played(3), [(1, 500)]);
    // The very bytes node 1 made are what crossed both links.
    assert_eq!(net.datagrams[&(1, 2)], core::slice::from_ref(&sent));
    assert_eq!(net.datagrams[&(2, 3)], core::slice::from_ref(&sent));
    // And nothing went back.
    assert_eq!(net.sent(2, 1), 0);
    assert_eq!(net.sent(3, 2), 0);
}

#[test]
fn audio_from_the_middle_goes_both_ways() {
    let mut net = line();
    net.speak(2, 1, 100);
    assert_eq!(net.played(1), [(2, 1)]);
    assert_eq!(net.played(3), [(2, 1)]);
    assert_eq!(net.played(2), []);
}

#[test]
fn audio_goes_only_over_links_of_the_tree() {
    // A triangle whose tree is 1-2, 2-3: the 1-3 link carries control only.
    let mut net = Net::new(&[1, 2, 3]);
    net.link(1, 2);
    net.link(2, 3);
    net.link(1, 3);
    net.audio_tree(&[(1, 2), (2, 3)], 3);
    net.speak(1, 9, 100);
    assert_eq!(net.sent(1, 3), 0);
    assert_eq!(net.sent(1, 2), 1);
    assert_eq!(net.sent(2, 3), 1);
    assert_eq!(net.played(3), [(1, 9)]);
}

#[test]
fn a_packet_that_arrives_twice_is_passed_on_once() {
    let mut net = line();
    let sent = net.speak(1, 77, 100);
    // The same datagram again, as a second path would deliver it.
    net.deliver(1, 2, Wire::Datagram(sent));
    assert_eq!(net.played(2), [(1, 77)]);
    assert_eq!(net.sent(2, 3), 1);
}

#[test]
fn only_the_loudest_n_cross_a_link() {
    // A hub, 10, with five leaves. Two tracks per link.
    let leaves = [1, 2, 3, 4, 5];
    let mut net = Net::new(&[10, 1, 2, 3, 4, 5]);
    let edges: Vec<(Id, Id)> = leaves.iter().map(|leaf| (10, *leaf)).collect();
    for (hub, leaf) in &edges {
        net.link(*hub, *leaf);
    }
    net.audio_tree(&edges, 2);
    // Leaves 1, 2 and 3 talk at once for two seconds; 3 is the quietest.
    for frame in 0..100u64 {
        net.now = at(frame * 20);
        net.speak(1, frame, 110);
        net.speak(2, frame, 100);
        net.speak(3, frame, 85);
    }
    // Leaf 5 hears 1 and 2, and of 3 only its first packets, before the louder two
    // had been heard from.
    let heard = |net: &Net, node: Id, alias: u64| {
        net.played(node).iter().filter(|(a, _)| *a == alias).count()
    };
    assert_eq!(heard(&net, 5, 1), 100);
    assert_eq!(heard(&net, 5, 2), 100);
    assert!(heard(&net, 5, 3) <= 2, "{}", heard(&net, 5, 3));
    // A talker hears the other loud one and the quiet one: its own track is not a
    // candidate on the link back to it.
    assert_eq!(heard(&net, 1, 2), 100);
    assert!(heard(&net, 1, 3) >= 98);
    assert_eq!(heard(&net, 1, 1), 0);
    // No link ever carried more than two tracks' worth.
    assert!(net.sent(10, 5) <= 202, "{}", net.sent(10, 5));
}

#[test]
fn when_more_than_n_start_together_the_wait_for_a_slot_is_paid_once_not_at_every_khef() {
    // Talkers on a hub, 10; listeners 20, 30 and 40 in a line behind it. Three tracks per
    // link. 1, 2 and 5 are talking. Then 4 and 3 start in the same instant, 4 being heard
    // first: it is louder than 5 and takes its slot, and 3, louder still, has to wait out
    // 4's hold.
    let mut net = Net::new(&[10, 1, 2, 3, 4, 5, 20, 30, 40]);
    let edges = [
        (10, 1),
        (10, 2),
        (10, 3),
        (10, 4),
        (10, 5),
        (10, 20),
        (20, 30),
        (30, 40),
    ];
    for (a, b) in edges {
        net.link(a, b);
    }
    net.audio_tree(&edges, 3);
    for frame in 0..100u64 {
        net.now = at(frame * 20);
        for (speaker, level) in [(1, 110), (2, 105), (5, 90)] {
            net.speak(speaker, frame, level);
        }
        if frame >= 20 {
            net.speak(4, frame, 95);
            net.speak(3, frame, 100);
        }
    }
    let missed =
        |net: &Net, listener: Id| 80 - net.played(listener).iter().filter(|(a, _)| *a == 3).count();
    // 4 cannot be replaced for 250 ms after it took the slot: 13 frames. It took the slot at
    // every selector on the way at the same moment, so when the hub gives it to 3, each of
    // the others does too, on the same packet.
    let missed = [missed(&net, 20), missed(&net, 30), missed(&net, 40)];
    assert_eq!(missed, [13, 13, 13]);
    // After that every listener hears exactly the loudest three.
    for listener in [20, 30, 40] {
        let late: Vec<u64> = net
            .played(listener)
            .iter()
            .filter(|(_, frame)| *frame >= 50)
            .map(|(alias, _)| *alias)
            .collect();
        assert_eq!(late.len(), 150, "node {listener}");
        assert!(!late.contains(&4), "node {listener}");
    }
}

#[test]
fn audio_before_the_plan_or_outside_it_goes_nowhere() {
    let mut net = Net::new(&[1, 2]);
    net.link(1, 2);
    // No audio plan yet.
    let mut bytes = vec![];
    let now = net.now;
    assert_eq!(
        net.khef(1).local_audio(1, 0, 60, &[1; 80], &mut bytes, now),
        None
    );
    assert!(bytes.is_empty());
    net.audio_tree(&[(1, 2)], 3);
    // An alias that is not this node's.
    assert_eq!(
        net.khef(1).local_audio(2, 0, 60, &[1; 80], &mut bytes, now),
        None
    );
    // A datagram under an alias no plan knows.
    let sent = net.speak(1, 1, 100);
    let mut stray = sent;
    stray[1] = 99;
    assert_eq!(net.khef(2).on_datagram(&1, &stray, now), None);
    assert_eq!(net.played(2), [(1, 1)]);
}

#[test]
fn when_the_plan_drops_a_track_its_packets_stop_and_a_new_one_starts() {
    let mut net = line();
    net.speak(3, 1, 100);
    assert_eq!(net.played(1), [(3, 1)]);
    // The plan now has 3 gone from the tree, with only 1 and 2 left in it.
    let tracks: BTreeMap<u64, FullTrackName> = [(1, mic(1)), (2, mic(2))].into();
    for (node, links) in [(1u8, vec![2u8]), (2, vec![1])] {
        let plan = AudioPlan {
            links: links.into_iter().collect(),
            tracks: tracks.clone(),
            own: BTreeSet::from([u64::from(node)]),
            top_n: TopNConfig::default(),
        };
        let now = net.now;
        net.khef(node).set_audio_plan(plan, now);
    }
    net.settle();
    // 3 still sends, on a link 2 no longer takes audio from.
    let before = net.sent(2, 1);
    let mut bytes = vec![];
    let now = net.now;
    let out = net.khef(3).local_audio(3, 2, 60, &[3; 80], &mut bytes, now);
    net.carry_audio(3, out, &bytes);
    assert_eq!(net.sent(2, 1), before);
    assert_eq!(net.played(1), [(3, 1)]);
    // 1 and 2 still hear each other.
    net.speak(1, 5, 100);
    assert_eq!(net.played(2).last(), Some(&(1, 5)));
}

#[test]
fn a_link_added_to_the_tree_later_starts_carrying_audio() {
    let mut net = Net::new(&[1, 2, 3]);
    net.link(1, 2);
    net.audio_tree(&[(1, 2)], 3);
    net.speak(1, 1, 100);
    assert_eq!(net.played(2), [(1, 1)]);
    assert_eq!(net.played(3), []);
    net.link(2, 3);
    net.audio_tree(&[(1, 2), (2, 3)], 3);
    net.speak(1, 2, 100);
    assert_eq!(net.played(3), [(1, 2)]);
    net.speak(3, 9, 100);
    assert_eq!(net.played(1), [(3, 9)]);
}

// --- Watching links ---

#[test]
fn idle_links_carry_fillers_and_stay_up() {
    let mut net = Net::new(&[1, 2]);
    net.link(1, 2);
    net.run(5000);
    assert_eq!(net.nodes[&1].dead, []);
    assert_eq!(net.nodes[&2].dead, []);
    // Five a second each way.
    let fillers = net.sent(1, 2);
    assert!((23..=25).contains(&fillers), "{fillers} fillers in 5 s");
    assert!(net.datagrams[&(1, 2)].iter().all(|d| d.len() == 5));
}

#[test]
fn a_link_carrying_audio_needs_no_fillers() {
    let mut net = Net::new(&[1, 2]);
    net.link(1, 2);
    net.audio_tree(&[(1, 2)], 3);
    for frame in 0..100u64 {
        net.now = at(frame * 20);
        net.speak(1, frame, 100);
        if frame % 5 == 4 {
            let now = net.now;
            net.khef(1).poll(now);
            net.khef(2).poll(now);
            net.settle();
        }
    }
    // 1 sent only audio. 2, which had nothing to say, sent fillers.
    assert_eq!(net.sent(1, 2), 100);
    let sizes: BTreeSet<usize> = net.datagrams[&(1, 2)].iter().map(Vec::len).collect();
    assert!(sizes.iter().all(|size| *size >= 86), "{sizes:?}");
    assert!(net.sent(2, 1) >= 8);
    assert_eq!(net.nodes[&1].dead, []);
    assert_eq!(net.nodes[&2].dead, []);
}

#[test]
fn a_neighbour_that_stops_is_reported_dead_after_seven_hundred_milliseconds() {
    let mut net = Net::new(&[1, 2, 3]);
    net.link(1, 2);
    net.link(2, 3);
    net.run(1000);
    net.stopped.insert(3);
    net.run(650);
    assert_eq!(net.nodes[&2].dead, []);
    net.run(100);
    assert_eq!(net.nodes[&2].dead, [3]);
    assert_eq!(net.nodes[&1].dead, [], "the other link is untouched");
    let left: Vec<Id> = net.khef(2).neighbours().copied().collect();
    assert_eq!(left, [1]);
    // Control still flows over what is left, and nothing is sent to the dead.
    net.publish(1, &Track::Plan, 3, b"plan");
    assert_eq!(net.arrivals(2).len(), 1);
    assert!(net.khef(2).next_wake().is_some());
}

#[test]
fn a_khef_with_no_links_has_nothing_to_wake_for() {
    let mut khef: Khef<Id> = Khef::new(config());
    assert_eq!(khef.next_wake(), None);
    khef.poll(at(10_000));
    assert_eq!(khef.poll_output(), None);
    // A control object made with nobody to send it to is still kept for whoever comes.
    assert!(khef.accept_control(None, &Track::Plan, 1, b"plan", at(0)));
    assert_eq!(
        khef.latest_control(&Track::Plan, at(0)),
        Some((1, &b"plan"[..]))
    );
}

// --- Video ---

const LINE: &[(Id, Id)] = &[(1, 2), (2, 3)];

/// Nodes 1, 2, 3 in a line, each with the video plan for it.
fn video_line() -> Net {
    let mut net = Net::new(&[1, 2, 3]);
    net.link(1, 2);
    net.link(2, 3);
    net.video_tree(LINE);
    net
}

/// Node 2 in the middle of 1, 3 and 4.
fn star() -> Net {
    let mut net = Net::new(&[1, 2, 3, 4]);
    net.link(1, 2);
    net.link(2, 3);
    net.link(2, 4);
    net.video_tree(&[(1, 2), (2, 3), (2, 4)]);
    net
}

#[test]
fn a_camera_nobody_watches_is_not_made_and_goes_nowhere() {
    let mut net = video_line();
    let before = net.streams_opened.clone();
    assert_eq!(net.produce(1), []);
    net.film(1, 0, true, &[b"key", b"delta"], true);
    assert_eq!(net.streams_opened, before);
    assert_eq!(net.frames(2), []);
}

#[test]
fn a_watched_camera_is_asked_for_along_the_line_and_its_pictures_come_back() {
    let mut net = video_line();
    net.watch(3, 1);
    assert_eq!(net.produce(1), [(cam(1), true)]);
    assert_eq!(net.produce(2), [], "only the node whose camera it is");
    net.film(1, 0, true, &[b"key", b"delta one"], false);
    // A group is passed on as it comes, not when it is whole.
    assert_eq!(
        net.frames(3),
        [(cam(1), 0, &b"key"[..]), (cam(1), 0, &b"delta one"[..])]
    );
    net.film(1, 0, false, &[b"delta two"], true);
    net.film(1, 1, true, &[b"key again"], true);
    let got: Vec<&[u8]> = net.frames(3).iter().map(|(_, _, frame)| *frame).collect();
    assert_eq!(got, [&b"key"[..], b"delta one", b"delta two", b"key again"]);
    assert_eq!(
        net.frames(2),
        [],
        "the khef in the middle only passes it on"
    );
    assert_eq!(net.frames(1), [], "nobody is shown their own camera");
}

#[test]
fn two_watchers_share_one_request_and_the_camera_stops_when_the_last_has_gone() {
    let mut net = star();
    net.watch(3, 1);
    let asked = net.streams_opened[&(2, 1)];
    net.watch(4, 1);
    assert_eq!(
        net.streams_opened[&(2, 1)],
        asked,
        "node 2 asks node 1 once"
    );
    assert_eq!(net.produce(1), [(cam(1), true)]);
    net.film(1, 0, true, &[b"key"], true);
    assert_eq!(net.frames(3), [(cam(1), 0, &b"key"[..])]);
    net.unwatch(3, 1);
    assert_eq!(net.produce(1), [(cam(1), true)]);
    net.film(1, 1, true, &[b"key"], true);
    assert_eq!(net.frames(3).len(), 1, "no more for who left");
    assert_eq!(net.frames(4).len(), 2, "all of it for who stays");
    net.unwatch(4, 1);
    assert_eq!(net.produce(1), [(cam(1), true), (cam(1), false)]);
    let before = net.streams_opened.clone();
    net.film(1, 2, true, &[b"key"], true);
    assert_eq!(net.streams_opened, before);
}

#[test]
fn whoever_begins_to_watch_part_way_gets_a_key_frame_asked_for_once_for_all_who_need_it() {
    let mut net = star();
    net.watch(3, 1);
    assert_eq!(net.nodes[&1].keys, [], "what begins, begins with one");
    net.film(1, 0, true, &[b"key"], false);
    net.watch(4, 1);
    assert_eq!(net.nodes[&1].keys, [cam(1)], "asked for at once");
    net.film(1, 0, false, &[b"delta"], false);
    assert_eq!(net.frames(3).len(), 2);
    assert_eq!(net.frames(4), [], "a group cannot be joined part way");
    // Another who needs the same group adds nothing: node 2 has already asked for it.
    net.unwatch(3, 1);
    net.watch(3, 1);
    net.run(2000);
    assert_eq!(net.nodes[&1].keys, [cam(1)]);
    net.film(1, 1, true, &[b"key again"], true);
    assert_eq!(net.frames(4), [(cam(1), 1, &b"key again"[..])]);
    assert_eq!(net.frames(3).last(), Some(&(cam(1), 1, &b"key again"[..])));
    // Whoever comes after that group began needs the one after it.
    net.unwatch(4, 1);
    net.watch(4, 1);
    assert_eq!(net.nodes[&1].keys, [cam(1), cam(1)]);
}

#[test]
fn a_watcher_that_lost_its_place_asks_for_a_key_frame_through_every_khef() {
    let mut net = video_line();
    net.watch(3, 1);
    net.film(1, 0, true, &[b"key"], true);
    let now = net.now;
    net.khef(3).ask_key_frame(cam(1), now);
    net.settle();
    assert_eq!(net.nodes[&1].keys, [cam(1)]);
    // Asked again before the group has come, there is nothing to add.
    net.khef(3).ask_key_frame(cam(1), now);
    net.settle();
    assert_eq!(net.nodes[&1].keys, [cam(1)]);
    net.film(1, 1, true, &[b"key"], true);
    net.khef(3).ask_key_frame(cam(1), now);
    net.settle();
    assert_eq!(net.nodes[&1].keys, [cam(1), cam(1)]);
    // Nobody can ask for a key frame of what they do not watch.
    net.khef(2).ask_key_frame(cam(1), now);
    net.settle();
    assert_eq!(net.nodes[&1].keys.len(), 2);
}

#[test]
fn a_group_given_up_part_way_is_given_up_all_the_way_down() {
    let mut net = video_line();
    net.watch(3, 1);
    net.film(1, 0, true, &[b"key"], false);
    let now = net.now;
    net.khef(1).local_video_reset(cam(1), 0, now);
    net.settle();
    assert_eq!(net.nodes[&3].resets, [(cam(1), 0)]);
    // What is made of it afterwards goes nowhere, and the next group is whole.
    net.film(1, 0, false, &[b"late"], true);
    net.film(1, 1, true, &[b"key again"], true);
    let got: Vec<&[u8]> = net.frames(3).iter().map(|(_, _, frame)| *frame).collect();
    assert_eq!(got, [&b"key"[..], b"key again"]);
}

#[test]
fn when_the_plan_changes_the_way_the_pictures_come_the_new_way() {
    // A square: 4 can be reached from 1 through 2 or through 3.
    let mut net = Net::new(&[1, 2, 3, 4]);
    for (a, b) in [(1, 2), (2, 4), (1, 3), (3, 4)] {
        net.link(a, b);
    }
    net.video_tree(&[(1, 2), (2, 4), (1, 3)]);
    net.watch(4, 1);
    net.film(1, 0, true, &[b"by two"], true);
    assert_eq!(net.frames(4).len(), 1);
    let by_two = net.streams_opened[&(2, 4)];
    net.video_tree(&[(1, 3), (3, 4), (1, 2)]);
    assert_eq!(net.produce(1).last(), Some(&(cam(1), true)));
    net.film(1, 1, true, &[b"by three"], true);
    assert_eq!(net.frames(4).last(), Some(&(cam(1), 1, &b"by three"[..])));
    assert_eq!(
        net.streams_opened[&(2, 4)],
        by_two,
        "nothing more through 2"
    );
}

#[test]
fn a_neighbour_that_has_not_had_the_plan_is_asked_again() {
    let mut net = Net::new(&[1, 2]);
    net.link(1, 2);
    net.video_plan_at(2, &[(1, 2)]);
    net.watch(2, 1);
    assert_eq!(net.produce(1), [], "node 1 knows of no such track");
    net.video_plan_at(1, &[(1, 2)]);
    net.settle();
    assert_eq!(net.produce(1), []);
    assert!(net.khef(2).next_wake().is_some());
    net.run(500);
    assert_eq!(net.produce(1), [(cam(1), true)]);
    net.film(1, 0, true, &[b"key"], true);
    assert_eq!(net.frames(2), [(cam(1), 0, &b"key"[..])]);
}

#[test]
fn a_track_that_leaves_the_plan_ends_and_one_that_returns_is_watched_again() {
    let mut net = video_line();
    net.watch(3, 1);
    net.film(1, 0, true, &[b"key"], false);
    let ids: Vec<Id> = net.nodes.keys().copied().collect();
    for id in &ids {
        let now = net.now;
        net.khef(*id).set_video_plan(VideoPlan::default(), now);
    }
    net.settle();
    assert_eq!(net.produce(1), [(cam(1), true), (cam(1), false)]);
    assert_eq!(net.nodes[&3].resets, [(cam(1), 0)]);
    // The client still wants it: when the plan has it again, it comes.
    net.video_tree(LINE);
    net.run(500);
    assert_eq!(net.produce(1).last(), Some(&(cam(1), true)));
    net.film(1, 5, true, &[b"back"], true);
    assert_eq!(net.frames(3).last(), Some(&(cam(1), 5, &b"back"[..])));
}

#[test]
fn when_the_link_to_the_only_watcher_drops_the_camera_stops() {
    let mut net = video_line();
    net.watch(3, 1);
    net.film(1, 0, true, &[b"key"], false);
    let now = net.now;
    net.khef(2).link_down(&3, now);
    net.khef(3).link_down(&2, now);
    net.settle();
    assert_eq!(net.produce(1), [(cam(1), true), (cam(1), false)]);
    assert_eq!(net.nodes[&3].resets, [(cam(1), 0)]);
    // The link returns, and with it the watching.
    net.link(2, 3);
    assert_eq!(net.produce(1).last(), Some(&(cam(1), true)));
    net.film(1, 1, true, &[b"key"], true);
    assert_eq!(net.frames(3).last(), Some(&(cam(1), 1, &b"key"[..])));
}

#[test]
fn a_track_is_not_sent_back_the_way_it_comes() {
    let mut net = Net::new(&[1, 2]);
    net.link(1, 2);
    net.video_tree(&[(1, 2)]);
    // Node 1 believes its own camera comes from node 2: a plan gone wrong.
    let tracks = BTreeMap::from([(
        cam(1),
        VideoRoute {
            name: camera(1),
            from: VideoFrom::Link(2),
        },
    )]);
    let now = net.now;
    net.khef(1).set_video_plan(VideoPlan { tracks }, now);
    net.watch(2, 1);
    net.run(2000);
    assert_eq!(net.produce(1), []);
    assert_eq!(net.frames(2), []);
}
