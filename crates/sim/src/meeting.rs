//! A whole meeting's control plane in the simulator: every node runs the real
//! [`palaver_control::Node`] and [`palaver_khef_core::Flood`], joined by simulated links.
//!
//! What stands in for the parts not built yet (`net`, `sdk`) is deliberately thin and follows
//! the design's start-up rules:
//!
//! - a joiner dials one entry node; a link that comes up gives each end the other's latest
//!   control objects;
//! - an object is checked by the receiving node's control logic and flooded only if accepted;
//! - a joiner's control logic starts when its entry link is up, so its first report names
//!   that link;
//! - when a node adopts a plan it dials the planned peers it has no link to (the lower
//!   participant id dials);
//! - a link the plan does not name is closed only once both ends are wired as the plan says,
//!   which each end learns from the other's report;
//! - a node left with no link at all dials the members of the last plan it saw until one
//!   answers;
//! - a dial to a node it cannot reach fails after a timeout and is reported as unreachable;
//! - every node watches its links with the real [`LinkWatch`]: an idle link carries a filler
//!   every 200 ms, and a neighbour that crashes is noticed when its links fall silent.
//!
//! Signatures are the stand-in kind (`fake-crypto`): the simulator tests behaviour, not
//! cryptography.

use crate::{NodeId, Rng, World};
use palaver_control::{
    Config, LocalView, Node, Output, PlanVerdict, REPORT_TIMEOUT, ReportVerdict,
};
use palaver_khef_core::{Flood, LinkWatch, LinkWatchConfig, Offer};
use palaver_proto::sign::fake::{FakeIdentity, FakeVerifier};
use palaver_proto::{
    Link, MeetingTag, ParticipantId, PathKind, Plan, Redacted, Signer, Timestamp, Track, Verifier,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Most nodes a simulated meeting can hold: the stand-in identities have one seed byte.
pub const MAX_NODES: NodeId = 256;
/// Control tracks a simulated khef remembers.
const MAX_CONTROL_TRACKS: usize = 1024;

/// How long a khef keeps the latest object of a control track.
fn lifetime(track: &Track) -> Option<Duration> {
    match track {
        Track::Report { .. } => Some(REPORT_TIMEOUT),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    /// The node's control logic asked to be called.
    Wake,
    /// A control object arriving from a neighbour.
    Object {
        from: NodeId,
        track: Track,
        group: u64,
        payload: Vec<u8>,
    },
    /// A connection to `peer` is established.
    LinkUp { peer: NodeId },
    /// The connection to `peer` is gone.
    LinkDown { peer: NodeId },
    /// A dial to `peer` gave up.
    DialFailed { peer: NodeId },
    /// A filler from a neighbour with nothing else to send.
    Filler { from: NodeId },
}

struct Member {
    id: ParticipantId,
    control: Node<FakeIdentity, FakeVerifier>,
    flood: Flood<Track, NodeId>,
    links: BTreeSet<NodeId>,
    /// One per link: is the other end still sending?
    watches: BTreeMap<NodeId, LinkWatch>,
    dialing: BTreeSet<NodeId>,
    unreachable: BTreeSet<NodeId>,
    /// The node holds a link the plan does not name, kept until the other end is wired.
    surplus_links: bool,
    /// The earliest wake already scheduled, so each poll does not schedule another.
    wake_at: Option<Timestamp>,
}

/// Counters over a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Reports published by nodes.
    pub reports_published: u64,
    /// Control objects sent over links.
    pub objects_sent: u64,
    /// Their size in all, in bytes, without transport overhead.
    pub bytes_sent: u64,
    /// Fillers sent on links with nothing else to carry.
    pub fillers_sent: u64,
    /// Of those, how many the receiver already had or would not take.
    pub objects_dropped: u64,
    /// Plans issued by conductors.
    pub plans_issued: u64,
    /// Connections established.
    pub links_made: u64,
    /// Dials that failed.
    pub dials_failed: u64,
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
    stats: Stats,
    /// One-way delay of every link.
    pub link_delay: Duration,
    /// Extra random delay per object, up to this much; lets objects overtake each other.
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

impl Meeting {
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
            stats: Stats::default(),
            link_delay: Duration::from_millis(5),
            link_jitter: Duration::from_millis(2),
            dial_time: Duration::from_millis(30),
            dial_timeout: Duration::from_millis(500),
            link_watch: LinkWatchConfig::default(),
        })
    }

    pub fn now(&self) -> Timestamp {
        self.world.now()
    }

    /// Fingerprint of everything that has happened; equal for equal seeds and inputs.
    pub fn trace(&self) -> u64 {
        self.world.trace()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The participant id node `node` has, or would have.
    pub fn id_of(node: NodeId) -> ParticipantId {
        FakeVerifier.participant_id(&FakeIdentity(node as u8).public_key())
    }

    fn add(&mut self, node: NodeId) -> bool {
        if node >= MAX_NODES || self.by_id.values().any(|n| *n == node) {
            return false;
        }
        let config = Config {
            meeting: self.tag.clone(),
            name: Redacted::new(format!("node {node}")),
            first_sequence: 1,
        };
        let id = Self::id_of(node);
        self.by_id.insert(id.clone(), node);
        self.nodes.insert(
            node,
            Member {
                id,
                control: Node::new(config, FakeIdentity(node as u8), FakeVerifier),
                flood: Flood::new(lifetime, MAX_CONTROL_TRACKS),
                links: BTreeSet::new(),
                watches: BTreeMap::new(),
                dialing: BTreeSet::new(),
                unreachable: BTreeSet::new(),
                surplus_links: false,
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
        if let Some(member) = self.nodes.get_mut(&node) {
            member.control.create_meeting();
        }
        self.world.schedule(Duration::ZERO, node, Event::Wake);
        true
    }

    /// Node `node` joins now by dialling `entry`. `false` if the node number was used before.
    pub fn join(&mut self, node: NodeId, entry: NodeId) -> bool {
        if !self.add(node) {
            return false;
        }
        // Its control logic first runs when the entry link comes up, so that its first
        // report already names that link and the conductor can wire it by it.
        self.dial(node, entry);
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
    pub fn live(&self) -> Vec<NodeId> {
        self.nodes.keys().copied().collect()
    }

    pub fn plan_of(&self, node: NodeId) -> Option<&Plan> {
        self.nodes.get(&node)?.control.plan()
    }

    /// The nodes `node` holds a connection to, in order.
    pub fn links_of(&self, node: NodeId) -> Vec<NodeId> {
        self.nodes
            .get(&node)
            .map(|member| member.links.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The node with this participant id, if it ever joined.
    pub fn node_of(&self, id: &ParticipantId) -> Option<NodeId> {
        self.by_id.get(id).copied()
    }

    /// Whether the meeting has settled: every running node follows the same plan, whose
    /// roster is exactly the running nodes; the connections held are exactly the plan's
    /// peerings; and the audio overlay is one tree over everyone. `Err` says what is not so.
    pub fn settled(&self) -> Result<&Plan, String> {
        let mut plans = self.nodes.iter().map(|(node, member)| {
            member
                .control
                .plan()
                .ok_or(format!("node {node} has no plan"))
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
                .ok_or(format!("the plan names {id:?}, which is not running"))
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
            if member.links != want {
                return Err(format!(
                    "node {node} holds links {:?}, the plan says {want:?}",
                    member.links
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

    // --- The stand-in for the parts not built yet ---

    fn handle(&mut self, node: NodeId, event: Event) {
        if !self.nodes.contains_key(&node) {
            return; // it crashed while this was on its way
        }
        match event {
            Event::Wake => {
                if let Some(member) = self.nodes.get_mut(&node) {
                    member.wake_at = None;
                }
            }
            Event::Object {
                from,
                track,
                group,
                payload,
            } => self.on_object(node, from, track, group, &payload),
            Event::LinkUp { peer } => self.on_link_up(node, peer),
            Event::LinkDown { peer } => self.on_link_down(node, peer),
            Event::DialFailed { peer } => {
                self.stats.dials_failed = self.stats.dials_failed.saturating_add(1);
                if let Some(member) = self.nodes.get_mut(&node) {
                    member.dialing.remove(&peer);
                    member.unreachable.insert(peer);
                }
                self.refresh_view(node);
                self.rejoin_if_alone(node);
            }
            Event::Filler { from } => self.heard(node, from),
        }
        self.service(node);
    }

    /// Send one object from `node` to `to`.
    fn send(&mut self, node: NodeId, to: NodeId, track: &Track, group: u64, payload: &[u8]) {
        let jitter = u64::try_from(self.link_jitter.as_micros()).unwrap_or(u64::MAX);
        let delay = self
            .link_delay
            .saturating_add(Duration::from_micros(self.jitter.below(jitter)));
        self.stats.objects_sent = self.stats.objects_sent.saturating_add(1);
        self.stats.bytes_sent = self.stats.bytes_sent.saturating_add(payload.len() as u64);
        let now = self.world.now();
        if let Some(watch) = self
            .nodes
            .get_mut(&node)
            .and_then(|member| member.watches.get_mut(&to))
        {
            watch.sent(now);
        }
        self.world.schedule(
            delay,
            to,
            Event::Object {
                from: node,
                track: track.clone(),
                group,
                payload: payload.to_vec(),
            },
        );
    }

    /// Hand an accepted object to `node`'s flood and send it where the flood says.
    fn flood(
        &mut self,
        node: NodeId,
        from: Option<NodeId>,
        track: &Track,
        group: u64,
        payload: &[u8],
    ) {
        let now = self.world.now();
        let Some(member) = self.nodes.get_mut(&node) else {
            return;
        };
        let offer = member
            .flood
            .offer(from.as_ref(), track, group, payload, now);
        if let Offer::Fresh { forward_to } = offer {
            for to in forward_to {
                self.send(node, to, track, group, payload);
            }
        }
    }

    fn on_object(&mut self, node: NodeId, from: NodeId, track: Track, group: u64, payload: &[u8]) {
        let now = self.world.now();
        let Some(member) = self.nodes.get_mut(&node) else {
            return;
        };
        if !member.links.contains(&from) {
            return; // the link went down while this was on its way
        }
        if let Some(watch) = member.watches.get_mut(&from) {
            watch.heard(now);
        }
        let accepted = match &track {
            Track::Plan => member.control.on_plan(group, payload) == PlanVerdict::Adopted,
            Track::Report { participant } => {
                member.control.on_report(participant, group, payload, now)
                    == ReportVerdict::Accepted
            }
            _ => false,
        };
        self.world.note(group ^ u64::from(accepted));
        if !accepted {
            self.stats.objects_dropped = self.stats.objects_dropped.saturating_add(1);
            return;
        }
        self.flood(node, Some(from), &track, group, payload);
        // A new plan changes what the node should be connected to; a report may show that a
        // neighbour is now wired and an unplanned link to it can go.
        let surplus = self.nodes.get(&node).is_some_and(|m| m.surplus_links);
        if track == Track::Plan || surplus {
            self.apply_plan(node);
        }
    }

    fn on_link_up(&mut self, node: NodeId, peer: NodeId) {
        let now = self.world.now();
        let Some(member) = self.nodes.get_mut(&node) else {
            return;
        };
        member.dialing.remove(&peer);
        if !member.links.insert(peer) {
            return;
        }
        member
            .watches
            .insert(peer, LinkWatch::new(self.link_watch, now));
        // Counted once per link, by the end with the lower number.
        if node < peer {
            self.stats.links_made = self.stats.links_made.saturating_add(1);
        }
        for cached in member.flood.connect(peer, now) {
            self.send(node, peer, &cached.track, cached.group, &cached.payload);
        }
        self.refresh_view(node);
        self.apply_plan(node);
    }

    fn on_link_down(&mut self, node: NodeId, peer: NodeId) {
        let Some(member) = self.nodes.get_mut(&node) else {
            return;
        };
        if !member.links.remove(&peer) {
            return;
        }
        member.watches.remove(&peer);
        member.flood.disconnect(&peer);
        self.refresh_view(node);
        // If the plan still names the peer, try to get it back.
        self.apply_plan(node);
        self.rejoin_if_alone(node);
    }

    /// A node left with no connection at all dials the members of the last plan it saw, one
    /// after another, until one answers. Nobody else can do it: the others cannot tell a
    /// node that was cut off from one that left.
    fn rejoin_if_alone(&mut self, node: NodeId) {
        let Some(member) = self.nodes.get(&node) else {
            return;
        };
        if !member.links.is_empty() || !member.dialing.is_empty() {
            return;
        }
        let Some(plan) = member.control.plan() else {
            return;
        };
        let next = plan
            .roster
            .iter()
            .filter_map(|m| self.node_of(&FakeVerifier.participant_id(&m.key)))
            .find(|peer| *peer != node && !member.unreachable.contains(peer));
        if let Some(peer) = next {
            self.dial(node, peer);
        }
    }

    /// Start connecting `node` to `peer`.
    fn dial(&mut self, node: NodeId, peer: NodeId) {
        let reachable =
            self.nodes.contains_key(&peer) && !self.blocked.contains(&ordered(node, peer));
        let Some(member) = self.nodes.get_mut(&node) else {
            return;
        };
        if !member.dialing.insert(peer) {
            return;
        }
        if reachable {
            self.world
                .schedule(self.dial_time, node, Event::LinkUp { peer });
            self.world
                .schedule(self.dial_time, peer, Event::LinkUp { peer: node });
        } else {
            self.world
                .schedule(self.dial_timeout, node, Event::DialFailed { peer });
        }
    }

    /// Tell `node`'s control logic what its links are now.
    fn refresh_view(&mut self, node: NodeId) {
        let rtt = self.link_delay.saturating_mul(2);
        let Some(member) = self.nodes.get_mut(&node) else {
            return;
        };
        let view = LocalView {
            links: member
                .links
                .iter()
                .map(|peer| Link {
                    peer: Self::id_of(*peer),
                    path: PathKind::Direct,
                    rtt,
                })
                .collect(),
            unreachable: member.unreachable.iter().map(|p| Self::id_of(*p)).collect(),
            ..LocalView::default()
        };
        member.control.set_view(view);
    }

    /// Bring `node`'s connections into line with the plan it holds.
    fn apply_plan(&mut self, node: NodeId) {
        let Some(member) = self.nodes.get(&node) else {
            return;
        };
        let Some(plan) = member.control.plan() else {
            return;
        };
        let me = &member.id;
        let planned: BTreeSet<(NodeId, bool)> = plan
            .peerings
            .iter()
            .filter_map(|edge| {
                let other = if edge.0 == *me {
                    &edge.1
                } else if edge.1 == *me {
                    &edge.0
                } else {
                    return None;
                };
                // The lower participant id dials.
                Some((self.node_of(other)?, me < other))
            })
            .collect();

        let to_dial: Vec<NodeId> = planned
            .iter()
            .filter(|(peer, i_dial)| {
                *i_dial && !member.links.contains(peer) && !member.unreachable.contains(peer)
            })
            .map(|(peer, _)| *peer)
            .collect();
        let all_up = planned.iter().all(|(peer, _)| member.links.contains(peer));
        // A link the plan does not name goes only once both ends are wired as the plan says:
        // until then it may be the other end's only way to receive the plan, or to be heard
        // of at all if the plan does not know it yet. A node knows how the other end stands
        // from that node's report.
        let roster: BTreeSet<NodeId> = plan
            .roster
            .iter()
            .filter_map(|m| self.node_of(&FakeVerifier.participant_id(&m.key)))
            .collect();
        let to_close: Vec<NodeId> = if roster.contains(&node) && all_up {
            member
                .links
                .iter()
                .filter(|peer| !planned.iter().any(|(p, _)| p == *peer))
                .filter(|peer| {
                    let id = Self::id_of(**peer);
                    roster.contains(*peer)
                        && member.control.report_of(&id).is_some_and(|report| {
                            // Not "has applied this plan": a node the plan asks nothing new
                            // of says so only in its next regular report. A node holding
                            // all its planned links receives plans over them.
                            plan.peerings.iter().all(|edge| {
                                let other = if edge.0 == id {
                                    &edge.1
                                } else if edge.1 == id {
                                    &edge.0
                                } else {
                                    return true;
                                };
                                report.links.iter().any(|link| link.peer == *other)
                            })
                        })
                })
                .copied()
                .collect()
        } else {
            Vec::new()
        };

        let surplus = member
            .links
            .iter()
            .any(|peer| !planned.iter().any(|(p, _)| p == peer));
        if let Some(member) = self.nodes.get_mut(&node) {
            member.surplus_links = surplus;
        }
        for peer in to_dial {
            self.dial(node, peer);
        }
        for peer in to_close {
            self.world
                .schedule(Duration::ZERO, node, Event::LinkDown { peer });
            self.world
                .schedule(self.link_delay, peer, Event::LinkDown { peer: node });
        }
    }

    /// Something `from` sent reached `node`.
    fn heard(&mut self, node: NodeId, from: NodeId) {
        let now = self.world.now();
        if let Some(watch) = self
            .nodes
            .get_mut(&node)
            .and_then(|member| member.watches.get_mut(&from))
        {
            watch.heard(now);
        }
    }

    /// Look at `node`'s links: drop the ones that have fallen silent, and send a filler on
    /// the ones that have carried nothing lately.
    fn watch_links(&mut self, node: NodeId) {
        let now = self.world.now();
        let Some(member) = self.nodes.get_mut(&node) else {
            return;
        };
        let dead: Vec<NodeId> = member
            .watches
            .iter()
            .filter(|(_, watch)| watch.is_dead(now))
            .map(|(peer, _)| *peer)
            .collect();
        let mut idle = Vec::new();
        for (peer, watch) in &mut member.watches {
            if !dead.contains(peer) && watch.filler_due(now) {
                watch.sent(now);
                idle.push(*peer);
            }
        }
        for peer in idle {
            self.stats.fillers_sent = self.stats.fillers_sent.saturating_add(1);
            self.world
                .schedule(self.link_delay, peer, Event::Filler { from: node });
        }
        for peer in dead {
            self.on_link_down(node, peer);
        }
    }

    /// Let `node` watch its links and its control logic send what it wants to, and book its
    /// next wake.
    fn service(&mut self, node: NodeId) {
        self.watch_links(node);
        let now = self.world.now();
        loop {
            let Some(member) = self.nodes.get_mut(&node) else {
                return;
            };
            let me = member.id.clone();
            match member.control.poll(now) {
                Some(Output::PublishReport { group_id, object }) => {
                    self.stats.reports_published = self.stats.reports_published.saturating_add(1);
                    let track = Track::Report { participant: me };
                    self.flood(node, None, &track, group_id, &object);
                }
                Some(Output::PublishPlan { group_id, object }) => {
                    self.stats.plans_issued = self.stats.plans_issued.saturating_add(1);
                    self.world.note(group_id);
                    self.flood(node, None, &Track::Plan, group_id, &object);
                    self.apply_plan(node);
                }
                None => break,
            }
        }
        let Some(member) = self.nodes.get_mut(&node) else {
            return;
        };
        // Never book a wake for the present moment: that would spin without the clock moving.
        let wake = member
            .watches
            .values()
            .map(LinkWatch::next_wake)
            .fold(member.control.next_wake(now), Timestamp::min)
            .max(now.saturating_add(Duration::from_millis(1)));
        if member.wake_at.is_none_or(|booked| wake < booked) {
            member.wake_at = Some(wake);
            self.world
                .schedule(wake.saturating_since(now), node, Event::Wake);
        }
    }
}
