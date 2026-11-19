//! Meetings forming and re-forming: the real control logic, flood and wiring rule on every
//! node, over simulated links.
#![allow(clippy::unwrap_used)]

use palaver_proto::{Edge, PlanStamp, Timestamp};
use palaver_sim::meeting::Meeting;
use std::time::Duration;

const SECOND: Duration = Duration::from_secs(1);

fn meeting(seed: u64) -> Meeting {
    Meeting::new(seed).unwrap()
}

/// A meeting created by node 0 that nodes `1..nodes` join through it, 100 ms apart.
fn gathered(nodes: u32, seed: u64) -> Meeting {
    gathered_every(Duration::from_millis(100), nodes, seed)
}

fn gathered_every(spacing: Duration, nodes: u32, seed: u64) -> Meeting {
    let mut m = meeting(seed);
    assert!(m.create(0));
    for node in 1..nodes {
        m.run_for(spacing);
        assert!(m.join(node, 0));
    }
    m
}

fn edge(a: u32, b: u32) -> Edge {
    let (a, b) = (Meeting::id_of(a), Meeting::id_of(b));
    if a < b { Edge(a, b) } else { Edge(b, a) }
}

/// The time the meeting takes to settle from now, checked every 50 ms, up to `limit`.
fn settle(m: &mut Meeting, limit: Duration) -> Duration {
    let start = m.now();
    loop {
        if m.settled().is_ok() {
            return m.now().saturating_since(start);
        }
        let waited = m.now().saturating_since(start);
        assert!(
            waited < limit,
            "not settled after {waited:?}: {}",
            m.settled().unwrap_err()
        );
        m.run_for(Duration::from_millis(50));
    }
}

#[test]
fn the_creator_alone_holds_a_plan_of_one() {
    let mut m = meeting(1);
    m.create(0);
    m.run_for(SECOND);
    let plan = m.settled().unwrap();
    assert_eq!(
        plan.stamp,
        PlanStamp {
            term: 1,
            generation: 0
        }
    );
    assert_eq!(plan.conductor, Meeting::id_of(0));
    assert_eq!(m.stats().plans_issued, 1);
}

#[test]
fn a_second_node_joins_and_both_follow_the_same_plan() {
    let mut m = meeting(1);
    m.create(0);
    m.run_for(SECOND);
    m.join(1, 0);
    let took = settle(&mut m, SECOND);
    assert!(took <= Duration::from_millis(150), "took {took:?}");
    let plan = m.settled().unwrap();
    assert_eq!(plan.audio_overlay, [edge(0, 1)]);
    assert_eq!(m.links_of(1), [0]);
}

#[test]
fn six_nodes_gather_through_the_creator() {
    let mut m = gathered(6, 7);
    let took = settle(&mut m, 2 * SECOND);
    assert!(took <= Duration::from_millis(500), "took {took:?}");
    let plan = m.settled().unwrap();
    assert_eq!(plan.roster.len(), 6);
    assert_eq!(plan.audio_overlay.len(), 5);
    // And it stays settled, with no further plans, while nothing changes.
    let plans = m.stats().plans_issued;
    m.run_for(10 * SECOND);
    m.settled().unwrap();
    assert_eq!(m.stats().plans_issued, plans);
}

#[test]
fn a_node_can_join_through_any_member() {
    let mut m = gathered(4, 3);
    settle(&mut m, 2 * SECOND);
    m.join(9, 3);
    settle(&mut m, 2 * SECOND);
    let plan = m.settled().unwrap();
    assert!(
        plan.audio_overlay.contains(&edge(3, 9)),
        "the joiner is attached by the link it came in on"
    );
}

#[test]
fn the_three_node_line_with_no_link_between_the_ends() {
    // 0 conducts, 2 can reach only 1. The plan first hopes for a 0-2 peering; the dial
    // fails, 0 reports 2 unreachable (or 2 reports 0), and the next plan drops it.
    let mut m = meeting(5);
    m.block(0, 2);
    m.create(0);
    m.run_for(SECOND);
    m.join(1, 0);
    m.run_for(SECOND);
    m.join(2, 1);
    let took = settle(&mut m, 3 * SECOND);
    let plan = m.settled().unwrap();
    assert_eq!(plan.roster.len(), 3);
    let mut line = vec![edge(0, 1), edge(1, 2)];
    line.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    assert_eq!(plan.audio_overlay, line);
    assert_eq!(plan.peerings, line);
    assert_eq!(m.links_of(1), [0, 2]);
    assert_eq!(m.stats().dials_failed, 1);
    // Audio was wired over the working link at once; only the spare peering took a timeout.
    assert!(took <= Duration::from_millis(900), "took {took:?}");
}

#[test]
fn a_node_that_crashes_is_dropped_within_about_three_seconds() {
    let mut m = gathered(6, 11);
    settle(&mut m, 2 * SECOND);
    let before = m.settled().unwrap().clone();
    // Pick a leaf of the audio tree that is not the conductor.
    let leaf = (1..6)
        .find(|n| {
            let id = Meeting::id_of(*n);
            before
                .audio_overlay
                .iter()
                .filter(|e| e.0 == id || e.1 == id)
                .count()
                == 1
        })
        .unwrap();
    m.crash(leaf);
    let took = settle(&mut m, 5 * SECOND);
    assert!(took <= Duration::from_millis(3600), "took {took:?}");
    let after = m.settled().unwrap();
    assert_eq!(after.roster.len(), 5);
    assert!(before.may_replace(after) || before.stamp < after.stamp);
    // Nobody else's audio link moved.
    let gone = Meeting::id_of(leaf);
    for e in &before.audio_overlay {
        if e.0 != gone && e.1 != gone {
            assert!(after.audio_overlay.contains(e), "{e:?} was dropped");
        }
    }
}

#[test]
fn when_a_node_in_the_middle_of_the_tree_crashes_the_tree_is_rejoined() {
    let mut m = gathered(20, 13);
    settle(&mut m, 3 * SECOND);
    let before = m.settled().unwrap().clone();
    // A node other than the conductor with more than one audio link.
    let middle = (1..20)
        .find(|n| {
            let id = Meeting::id_of(*n);
            before
                .audio_overlay
                .iter()
                .filter(|e| e.0 == id || e.1 == id)
                .count()
                > 1
        })
        .expect("a tree of 20 with degree 4 has inner nodes");
    m.crash(middle);
    let took = settle(&mut m, 6 * SECOND);
    assert!(took <= Duration::from_millis(4000), "took {took:?}");
    assert_eq!(m.settled().unwrap().roster.len(), 19);
}

#[test]
fn two_nodes_crashing_together_are_both_dropped() {
    let mut m = gathered(10, 17);
    settle(&mut m, 3 * SECOND);
    m.crash(4);
    m.crash(7);
    settle(&mut m, 6 * SECOND);
    assert_eq!(m.settled().unwrap().roster.len(), 8);
}

#[test]
fn nodes_joining_while_others_leave_still_settle() {
    let mut m = gathered(8, 19);
    m.run_for(Duration::from_millis(300));
    m.crash(3);
    m.join(20, 5);
    m.run_for(Duration::from_millis(700));
    m.join(21, 20);
    m.crash(6);
    settle(&mut m, 6 * SECOND);
    assert_eq!(m.live(), [0, 1, 2, 4, 5, 7, 20, 21]);
}

#[test]
fn a_node_cut_off_by_the_crash_of_all_its_neighbours_finds_its_way_back() {
    let mut m = gathered(20, 37);
    settle(&mut m, 3 * SECOND);
    // A node with no link to the conductor, so the conductor does not see it happen.
    let alone = (1..20).find(|n| !m.links_of(*n).contains(&0)).unwrap();
    let neighbours = m.links_of(alone);
    for n in &neighbours {
        m.crash(*n);
    }
    let took = settle(&mut m, 8 * SECOND);
    assert!(m.live().contains(&alone));
    assert_eq!(m.settled().unwrap().roster.len(), 20 - neighbours.len());
    // Detection, a failed dial to each dead neighbour in the old plan at worst, then the
    // three seconds the dead take to leave the roster.
    assert!(took <= Duration::from_millis(4500), "took {took:?}");
}

#[test]
fn a_joiner_keeps_its_entry_link_until_its_planned_links_are_up() {
    // 9 joins through 3 but can reach nobody else for a while: whatever the plan says, the
    // entry link must stay, or 9 would be cut off from the plan that is meant to wire it.
    let mut m = gathered(4, 41);
    settle(&mut m, 2 * SECOND);
    for other in 0..3 {
        m.block(9, other);
    }
    m.join(9, 3);
    for _ in 0..40 {
        m.run_for(Duration::from_millis(50));
        assert!(
            m.links_of(9).contains(&3) || m.now() < Timestamp::from_millis(2400),
            "the entry link was dropped at {:?}",
            m.now()
        );
    }
    let plan = m.settled().unwrap();
    assert!(plan.audio_overlay.contains(&edge(3, 9)));
}

#[test]
fn fifty_nodes_form_a_meeting() {
    let mut m = gathered_every(Duration::from_millis(40), 50, 23);
    let took = settle(&mut m, 5 * SECOND);
    assert!(took <= SECOND, "took {took:?}");
    let plan = m.settled().unwrap();
    assert_eq!(plan.roster.len(), 50);
    assert_eq!(plan.audio_overlay.len(), 49);
    // One plan per join, plus the first: nothing made the conductor plan twice.
    assert_eq!(m.stats().plans_issued, 50);

    // The cost of the flood while nothing changes: per second, each of 50 reports crosses
    // each link at most once each way.
    let links = plan.peerings.len() as u64;
    let before = m.stats();
    m.run_for(3 * SECOND);
    m.settled().unwrap();
    let after = m.stats();
    assert_eq!(after.plans_issued, before.plans_issued);
    let per_second = (after.objects_sent - before.objects_sent) / 3;
    assert!(
        per_second <= 50 * 2 * links,
        "{per_second} objects a second over {links} links"
    );
    assert!(
        per_second >= 50 * links,
        "{per_second} is too few to be a flood"
    );
}

#[test]
fn a_join_makes_only_the_nodes_it_touches_report_at_once() {
    let mut m = gathered_every(Duration::from_millis(40), 50, 23);
    settle(&mut m, 5 * SECOND);
    m.run_for(2 * SECOND);
    let before = m.stats();
    m.join(100, 7);
    m.run_for(Duration::from_millis(150));
    m.settled().unwrap();
    let reports = m.stats().reports_published - before.reports_published;
    // About 8 regular reports fall in any 150 ms (51 nodes, one a second each), plus the
    // joiner and the few nodes it was wired to. If every node reported the new plan at
    // once there would be more than 50.
    assert!(
        reports <= 16,
        "{reports} reports in the 150 ms after a join"
    );
}

#[test]
fn everyone_joining_in_the_same_instant_settles() {
    let mut m = meeting(29);
    m.create(0);
    for node in 1..30 {
        m.join(node, 0);
    }
    settle(&mut m, 3 * SECOND);
    assert_eq!(m.settled().unwrap().roster.len(), 30);
}

#[test]
fn a_node_number_cannot_be_used_twice() {
    let mut m = meeting(1);
    assert!(m.create(0));
    assert!(!m.join(0, 0));
    assert!(!m.create(0));
    assert!(!m.join(256, 0), "beyond the stand-in identities");
}

/// A busy run whose trace depends on jitter, ordering and timeouts.
fn busy(seed: u64) -> (u64, palaver_sim::meeting::Stats, Timestamp) {
    let mut m = gathered(16, seed);
    m.block(3, 9);
    m.run_for(SECOND);
    m.crash(5);
    m.join(40, 9);
    m.run_for(5 * SECOND);
    m.settled().unwrap();
    (m.trace(), m.stats(), m.now())
}

#[test]
fn the_same_seed_gives_the_same_run() {
    let first = busy(31);
    assert_eq!(first, busy(31));
    assert_ne!(first.0, busy(32).0);
}
