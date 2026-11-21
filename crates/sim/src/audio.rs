//! An audio overlay of khefs for simulating top-N selection: nodes joined by links in a
//! loop-free tree, each running the real [`TopN`] once per link and once for its own client.
//!
//! Every node is also a publisher of one audio track, named by its node id.

use crate::{NodeId, Rng, World};
use palaver_khef_core::{TopN, TopNConfig};
use palaver_proto::Timestamp;
use std::collections::BTreeMap;
use std::time::Duration;

/// One audio packet interval.
pub const PACKET_INTERVAL: Duration = Duration::from_millis(20);

/// An audio packet arriving at a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Arrival {
    /// The neighbour it came from, or `None` for the node's own client.
    from: Option<NodeId>,
    track: NodeId,
    sent_at: Timestamp,
    level: u8,
    speech: bool,
}

struct Node {
    neighbours: Vec<NodeId>,
    /// One selector per neighbour, in the same order.
    links: Vec<TopN<NodeId>>,
    client: TopN<NodeId>,
}

/// What listeners received of one speech packet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delivery {
    /// How many clients it reached.
    pub listeners: u32,
    /// The longest time it took to reach one of them.
    pub worst_latency: Duration,
}

pub struct AudioOverlay {
    world: World<Arrival>,
    nodes: Vec<Node>,
    loss: Rng,
    /// Delay of every link.
    pub hop_delay: Duration,
    /// Chance, in parts per million, that a packet is lost on a hop.
    pub loss_per_million: u32,
    deliveries: BTreeMap<(NodeId, Timestamp), Delivery>,
    link_packets: BTreeMap<(NodeId, NodeId), u64>,
}

impl AudioOverlay {
    /// `edges` must form a tree over nodes `0..nodes`.
    #[must_use]
    pub fn new(nodes: u32, edges: &[(NodeId, NodeId)], config: TopNConfig, seed: u64) -> Self {
        let world = World::new(seed);
        let loss = world.rng().derive(u64::MAX);
        let mut list: Vec<Node> = (0..nodes)
            .map(|_| Node {
                neighbours: vec![],
                links: vec![],
                client: TopN::new(config),
            })
            .collect();
        for &(a, b) in edges {
            for (here, there) in [(a, b), (b, a)] {
                if let Some(node) = list.get_mut(here as usize) {
                    node.neighbours.push(there);
                    node.links.push(TopN::new(config));
                }
            }
        }
        Self {
            world,
            nodes: list,
            loss,
            hop_delay: PACKET_INTERVAL,
            loss_per_million: 0,
            deliveries: BTreeMap::new(),
            link_packets: BTreeMap::new(),
        }
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

    /// The client of `node` emits a packet of its own track now.
    pub fn publish(&mut self, node: NodeId, level: u8, speech: bool) {
        let sent_at = self.world.now();
        self.world.schedule(
            Duration::ZERO,
            node,
            Arrival {
                from: None,
                track: node,
                sent_at,
                level,
                speech,
            },
        );
    }

    /// Deliver every event due at or before `until`, then set the clock to `until`.
    pub fn run_until(&mut self, until: Timestamp) {
        while self.world.next_at().is_some_and(|at| at <= until) {
            if let Some((node, arrival)) = self.world.step() {
                self.handle(node, arrival);
            }
        }
        self.world.advance_to(until);
    }

    fn handle(&mut self, at: NodeId, a: Arrival) {
        let now = self.world.now();
        let Some(node) = self.nodes.get_mut(at as usize) else {
            return;
        };
        if a.track != at && node.client.on_packet(a.track, a.level, now) && a.speech {
            let d = self.deliveries.entry((a.track, a.sent_at)).or_default();
            d.listeners += 1;
            d.worst_latency = d.worst_latency.max(now - a.sent_at);
            self.world.note(u64::from(at) << 32 | u64::from(a.track));
        }
        let mut forward = vec![];
        for (neighbour, selector) in node.neighbours.iter().zip(node.links.iter_mut()) {
            if Some(*neighbour) != a.from && selector.on_packet(a.track, a.level, now) {
                forward.push(*neighbour);
            }
        }
        for neighbour in forward {
            *self.link_packets.entry((at, neighbour)).or_default() += 1;
            if self.loss.chance(self.loss_per_million) {
                continue;
            }
            self.world.schedule(
                self.hop_delay,
                neighbour,
                Arrival {
                    from: Some(at),
                    ..a
                },
            );
        }
    }

    /// What became of the speech packet `track` sent at `sent_at`.
    #[must_use]
    pub fn delivery(&self, track: NodeId, sent_at: Timestamp) -> Delivery {
        self.deliveries
            .get(&(track, sent_at))
            .copied()
            .unwrap_or_default()
    }

    /// The most packets any one link direction has carried.
    #[must_use]
    pub fn busiest_link_packets(&self) -> u64 {
        self.link_packets.values().copied().max().unwrap_or(0)
    }

    /// How many times client selectors have replaced a track, summed over all nodes.
    #[must_use]
    pub fn client_switches(&self) -> u64 {
        self.nodes.iter().map(|n| n.client.switches()).sum()
    }
}

/// A stretch of speech by one participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spurt {
    pub who: NodeId,
    pub start: Timestamp,
    pub len: Duration,
}

/// Turns a script of speech into packets, the way an Opus sender behaves.
///
/// That is a packet every 20 ms while speaking (varying in level, with dips between
/// syllables), a few quiet packets after speech ends, then a quiet packet every 400 ms.
///
/// Levels are on the scale of design section 8 (127 is full scale, a step is a decibel):
/// speech between 83 and 118, a quiet room between 50 and 70.
pub struct Voices {
    rng: Rng,
    base: Vec<u8>,
    floor: Vec<u8>,
    last_speech: Vec<Option<Timestamp>>,
    /// Participants that have stopped sending altogether, and since when.
    pub crashed: Vec<(NodeId, Timestamp)>,
}

impl Voices {
    #[must_use]
    pub fn new(participants: u32, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let base = (0..participants)
            .map(|_| 95 + u8::try_from(rng.below(16)).unwrap_or(0))
            .collect();
        let floor = (0..participants)
            .map(|_| 50 + u8::try_from(rng.below(21)).unwrap_or(0))
            .collect();
        Self {
            rng,
            base,
            floor,
            last_speech: vec![None; participants as usize],
            crashed: vec![],
        }
    }

    /// Emit this tick's packets into `overlay`. Call once per packet interval.
    pub fn tick(&mut self, overlay: &mut AudioOverlay, script: &[Spurt]) {
        let now = overlay.now();
        // `base` has one entry per participant, and their number came in as a `u32`.
        let participants = u32::try_from(self.base.len()).unwrap_or(u32::MAX);
        for who in 0..participants {
            if self
                .crashed
                .iter()
                .any(|&(node, since)| node == who && now >= since)
            {
                continue;
            }
            let i = who as usize;
            let (base, floor) = (self.base[i], self.floor[i]);
            let spurt = script
                .iter()
                .find(|s| s.who == who && now >= s.start && now < s.start + s.len);
            if let Some(spurt) = spurt {
                let wobble = u8::try_from(self.rng.below(17)).unwrap_or(0);
                let level = if now == spurt.start {
                    base - 12 // a word's first frame is weaker
                } else if self.rng.chance(120_000) {
                    70 // a dip between syllables, below the minimum level
                } else {
                    base - 8 + wobble
                };
                self.last_speech[i] = Some(now);
                overlay.publish(who, level, true);
            } else {
                let just_stopped =
                    self.last_speech[i].is_some_and(|at| now - at <= PACKET_INTERVAL * 3);
                let interval = u64::try_from(PACKET_INTERVAL.as_micros()).unwrap_or(u64::MAX);
                let tick = now.as_micros() / interval;
                let dtx_slot = (tick + u64::from(who) * 7).is_multiple_of(20);
                if just_stopped || dtx_slot {
                    overlay.publish(who, floor, false);
                }
            }
        }
    }
}

/// Eight transit khefs joined as a tree (`chain` false: longest path 7 hops between leaves)
/// or in a line (`chain` true: 9 hops), with the remaining nodes attached to them in turn.
#[must_use]
pub fn transit_topology(nodes: u32, chain: bool) -> Vec<(NodeId, NodeId)> {
    let mut edges: Vec<(NodeId, NodeId)> = if chain {
        (0..7).map(|i| (i, i + 1)).collect()
    } else {
        vec![(0, 1), (0, 2), (1, 3), (1, 4), (2, 5), (2, 6), (3, 7)]
    };
    edges.extend((8..nodes).map(|leaf| (leaf, leaf % 8)));
    edges
}
