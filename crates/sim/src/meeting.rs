//! A whole meeting in the simulator.
//!
//! Every node is a real [`palaver_node::Node`], which is the real khef and the real control
//! logic driven together, and what crosses the simulated links is what would cross real
//! ones: `MoQ` sessions on streams, and datagrams.
//!
//! The simulator supplies only what the network and the operating system would:
//!
//! - connections: a dial succeeds after a delay, or fails after a timeout if the two nodes
//!   cannot reach each other or the other is not running;
//! - streams that deliver their bytes in order after a delay, and datagrams that may
//!   overtake one another;
//! - a clock.
//!
//! Everything else is the nodes' own doing, including noticing that a neighbour has crashed
//! (its links fall silent).
//!
//! Signatures are the stand-in kind (`fake-crypto`): the simulator tests behaviour, not
//! cryptography.

use crate::{NodeId, Rng, World};
use palaver_khef_core::{Action, AudioOut, LinkWatchConfig, Role, StreamId, StreamKind};
use palaver_moq_wire::{PalaverExtensions, Setup};
use palaver_node::{Config, Counters, Node, Output};
use palaver_proto::sign::fake::{FakeIdentity, FakeVerifier};
use palaver_proto::{
    MediaTrack, MeetingTag, ParticipantId, Plan, Redacted, Signer, Source, Timestamp, Verifier,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Most nodes a simulated meeting can hold: the stand-in identities have one seed byte.
pub const MAX_NODES: NodeId = 256;
/// The length of a filler datagram.
const FILLER_BYTES: usize = 5;

/// What can reach a node. A link is named by the node at its other end and by which
/// connection between the two it is, so that bytes still on their way when a connection is
/// replaced are not taken for the new one's.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    /// The node asked to be called.
    Wake,
    /// A connection to `peer` is established. `dialled` if this end dialled it.
    LinkUp {
        peer: NodeId,
        connection: u64,
        dialled: bool,
    },
    /// The other end closed the connection.
    LinkDown { peer: NodeId, connection: u64 },
    /// A dial to `peer` gave up.
    DialFailed { peer: NodeId },
    Data {
        from: NodeId,
        connection: u64,
        stream: StreamId,
        unidirectional: bool,
        bytes: Vec<u8>,
        fin: bool,
    },
    Reset {
        from: NodeId,
        connection: u64,
        stream: StreamId,
    },
    Datagram {
        from: NodeId,
        connection: u64,
        bytes: Vec<u8>,
    },
}

struct Member {
    node: Node<FakeIdentity, FakeVerifier>,
    /// The connections this node holds, by the node at the other end.
    links: BTreeMap<NodeId, u64>,
    /// The kind of each stream this node opened and has not finished, by connection and
    /// number.
    kinds: BTreeMap<(u64, u64), StreamKind>,
    /// The connections on which this node has opened its control stream.
    control_streams: BTreeSet<u64>,
    /// Audio frames this node's client was given to play: (alias, frame).
    heard: Vec<(u64, u64)>,
    /// What the node had counted when last looked at.
    counted: Counters,
    /// The earliest wake already scheduled, so each call does not schedule another.
    wake_at: Option<Timestamp>,
}

/// Counters over a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Reports published by nodes.
    pub reports_published: u64,
    /// Control objects sent over links, each on a stream of its own.
    pub objects_sent: u64,
    /// Bytes sent on streams: control objects and the `MoQ` messages around them.
    pub bytes_sent: u64,
    /// Audio datagrams sent over links.
    pub audio_sent: u64,
    /// Fillers sent on links with nothing else to carry.
    pub fillers_sent: u64,
    /// Control objects the receiver already had or would not take.
    pub objects_dropped: u64,
    /// Plans issued by conductors.
    pub plans_issued: u64,
    /// Connections established.
    pub links_made: u64,
    /// Dials that failed.
    pub dials_failed: u64,
    /// `MoQ` sessions closed for a protocol error. Always a bug somewhere.
    pub sessions_closed: u64,
}

/// A simulated meeting.
pub struct Meeting {
    world: World<Event>,
    jitter: Rng,
    tag: MeetingTag,
    nodes: BTreeMap<NodeId, Member>,
    /// Every node that ever joined, by participant id.
    by_id: BTreeMap<ParticipantId, NodeId>,
    /// Pairs that cannot connect, lower node first.
    blocked: BTreeSet<(NodeId, NodeId)>,
    connections: u64,
    /// When the last bytes sent on each stream will arrive, so that later ones do not
    /// arrive first: by sender, connection and the sender's name for the stream.
    stream_clock: BTreeMap<(NodeId, u64, StreamId), Timestamp>,
    stats: Stats,
    /// One-way delay of every link.
    pub link_delay: Duration,
    /// Extra random delay, up to this much. Lets datagrams, and bytes on different streams,
    /// overtake each other.
    pub link_jitter: Duration,
    /// How long a successful dial takes.
    pub dial_time: Duration,
    /// How long a dial to an unreachable node takes to give up.
    pub dial_timeout: Duration,
    /// How each node watches its links: how often an idle link carries a filler, and how
    /// long a silence means the neighbour is gone.
    pub link_watch: LinkWatchConfig,
}

fn ordered(a: NodeId, b: NodeId) -> (NodeId, NodeId) {
    (a.min(b), a.max(b))
}

/// A stream as the other end of the connection names it.
const fn flipped(stream: StreamId) -> StreamId {
    match stream {
        StreamId::Ours(n) => StreamId::Theirs(n),
        StreamId::Theirs(n) => StreamId::Ours(n),
    }
}

impl Meeting {
    #[must_use]
    pub fn new(seed: u64) -> Option<Self> {
        let world = World::new(seed);
        let jitter = world.rng().derive(u64::MAX);
        Some(Self {
            world,
            jitter,
            tag: MeetingTag::new(b"sim-meeting")?,
            nodes: BTreeMap::new(),
            by_id: BTreeMap::new(),
            blocked: BTreeSet::new(),
            connections: 0,
            stream_clock: BTreeMap::new(),
            stats: Stats::default(),
            link_delay: Duration::from_millis(5),
            link_jitter: Duration::from_millis(2),
            dial_time: Duration::from_millis(30),
            dial_timeout: Duration::from_millis(500),
            link_watch: LinkWatchConfig::default(),
        })
    }

    #[must_use]
    pub const fn now(&self) -> Timestamp {
        self.world.now()
    }

    /// Fingerprint of everything that has happened; equal for equal seeds and inputs.
    #[must_use]
    pub const fn trace(&self) -> u64 {
        self.world.trace()
    }

    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    /// The participant id node `node` has, or would have.
    #[must_use]
    pub fn id_of(node: NodeId) -> ParticipantId {
        FakeVerifier.participant_id(&Self::identity(node).public_key())
    }

    /// The stand-in identity of node `node`.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "nodes that can join are below MAX_NODES (256); a higher number has no node, and wraps as it always has"
    )]
    const fn identity(node: NodeId) -> FakeIdentity {
        FakeIdentity(node as u8)
    }

    /// The microphone track every simulated node offers.
    #[must_use]
    pub fn microphone() -> MediaTrack {
        MediaTrack {
            audio: true,
            source: Source::Main,
            rendition: b"opus".to_vec(),
        }
    }

    fn add(&mut self, node: NodeId) -> bool {
        if node >= MAX_NODES || self.by_id.values().any(|n| *n == node) {
            return false;
        }
        let config = Config {
            meeting: self.tag.clone(),
            name: Redacted::new(format!("node {node}")),
            first_sequence: 1,
            setup: Setup {
                palaver: Some(PalaverExtensions { version: 1 }),
                ..Setup::default()
            },
            link_watch: self.link_watch,
            tracks: vec![Self::microphone()],
            addresses: Vec::new(),
        };
        self.by_id.insert(Self::id_of(node), node);
        self.nodes.insert(
            node,
            Member {
                node: Node::new(config, Self::identity(node), FakeVerifier),
                links: BTreeMap::new(),
                kinds: BTreeMap::new(),
                control_streams: BTreeSet::new(),
                heard: Vec::new(),
                counted: Counters::default(),
                wake_at: None,
            },
        );
        true
    }

    /// Node `node` creates the meeting now and is its conductor. `false` if the node number
    /// was used before.
    pub fn create(&mut self, node: NodeId) -> bool {
        if !self.add(node) {
            return false;
        }
        let now = self.world.now();
        if let Some(member) = self.nodes.get_mut(&node) {
            member.node.create_meeting(now);
        }
        self.carry_out(node);
        true
    }

    /// Node `node` joins now by dialling `entry`. `false` if the node number was used before.
    pub fn join(&mut self, node: NodeId, entry: NodeId) -> bool {
        if !self.add(node) {
            return false;
        }
        let now = self.world.now();
        if let Some(member) = self.nodes.get_mut(&node) {
            member.node.join(vec![Self::id_of(entry)], now);
        }
        self.carry_out(node);
        true
    }

    /// From now on `a` and `b` cannot connect to each other. A link they already hold stays.
    pub fn block(&mut self, a: NodeId, b: NodeId) {
        self.blocked.insert(ordered(a, b));
    }

    /// Node `node` vanishes now, without a word. Its neighbours notice when its links fall
    /// silent.
    pub fn crash(&mut self, node: NodeId) {
        self.nodes.remove(&node);
    }

    /// Node `node`'s microphone produces frame `frame` at loudness `level`. `false` if the
    /// plan has not given its microphone an alias yet.
    pub fn speak(&mut self, node: NodeId, frame: u64, level: u8) -> bool {
        let now = self.world.now();
        let Some(member) = self.nodes.get_mut(&node) else {
            return false;
        };
        let mut datagram = Vec::new();
        let out = member
            .node
            .local_audio(frame, level, &[0x5a; 80], &mut datagram, now);
        let spoke = out.is_some();
        self.carry_audio(node, out, &datagram);
        self.carry_out(node);
        spoke
    }

    /// Run until the clock reads `until`.
    pub fn run_until(&mut self, until: Timestamp) {
        while self.world.next_at().is_some_and(|at| at <= until) {
            if let Some((node, event)) = self.world.step() {
                self.handle(node, event);
            }
        }
        self.world.advance_to(until);
    }

    /// Run for `span` more.
    pub fn run_for(&mut self, span: Duration) {
        self.run_until(self.world.now().saturating_add(span));
    }

    // --- What the tests look at ---

    /// The nodes that are running, in order.
    #[must_use]
    pub fn live(&self) -> Vec<NodeId> {
        self.nodes.keys().copied().collect()
    }

    #[must_use]
    pub fn plan_of(&self, node: NodeId) -> Option<&Plan> {
        self.nodes.get(&node)?.node.plan()
    }

    /// The nodes `node` holds a connection to, in order.
    #[must_use]
    pub fn links_of(&self, node: NodeId) -> Vec<NodeId> {
        self.nodes
            .get(&node)
            .map(|member| member.links.keys().copied().collect())
            .unwrap_or_default()
    }

    /// The audio frames `node`'s client was given to play, in order: (alias, frame).
    #[must_use]
    pub fn heard_by(&self, node: NodeId) -> &[(u64, u64)] {
        self.nodes
            .get(&node)
            .map(|member| member.heard.as_slice())
            .unwrap_or_default()
    }

    /// The alias the plan `node` follows gives `speaker`'s microphone.
    #[must_use]
    pub fn microphone_alias(&self, node: NodeId, speaker: NodeId) -> Option<u64> {
        self.plan_of(node)?
            .alias_of(&Self::microphone().of(Self::id_of(speaker)), &FakeVerifier)
    }

    /// The node with this participant id, if it ever joined.
    #[must_use]
    pub fn node_of(&self, id: &ParticipantId) -> Option<NodeId> {
        self.by_id.get(id).copied()
    }

    /// Whether the meeting has settled: every running node follows the same plan, whose
    /// roster is exactly the running nodes; the connections held are exactly the plan's
    /// peerings; and the audio overlay is one tree over everyone.
    ///
    /// # Errors
    ///
    /// When the meeting has not settled. The message says what is not so.
    pub fn settled(&self) -> Result<&Plan, String> {
        let mut plans = self.nodes.iter().map(|(node, member)| {
            member
                .node
                .plan()
                .ok_or_else(|| format!("node {node} has no plan"))
        });
        let plan = plans.next().ok_or("nobody is running")??;
        for other in plans {
            let other = other?;
            if other != plan {
                return Err(format!(
                    "plans differ: {:?} and {:?}",
                    plan.stamp, other.stamp
                ));
            }
        }
        let node_of = |id: &ParticipantId| {
            self.node_of(id)
                .filter(|node| self.nodes.contains_key(node))
                .ok_or_else(|| format!("the plan names {id:?}, which is not running"))
        };
        let mut roster = BTreeSet::new();
        for member in &plan.roster {
            roster.insert(node_of(&FakeVerifier.participant_id(&member.key))?);
        }
        let live: BTreeSet<NodeId> = self.nodes.keys().copied().collect();
        if roster != live {
            return Err(format!("roster is {roster:?} but {live:?} are running"));
        }

        let mut planned: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
        for edge in &plan.peerings {
            let (a, b) = (node_of(&edge.0)?, node_of(&edge.1)?);
            planned.entry(a).or_default().insert(b);
            planned.entry(b).or_default().insert(a);
        }
        for (node, member) in &self.nodes {
            let want = planned.remove(node).unwrap_or_default();
            let have: BTreeSet<NodeId> = member.links.keys().copied().collect();
            if have != want {
                return Err(format!(
                    "node {node} holds links {have:?}, the plan says {want:?}"
                ));
            }
            // What the simulator connected and what the node believes must agree.
            let believed: BTreeSet<NodeId> = member
                .node
                .links()
                .filter_map(|id| self.node_of(id))
                .collect();
            if believed != have {
                return Err(format!(
                    "node {node} believes it holds {believed:?}, but holds {have:?}"
                ));
            }
        }

        let mut neighbours: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for edge in &plan.audio_overlay {
            let (a, b) = (node_of(&edge.0)?, node_of(&edge.1)?);
            neighbours.entry(a).or_default().push(b);
            neighbours.entry(b).or_default().push(a);
        }
        if plan.audio_overlay.len().saturating_add(1) != live.len() {
            return Err(format!(
                "{} audio links for {} nodes is not a tree",
                plan.audio_overlay.len(),
                live.len()
            ));
        }
        let mut reached = BTreeSet::new();
        let mut todo: Vec<NodeId> = live.first().copied().into_iter().collect();
        while let Some(node) = todo.pop() {
            if reached.insert(node) {
                todo.extend(neighbours.get(&node).into_iter().flatten());
            }
        }
        if reached != live {
            return Err(format!("the audio overlay reaches only {reached:?}"));
        }
        Ok(plan)
    }

    // --- The network ---

    fn handle(&mut self, at_node: NodeId, event: Event) {
        let now = self.world.now();
        let rtt = self.link_delay.saturating_mul(2);
        let Some(member) = self.nodes.get_mut(&at_node) else {
            return; // it crashed while this was on its way
        };
        // Is this from the connection the node holds to that peer now?
        let current = |member: &Member, peer: NodeId, connection: u64| {
            member.links.get(&peer) == Some(&connection)
        };
        match event {
            Event::Wake => {
                member.wake_at = None;
                member.node.poll(now);
            }
            Event::LinkUp {
                peer,
                connection,
                dialled,
            } => {
                // If both ends dialled at once, the first connection stands.
                if let std::collections::btree_map::Entry::Vacant(link) = member.links.entry(peer) {
                    link.insert(connection);
                    if dialled {
                        self.stats.links_made = self.stats.links_made.saturating_add(1);
                    }
                    let role = if dialled { Role::Client } else { Role::Server };
                    member.node.link_up(Self::id_of(peer), role, rtt, None, now);
                }
            }
            Event::LinkDown { peer, connection } => {
                if current(member, peer, connection) {
                    member.links.remove(&peer);
                    member.node.link_down(&Self::id_of(peer), now);
                }
            }
            Event::DialFailed { peer } => {
                self.stats.dials_failed = self.stats.dials_failed.saturating_add(1);
                member.node.dial_failed(&Self::id_of(peer), now);
            }
            Event::Data {
                from,
                connection,
                stream,
                unidirectional,
                bytes,
                fin,
            } => {
                if current(member, from, connection) {
                    let kind = if unidirectional {
                        StreamKind::Uni
                    } else {
                        StreamKind::Bidi
                    };
                    member
                        .node
                        .on_data(&Self::id_of(from), stream, kind, &bytes, fin, now);
                }
            }
            Event::Reset {
                from,
                connection,
                stream,
            } => {
                if current(member, from, connection) {
                    member.node.on_reset(&Self::id_of(from), stream, now);
                }
            }
            Event::Datagram {
                from,
                connection,
                bytes,
            } => {
                if current(member, from, connection) {
                    let out = member.node.on_datagram(&Self::id_of(from), &bytes, now);
                    self.carry_audio(at_node, out, &bytes);
                }
            }
        }
        self.carry_out(at_node);
    }

    /// A delay for something sent now.
    fn delay(&mut self) -> Duration {
        let jitter = u64::try_from(self.link_jitter.as_micros()).unwrap_or(u64::MAX);
        self.link_delay
            .saturating_add(Duration::from_micros(self.jitter.below(jitter)))
    }

    /// Do what a node said to do with an audio datagram.
    fn carry_audio(&mut self, at_node: NodeId, out: Option<AudioOut<ParticipantId>>, bytes: &[u8]) {
        let Some(out) = out else {
            return;
        };
        if out.for_client
            && let Some(member) = self.nodes.get_mut(&at_node)
        {
            member.heard.push((out.alias, out.group));
        }
        for peer in out.forward_to {
            self.send_datagram(at_node, &peer, bytes.to_vec());
        }
    }

    fn send_datagram(&mut self, from: NodeId, to: &ParticipantId, bytes: Vec<u8>) {
        let Some(to) = self.node_of(to) else {
            return;
        };
        let Some(connection) = self
            .nodes
            .get(&from)
            .and_then(|m| m.links.get(&to))
            .copied()
        else {
            return;
        };
        if bytes.len() == FILLER_BYTES {
            self.stats.fillers_sent = self.stats.fillers_sent.saturating_add(1);
        } else {
            self.stats.audio_sent = self.stats.audio_sent.saturating_add(1);
        }
        let delay = self.delay();
        self.world.schedule(
            delay,
            to,
            Event::Datagram {
                from,
                connection,
                bytes,
            },
        );
    }

    /// Start connecting `node` to `peer`.
    fn dial(&mut self, node: NodeId, peer: NodeId) {
        let reachable =
            self.nodes.contains_key(&peer) && !self.blocked.contains(&ordered(node, peer));
        if !reachable {
            self.world
                .schedule(self.dial_timeout, node, Event::DialFailed { peer });
            return;
        }
        self.connections = self.connections.saturating_add(1);
        let connection = self.connections;
        for (end, other, dialled) in [(node, peer, true), (peer, node, false)] {
            self.world.schedule(
                self.dial_time,
                end,
                Event::LinkUp {
                    peer: other,
                    connection,
                    dialled,
                },
            );
        }
    }

    /// Carry out everything `node` has asked for, and book its next wake.
    fn carry_out(&mut self, node: NodeId) {
        let now = self.world.now();
        loop {
            let Some(member) = self.nodes.get_mut(&node) else {
                return;
            };
            let Some(output) = member.node.poll_output() else {
                break;
            };
            match output {
                Output::Dial { peer } => match self.by_id.get(&peer).copied() {
                    Some(peer) => self.dial(node, peer),
                    // A node the simulator has never heard of cannot be reached.
                    None => member.node.dial_failed(&peer, now),
                },
                Output::Hangup { peer } => {
                    let Some(peer) = self.by_id.get(&peer).copied() else {
                        continue;
                    };
                    if let Some(connection) = member.links.remove(&peer) {
                        let delay = self.link_delay;
                        let event = Event::LinkDown {
                            peer: node,
                            connection,
                        };
                        self.world.schedule(delay, peer, event);
                    }
                }
                Output::Datagram { peer, bytes } => self.send_datagram(node, &peer, bytes),
                Output::Stream { peer, action } => {
                    let Some(peer) = self.by_id.get(&peer).copied() else {
                        continue;
                    };
                    let Some(connection) = member.links.get(&peer).copied() else {
                        continue;
                    };
                    self.carry_stream(node, peer, connection, action);
                }
            }
        }

        let Some(member) = self.nodes.get_mut(&node) else {
            return;
        };
        let counted = member.node.counters();
        let before = core::mem::replace(&mut member.counted, counted);
        let add = |total: &mut u64, now: u64, before: u64| {
            *total = total.saturating_add(now.saturating_sub(before));
        };
        add(
            &mut self.stats.reports_published,
            counted.reports_published,
            before.reports_published,
        );
        add(
            &mut self.stats.plans_issued,
            counted.plans_issued,
            before.plans_issued,
        );
        add(
            &mut self.stats.objects_dropped,
            counted.control_refused,
            before.control_refused,
        );
        if counted.plans_issued != before.plans_issued {
            self.world.note(counted.plans_issued);
        }

        // Never book a wake for the present moment: that would spin without the clock moving.
        let Some(wake) = member.node.next_wake(now) else {
            return;
        };
        let wake = wake.max(now.saturating_add(Duration::from_millis(1)));
        if member.wake_at.is_none_or(|booked| wake < booked) {
            member.wake_at = Some(wake);
            self.world
                .schedule(wake.saturating_since(now), node, Event::Wake);
        }
    }

    /// Carry out one thing a node's khef wants done to a connection.
    fn carry_stream(&mut self, node: NodeId, peer: NodeId, connection: u64, action: Action) {
        let now = self.world.now();
        match action {
            Action::Open { stream, kind } => {
                if let (StreamId::Ours(n), Some(member)) = (stream, self.nodes.get_mut(&node)) {
                    // The first unidirectional stream of a connection is the control stream;
                    // each one after it carries one control object.
                    if kind == StreamKind::Uni && !member.control_streams.insert(connection) {
                        self.stats.objects_sent = self.stats.objects_sent.saturating_add(1);
                    }
                    member.kinds.insert((connection, n), kind);
                }
            }
            Action::Send { stream, bytes, fin } => {
                let unidirectional = match (stream, self.nodes.get_mut(&node)) {
                    (StreamId::Ours(n), Some(member)) => {
                        let kind = member.kinds.get(&(connection, n)).copied();
                        // A finished stream needs no more remembering.
                        if fin {
                            member.kinds.remove(&(connection, n));
                        }
                        kind == Some(StreamKind::Uni)
                    }
                    // A stream the peer opened that we can write on is bidirectional.
                    _ => false,
                };
                self.stats.bytes_sent = self.stats.bytes_sent.saturating_add(bytes.len() as u64);
                // Bytes on one stream arrive in the order they were sent.
                let key = (node, connection, stream);
                let earliest = self.stream_clock.get(&key).copied().unwrap_or(now);
                let arrival = now.saturating_add(self.delay()).max(earliest);
                if fin {
                    self.stream_clock.remove(&key);
                } else {
                    self.stream_clock.insert(key, arrival);
                }
                self.world.schedule(
                    arrival.saturating_since(now),
                    peer,
                    Event::Data {
                        from: node,
                        connection,
                        stream: flipped(stream),
                        unidirectional,
                        bytes,
                        fin,
                    },
                );
            }
            Action::Reset { stream } => {
                let delay = self.delay();
                self.world.schedule(
                    delay,
                    peer,
                    Event::Reset {
                        from: node,
                        connection,
                        stream: flipped(stream),
                    },
                );
            }
            // A session that closes itself has met a protocol error. The connection goes
            // down at both ends; the tests check that this never happens.
            Action::Close { .. } => {
                self.stats.sessions_closed = self.stats.sessions_closed.saturating_add(1);
                for (end, other) in [(node, peer), (peer, node)] {
                    self.world.schedule(
                        Duration::ZERO,
                        end,
                        Event::LinkDown {
                            peer: other,
                            connection,
                        },
                    );
                }
            }
        }
    }
}
