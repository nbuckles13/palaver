//! The phase 1 wiring rule: which khefs peer, and the one tree audio is forwarded along.
//!
//! Deliberately simple (scored, capacity-aware plans come later), but it emits a real plan:
//!
//! - **Change as little as possible.** Every edge of the previous plan stays while both its
//!   ends are members and neither reports the other unreachable. Audio that is flowing is not
//!   disturbed by someone else joining.
//! - **Audio tree.** What is left of the previous tree is a set of pieces; the piece holding
//!   the root (the conductor) is grown by one edge at a time until it holds everyone. The
//!   edge chosen is the best by, in order: a link that is already up, then one not
//!   tried yet; the attachment point nearest the root; the
//!   lowest round-trip time; the lowest ids. No node gets more than [`MAX_AUDIO_DEGREE`] tree
//!   links unless that is the only way to reach someone.
//! - **Peerings.** Every tree edge is a peering. Each node is then given peerings to its
//!   nearest nodes until it has [`MIN_PEERINGS`], so the control flood has a second path and
//!   a replacement tree edge is already connected when a neighbour leaves. Nodes that
//!   already have [`BUSY_PEERINGS`] are passed over while another will do.
//!
//! A pure function of its input: the same input gives the same wiring on every node.

use palaver_proto::{Edge, ParticipantId};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Most audio tree links a node carries, unless a node can be reached no other way. Each link
/// carries at most N tracks each way, so this bounds a node's audio uplink.
pub const MAX_AUDIO_DEGREE: usize = 4;
/// Peerings every node is given where the graph allows.
pub const MIN_PEERINGS: usize = 2;
/// A node with this many peerings is passed over for extra ones while another will do.
///
/// Otherwise the node everyone joined through, to which they all hold a link that is up,
/// would be everyone's second peering and carry the whole control flood.
pub const BUSY_PEERINGS: usize = 6;

/// A connection one node says it holds to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedLink {
    pub from: ParticipantId,
    pub to: ParticipantId,
    pub rtt: Duration,
}

/// What the wiring is computed from.
#[derive(Debug, Clone, Default)]
pub struct Input {
    /// Everyone in the meeting. Order and duplicates do not matter.
    pub members: Vec<ParticipantId>,
    /// The audio tree is grown from this node: the conductor. `None`, or a node that is not a
    /// member, means the lowest member id.
    pub root: Option<ParticipantId>,
    /// Links as reported, by either end or both.
    pub links: Vec<ReportedLink>,
    /// Pairs where one node reports it cannot connect to the other, in either order.
    pub unreachable: Vec<Edge>,
    /// The peerings and audio tree of the plan in force, if any.
    pub previous_peerings: Vec<Edge>,
    pub previous_audio_overlay: Vec<Edge>,
}

/// The result. Every edge has the lower id first; both lists are sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wiring {
    pub peerings: Vec<Edge>,
    pub audio_overlay: Vec<Edge>,
    /// Members the audio tree could not reach: every pair that would connect them is reported
    /// unreachable. Sorted.
    pub unwired: Vec<ParticipantId>,
}

/// How good a pair is to wire, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    /// Connected now.
    Up,
    /// Not connected, and nobody says it cannot be.
    Untried,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Cost {
    class: Class,
    rtt: Duration,
}

const UNTRIED: Cost = Cost {
    class: Class::Untried,
    rtt: Duration::ZERO,
};

type Pair = (ParticipantId, ParticipantId);

fn pair(a: &ParticipantId, b: &ParticipantId) -> Pair {
    if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

/// What is known about the pairs of members.
struct Graph {
    members: BTreeSet<ParticipantId>,
    up: BTreeMap<Pair, Cost>,
    blocked: BTreeSet<Pair>,
}

impl Graph {
    fn new(input: &Input) -> Self {
        let members: BTreeSet<ParticipantId> = input.members.iter().cloned().collect();
        let both = |a: &ParticipantId, b: &ParticipantId| {
            a != b && members.contains(a) && members.contains(b)
        };
        let mut up: BTreeMap<Pair, Cost> = BTreeMap::new();
        for link in input.links.iter().filter(|l| both(&l.from, &l.to)) {
            let cost = Cost {
                class: Class::Up,
                rtt: link.rtt,
            };
            // Where the two ends disagree, believe the worse account.
            up.entry(pair(&link.from, &link.to))
                .and_modify(|held| *held = (*held).max(cost))
                .or_insert(cost);
        }
        let blocked = input
            .unreachable
            .iter()
            .filter(|e| both(&e.0, &e.1))
            .map(|e| pair(&e.0, &e.1))
            .collect();
        Self {
            members,
            up,
            blocked,
        }
    }

    /// The cost of wiring a pair, or `None` if it cannot be wired. A link that is up counts
    /// even if one end once reported the other unreachable.
    fn cost(&self, a: &ParticipantId, b: &ParticipantId) -> Option<Cost> {
        let key = pair(a, b);
        match self.up.get(&key) {
            Some(cost) => Some(*cost),
            None if self.blocked.contains(&key) => None,
            None => Some(UNTRIED),
        }
    }

    /// The edges of `previous` that may stay.
    fn kept(&self, previous: &[Edge]) -> BTreeSet<Pair> {
        previous
            .iter()
            .filter(|e| e.0 != e.1 && self.members.contains(&e.0) && self.members.contains(&e.1))
            .filter(|e| self.cost(&e.0, &e.1).is_some())
            .map(|e| pair(&e.0, &e.1))
            .collect()
    }
}

fn degrees(edges: &BTreeSet<Pair>) -> BTreeMap<ParticipantId, usize> {
    let mut out: BTreeMap<ParticipantId, usize> = BTreeMap::new();
    for (a, b) in edges {
        for end in [a, b] {
            let count = out.entry(end.clone()).or_default();
            *count = count.saturating_add(1);
        }
    }
    out
}

fn degree(degrees: &BTreeMap<ParticipantId, usize>, node: &ParticipantId) -> usize {
    degrees.get(node).copied().unwrap_or(0)
}

/// Distance in edges from `root` to every node `edges` connects it to, `root` included.
fn depths(edges: &BTreeSet<Pair>, root: &ParticipantId) -> BTreeMap<ParticipantId, usize> {
    let mut neighbours: BTreeMap<&ParticipantId, Vec<&ParticipantId>> = BTreeMap::new();
    for (a, b) in edges {
        neighbours.entry(a).or_default().push(b);
        neighbours.entry(b).or_default().push(a);
    }
    let mut out = BTreeMap::from([(root.clone(), 0usize)]);
    let mut frontier = vec![root];
    let mut depth = 0usize;
    while !frontier.is_empty() {
        depth = depth.saturating_add(1);
        let mut next = Vec::new();
        for node in frontier {
            for other in neighbours.get(node).into_iter().flatten() {
                if !out.contains_key(*other) {
                    out.insert((*other).clone(), depth);
                    next.push(*other);
                }
            }
        }
        frontier = next;
    }
    out
}

/// Drop edges that would close a loop, so what is kept of an old tree is still loop-free
/// even if the old plan was not. Returns the edges kept.
fn loop_free(edges: BTreeSet<Pair>) -> BTreeSet<Pair> {
    // Which piece each node is in, named by one of its nodes. Joining two pieces renames one.
    let mut piece: BTreeMap<ParticipantId, ParticipantId> = BTreeMap::new();
    let mut out = BTreeSet::new();
    for edge in edges {
        let a = piece.get(&edge.0).unwrap_or(&edge.0).clone();
        let b = piece.get(&edge.1).unwrap_or(&edge.1).clone();
        if a == b {
            continue;
        }
        for name in piece.values_mut().filter(|name| **name == b) {
            *name = a.clone();
        }
        piece.insert(edge.0.clone(), a.clone());
        piece.insert(edge.1.clone(), a);
        out.insert(edge);
    }
    out
}

/// Grow the piece of `tree` that holds `root` until it holds every member it can.
fn grow_tree(graph: &Graph, root: &ParticipantId, tree: &mut BTreeSet<Pair>) {
    loop {
        let inside = depths(tree, root);
        if inside.len() >= graph.members.len() {
            return;
        }
        let degrees = degrees(tree);
        let mut best = None;
        for (from, depth) in &inside {
            for to in graph.members.iter().filter(|m| !inside.contains_key(*m)) {
                let Some(cost) = graph.cost(from, to) else {
                    continue;
                };
                let crowded = degree(&degrees, from) >= MAX_AUDIO_DEGREE
                    || degree(&degrees, to) >= MAX_AUDIO_DEGREE;
                let key = (crowded, cost.class, *depth, cost.rtt, from, to);
                if best.as_ref().is_none_or(|held| key < *held) {
                    best = Some(key);
                }
            }
        }
        match best {
            Some((.., from, to)) => tree.insert(pair(from, to)),
            None => return,
        };
    }
}

/// Give every node its nearest peers until it has [`MIN_PEERINGS`].
fn add_peerings(graph: &Graph, peerings: &mut BTreeSet<Pair>) {
    let mut degrees = degrees(peerings);
    for node in &graph.members {
        while degree(&degrees, node) < MIN_PEERINGS {
            let best = graph
                .members
                .iter()
                .filter(|other| *other != node && !peerings.contains(&pair(node, other)))
                .filter_map(|other| {
                    let cost = graph.cost(node, other)?;
                    let peers = degree(&degrees, other);
                    Some((peers >= BUSY_PEERINGS, cost, peers, other))
                })
                .min();
            let Some((.., other)) = best else {
                break;
            };
            peerings.insert(pair(node, other));
            for end in [node, other] {
                let count = degrees.entry(end.clone()).or_default();
                *count = count.saturating_add(1);
            }
        }
    }
}

fn edges(pairs: BTreeSet<Pair>) -> Vec<Edge> {
    pairs.into_iter().map(|(a, b)| Edge(a, b)).collect()
}

/// Compute the peerings and the audio tree.
#[must_use]
pub fn wire(input: &Input) -> Wiring {
    let graph = Graph::new(input);
    let root = input
        .root
        .as_ref()
        .filter(|root| graph.members.contains(*root))
        .or_else(|| graph.members.first());
    let Some(root) = root else {
        return Wiring::default();
    };

    let mut tree = loop_free(graph.kept(&input.previous_audio_overlay));
    grow_tree(&graph, root, &mut tree);
    // Pieces of the old tree that could not be joined to the root carry no audio; drop them.
    let reached = depths(&tree, root);
    tree.retain(|(a, _)| reached.contains_key(a));

    let mut peerings = graph.kept(&input.previous_peerings);
    peerings.extend(tree.iter().cloned());
    add_peerings(&graph, &mut peerings);

    Wiring {
        peerings: edges(peerings),
        audio_overlay: edges(tree),
        unwired: graph
            .members
            .iter()
            .filter(|m| !reached.contains_key(*m))
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ParticipantId {
        ParticipantId::new(&[n]).unwrap()
    }

    fn ids(range: core::ops::RangeInclusive<u8>) -> Vec<ParticipantId> {
        range.map(id).collect()
    }

    fn edge(a: u8, b: u8) -> Edge {
        Edge(id(a), id(b))
    }

    fn direct(from: u8, to: u8, rtt_ms: u64) -> ReportedLink {
        ReportedLink {
            from: id(from),
            to: id(to),
            rtt: Duration::from_millis(rtt_ms),
        }
    }

    /// The wiring that follows `previous` when the rest of the input is `input`.
    fn rewire(previous: &Wiring, input: Input) -> Wiring {
        wire(&Input {
            previous_peerings: previous.peerings.clone(),
            previous_audio_overlay: previous.audio_overlay.clone(),
            ..input
        })
    }

    /// Panics unless `wiring` is sound for `members`: the audio edges form one tree over all
    /// of them, every audio edge is a peering, and edges are in canonical order.
    fn assert_sound(wiring: &Wiring, members: &[ParticipantId]) {
        assert_eq!(wiring.unwired, [], "everyone is wired");
        let tree: BTreeSet<Pair> = wiring
            .audio_overlay
            .iter()
            .map(|e| (e.0.clone(), e.1.clone()))
            .collect();
        assert_eq!(
            tree.len(),
            members.len().saturating_sub(1),
            "a tree has n-1 edges"
        );
        if let Some(first) = members.first() {
            assert_eq!(
                depths(&tree, first).len(),
                members.len(),
                "the tree is connected"
            );
        }
        for e in wiring.peerings.iter().chain(&wiring.audio_overlay) {
            assert!(e.0 < e.1, "lower id first: {e:?}");
            assert!(members.contains(&e.0) && members.contains(&e.1));
        }
        for e in &wiring.audio_overlay {
            assert!(
                wiring.peerings.contains(e),
                "audio edge {e:?} is not a peering"
            );
        }
        let sorted = |list: &[Edge]| {
            list.windows(2)
                .all(|w| (&w[0].0, &w[0].1) < (&w[1].0, &w[1].1))
        };
        assert!(sorted(&wiring.peerings) && sorted(&wiring.audio_overlay));
    }

    #[test]
    fn nobody_and_one_node_need_no_wiring() {
        assert_eq!(wire(&Input::default()), Wiring::default());
        let alone = wire(&Input {
            members: ids(1..=1),
            ..Input::default()
        });
        assert_eq!(alone, Wiring::default());
    }

    #[test]
    fn two_nodes_get_the_one_edge() {
        let w = wire(&Input {
            members: ids(1..=2),
            ..Input::default()
        });
        assert_eq!(w.audio_overlay, [edge(1, 2)]);
        assert_eq!(w.peerings, [edge(1, 2)]);
    }

    #[test]
    fn a_joiner_is_attached_by_the_link_it_already_has() {
        // 1 and 2 are wired; 3 joined through 2.
        let before = wire(&Input {
            members: ids(1..=2),
            root: Some(id(1)),
            links: vec![direct(1, 2, 2)],
            ..Input::default()
        });
        let after = rewire(
            &before,
            Input {
                members: ids(1..=3),
                root: Some(id(1)),
                links: vec![direct(1, 2, 2), direct(3, 2, 2)],
                ..Input::default()
            },
        );
        assert_eq!(after.audio_overlay, [edge(1, 2), edge(2, 3)]);
        // Its second peering is the only other node.
        assert_eq!(after.peerings, [edge(1, 2), edge(1, 3), edge(2, 3)]);
        assert_sound(&after, &ids(1..=3));
    }

    #[test]
    fn nothing_changes_for_others_when_someone_joins() {
        let mut wiring = Wiring::default();
        for n in 2..=30u8 {
            let next = rewire(
                &wiring,
                Input {
                    members: ids(1..=n),
                    root: Some(id(1)),
                    ..Input::default()
                },
            );
            assert_sound(&next, &ids(1..=n));
            for e in &wiring.audio_overlay {
                assert!(next.audio_overlay.contains(e), "{e:?} was dropped at {n}");
            }
            for e in &wiring.peerings {
                assert!(next.peerings.contains(e), "{e:?} was dropped at {n}");
            }
            assert_eq!(next.audio_overlay.len(), wiring.audio_overlay.len() + 1);
            wiring = next;
        }
    }

    #[test]
    fn the_tree_is_bounded_in_degree_and_shallow() {
        let members = ids(1..=50);
        let w = wire(&Input {
            members: members.clone(),
            root: Some(id(1)),
            ..Input::default()
        });
        assert_sound(&w, &members);
        let tree: BTreeSet<Pair> = w
            .audio_overlay
            .iter()
            .map(|e| (e.0.clone(), e.1.clone()))
            .collect();
        let most = degrees(&tree).into_values().max().unwrap();
        assert_eq!(most, MAX_AUDIO_DEGREE);
        // Root with 4 children, then 3 each: 4 + 12 + 36 >= 49, so depth 3.
        let deepest = depths(&tree, &id(1)).into_values().max().unwrap();
        assert_eq!(deepest, 3);
    }

    #[test]
    fn every_node_gets_two_peerings_where_there_are_three_or_more() {
        for n in 3..=20u8 {
            let members = ids(1..=n);
            let w = wire(&Input {
                members: members.clone(),
                ..Input::default()
            });
            let pairs: BTreeSet<Pair> = w
                .peerings
                .iter()
                .map(|e| (e.0.clone(), e.1.clone()))
                .collect();
            let degrees = degrees(&pairs);
            for m in &members {
                assert!(degree(&degrees, m) >= MIN_PEERINGS, "node {m:?} of {n}");
            }
        }
    }

    #[test]
    fn extra_peerings_are_spread_rather_than_piled_on_one_node() {
        let members = ids(1..=30);
        let w = wire(&Input {
            members,
            ..Input::default()
        });
        let pairs: BTreeSet<Pair> = w
            .peerings
            .iter()
            .map(|e| (e.0.clone(), e.1.clone()))
            .collect();
        let most = degrees(&pairs).into_values().max().unwrap();
        assert!(most <= MAX_AUDIO_DEGREE + 2, "a node has {most} peerings");
    }

    #[test]
    fn a_link_that_is_up_beats_an_untried_pair() {
        // 3 can attach to 1 (untried, nearer the root) or to 2 (up).
        let base = Input {
            members: ids(1..=3),
            root: Some(id(1)),
            previous_audio_overlay: vec![edge(1, 2)],
            ..Input::default()
        };
        let w = wire(&Input {
            links: vec![direct(2, 3, 40)],
            ..base.clone()
        });
        assert_eq!(w.audio_overlay, [edge(1, 2), edge(2, 3)]);
        // With no link up, the untried pair nearer the root is preferred.
        let w = wire(&base);
        assert_eq!(w.audio_overlay, [edge(1, 2), edge(1, 3)]);
    }

    #[test]
    fn among_equal_links_the_lowest_round_trip_time_wins() {
        // 4 has direct links to 2 and 3, both one step from the root.
        let w = wire(&Input {
            members: ids(1..=4),
            root: Some(id(1)),
            links: vec![direct(4, 2, 30), direct(4, 3, 8)],
            previous_audio_overlay: vec![edge(1, 2), edge(1, 3)],
            ..Input::default()
        });
        assert!(w.audio_overlay.contains(&edge(3, 4)), "{w:?}");
    }

    #[test]
    fn where_the_two_ends_disagree_the_worse_account_is_believed() {
        // 4 has links to 2 and 3. 2 says theirs takes 5 ms and 4 says 50; both ends of the
        // other say 20. The slower account of the first is believed, so the second wins.
        let w = wire(&Input {
            members: ids(1..=4),
            root: Some(id(1)),
            links: vec![
                direct(2, 4, 5),
                direct(4, 2, 50),
                direct(3, 4, 20),
                direct(4, 3, 20),
            ],
            previous_audio_overlay: vec![edge(1, 2), edge(1, 3)],
            ..Input::default()
        });
        assert!(w.audio_overlay.contains(&edge(3, 4)), "{w:?}");
    }

    #[test]
    fn the_three_node_line_with_no_link_between_the_ends() {
        // A-B-C where A and C cannot connect: B is in the middle, and nobody has two peerings
        // forced on a pair that cannot connect.
        let w = wire(&Input {
            members: ids(1..=3),
            root: Some(id(1)),
            links: vec![direct(1, 2, 3), direct(2, 3, 3)],
            unreachable: vec![edge(1, 3)],
            ..Input::default()
        });
        assert_eq!(w.audio_overlay, [edge(1, 2), edge(2, 3)]);
        assert_eq!(w.peerings, [edge(1, 2), edge(2, 3)]);
        assert_sound(&w, &ids(1..=3));
    }

    #[test]
    fn when_a_middle_node_leaves_the_pieces_are_rejoined_and_kept_whole() {
        let members = ids(1..=20);
        let before = wire(&Input {
            members: members.clone(),
            root: Some(id(1)),
            ..Input::default()
        });
        // Node 2 is a child of the root with children of its own.
        assert!(before.audio_overlay.iter().any(|e| e.0 == id(2)));
        let left: Vec<ParticipantId> = members.into_iter().filter(|m| *m != id(2)).collect();
        let after = rewire(
            &before,
            Input {
                members: left.clone(),
                root: Some(id(1)),
                ..Input::default()
            },
        );
        assert_sound(&after, &left);
        for e in &before.audio_overlay {
            if e.0 != id(2) && e.1 != id(2) {
                assert!(after.audio_overlay.contains(e), "{e:?} was dropped");
            }
        }
        assert!(after.peerings.iter().all(|e| e.0 != id(2) && e.1 != id(2)));
    }

    #[test]
    fn an_edge_reported_unreachable_is_replaced() {
        let before = wire(&Input {
            members: ids(1..=4),
            root: Some(id(1)),
            ..Input::default()
        });
        assert!(before.audio_overlay.contains(&edge(1, 4)));
        let after = rewire(
            &before,
            Input {
                members: ids(1..=4),
                root: Some(id(1)),
                unreachable: vec![edge(4, 1)],
                ..Input::default()
            },
        );
        assert!(!after.audio_overlay.contains(&edge(1, 4)));
        assert!(!after.peerings.contains(&edge(1, 4)));
        assert_sound(&after, &ids(1..=4));
    }

    #[test]
    fn a_link_that_is_up_counts_even_if_once_reported_unreachable() {
        let w = wire(&Input {
            members: ids(1..=2),
            links: vec![direct(1, 2, 4)],
            unreachable: vec![edge(2, 1)],
            ..Input::default()
        });
        assert_eq!(w.audio_overlay, [edge(1, 2)]);
    }

    #[test]
    fn a_node_nobody_can_reach_is_reported_and_the_rest_are_wired() {
        let w = wire(&Input {
            members: ids(1..=4),
            root: Some(id(1)),
            unreachable: vec![edge(4, 1), edge(4, 2), edge(3, 4)],
            ..Input::default()
        });
        assert_eq!(w.unwired, [id(4)]);
        assert_eq!(w.audio_overlay.len(), 2);
        assert!(w.peerings.iter().all(|e| e.1 != id(4)));
    }

    #[test]
    fn a_node_reachable_only_through_a_crowded_node_is_still_attached() {
        // 6 can reach only the root, which already has MAX_AUDIO_DEGREE tree links.
        let w = wire(&Input {
            members: ids(1..=6),
            root: Some(id(1)),
            unreachable: (2..=5).map(|n| edge(6, n)).collect(),
            previous_audio_overlay: (2..=5).map(|n| edge(1, n)).collect(),
            ..Input::default()
        });
        assert!(w.audio_overlay.contains(&edge(1, 6)));
        assert_sound(&w, &ids(1..=6));
    }

    #[test]
    fn a_previous_tree_with_a_loop_or_strangers_is_repaired() {
        let w = wire(&Input {
            members: ids(1..=3),
            root: Some(id(1)),
            previous_audio_overlay: vec![
                edge(1, 2),
                edge(2, 3),
                edge(3, 1),
                edge(3, 9),
                edge(2, 2),
            ],
            previous_peerings: vec![edge(1, 9)],
            ..Input::default()
        });
        assert_sound(&w, &ids(1..=3));
    }

    #[test]
    fn the_result_does_not_depend_on_the_order_of_the_input() {
        let forward = Input {
            members: ids(1..=12),
            root: Some(id(5)),
            links: vec![direct(1, 2, 3), direct(7, 3, 9), direct(4, 12, 1)],
            unreachable: vec![edge(1, 5), edge(8, 9)],
            ..Input::default()
        };
        let mut backward = forward.clone();
        backward.members.reverse();
        backward.members.push(id(3)); // a duplicate
        backward.links.reverse();
        backward.unreachable = vec![edge(9, 8), edge(5, 1)];
        assert_eq!(wire(&forward), wire(&backward));
        assert_sound(&wire(&forward), &ids(1..=12));
    }

    #[test]
    fn a_root_that_is_not_a_member_falls_back_to_the_lowest_id() {
        let w = wire(&Input {
            members: ids(1..=5),
            root: Some(id(9)),
            ..Input::default()
        });
        assert_sound(&w, &ids(1..=5));
    }

    #[test]
    fn the_node_everyone_joined_through_does_not_become_everyones_second_peering() {
        // Everyone holds an up, direct link to node 1, as after joining through it.
        let members = ids(1..=30);
        let w = wire(&Input {
            members: members.clone(),
            root: Some(id(1)),
            links: (2..=30).map(|n| direct(n, 1, 2)).collect(),
            ..Input::default()
        });
        assert_sound(&w, &members);
        let hub = w.peerings.iter().filter(|e| e.0 == id(1)).count();
        assert!(hub <= BUSY_PEERINGS, "node 1 has {hub} peerings");
    }
}
