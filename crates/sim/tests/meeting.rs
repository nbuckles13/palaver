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
            assert_eq!(
                m.stats().sessions_closed,
                0,
                "a session met a protocol error"
            );
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
fn a_node_that_crashes_is_dropped_within_a_second_and_a_half_with_one_plan() {
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
    let plans = m.stats().plans_issued;
    m.crash(leaf);
    let took = settle(&mut m, 5 * SECOND);
    // 700 ms for its neighbours to notice the silence, 500 ms of hold, and the flood.
    assert!(took <= Duration::from_millis(1400), "took {took:?}");
    m.run_for(3 * SECOND);
    assert_eq!(m.stats().plans_issued, plans + 1);
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
    let plans = m.stats().plans_issued;
    let took = settle(&mut m, 6 * SECOND);
    assert!(took <= Duration::from_millis(1500), "took {took:?}");
    m.run_for(3 * SECOND);
    assert_eq!(
        m.stats().plans_issued,
        plans + 1,
        "one plan repairs the tree"
    );
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
    // It notices at 700 ms, dials members of the last plan it saw (a failed dial costs
    // 500 ms), and is back before or soon after the dead have left the roster.
    assert!(took <= Duration::from_millis(2500), "took {took:?}");
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
}

#[test]
fn while_nothing_changes_almost_nothing_is_sent() {
    // Each node repeats its report every 30 s, and idle links carry a filler every 200 ms
    // each way. Nothing else.
    let mut m = gathered(20, 43);
    settle(&mut m, 3 * SECOND);
    let links = m.settled().unwrap().peerings.len() as u64;
    m.run_for(35 * SECOND);
    let before = m.stats();
    m.run_for(30 * SECOND);
    m.settled().unwrap();
    let after = m.stats();
    assert_eq!(after.plans_issued, before.plans_issued);
    assert_eq!(after.reports_published - before.reports_published, 20);
    // Each of the 20 reports crosses each link at most once each way, over 30 s.
    let objects = after.objects_sent - before.objects_sent;
    assert!(
        (20 * links..=20 * 2 * links).contains(&objects),
        "{objects} objects over {links} links"
    );
    let fillers_a_second = (after.fillers_sent - before.fillers_sent) / 30;
    assert!(
        (4 * 2 * links..=5 * 2 * links).contains(&fillers_a_second),
        "{fillers_a_second} fillers a second over {links} links"
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
    // The joiner and the few nodes it was wired to. If every node reported the new plan at
    // once there would be more than 50.
    assert!(reports <= 6, "{reports} reports in the 150 ms after a join");
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

// --- Audio, over the tree the plan lays out ---

/// `speaker` talks for `frames` frames, 20 ms apart, at `level`, starting at frame `first`.
fn talk(m: &mut Meeting, speaker: u32, first: u64, frames: u64, level: u8) {
    for frame in first..first + frames {
        assert!(
            m.speak(speaker, frame, level),
            "node {speaker} has no alias"
        );
        m.run_for(Duration::from_millis(20));
    }
}

/// How many frames of `speaker` node `listener` was given to play.
fn heard(m: &Meeting, listener: u32, speaker: u32) -> usize {
    let alias = m.microphone_alias(listener, speaker).unwrap();
    m.heard_by(listener)
        .iter()
        .filter(|(a, _)| *a == alias)
        .count()
}

#[test]
fn a_speaker_is_heard_by_everyone_else_over_the_plans_tree() {
    let mut m = gathered(8, 47);
    settle(&mut m, 3 * SECOND);
    let before = m.stats().audio_sent;
    talk(&mut m, 3, 0, 50, 70);
    m.run_for(Duration::from_millis(200));
    for listener in [0, 1, 2, 4, 5, 6, 7] {
        assert_eq!(heard(&m, listener, 3), 50, "node {listener}");
    }
    assert_eq!(m.heard_by(3), [], "nobody hears themselves");
    // Each frame crossed each of the seven tree links once.
    assert_eq!(m.stats().audio_sent - before, 50 * 7);
    // Everyone agrees on the alias, and it is small.
    let alias = m.microphone_alias(0, 3).unwrap();
    assert!(alias < 128);
    assert!((0..8).all(|node| m.microphone_alias(node, 3) == Some(alias)));
    assert_eq!(m.stats().sessions_closed, 0);
}

#[test]
fn audio_crosses_the_three_node_line_through_the_middle() {
    let mut m = meeting(5);
    m.block(0, 2);
    m.create(0);
    m.run_for(SECOND);
    m.join(1, 0);
    m.run_for(SECOND);
    m.join(2, 1);
    settle(&mut m, 3 * SECOND);
    talk(&mut m, 0, 0, 25, 70);
    talk(&mut m, 2, 0, 25, 70);
    m.run_for(Duration::from_millis(200));
    assert_eq!(heard(&m, 2, 0), 25);
    assert_eq!(heard(&m, 0, 2), 25);
    assert_eq!(heard(&m, 1, 0) + heard(&m, 1, 2), 50);
}

#[test]
fn a_node_cannot_speak_before_the_plan_lists_it_and_can_soon_after_joining() {
    let mut m = gathered(4, 53);
    settle(&mut m, 3 * SECOND);
    m.join(9, 2);
    assert!(!m.speak(9, 0, 70), "not in any plan yet");
    let took = settle(&mut m, 2 * SECOND);
    assert!(took <= Duration::from_millis(300), "took {took:?}");
    talk(&mut m, 9, 1, 10, 70);
    m.run_for(Duration::from_millis(200));
    for listener in 0..4 {
        assert_eq!(heard(&m, listener, 9), 10, "node {listener}");
    }
    // And the joiner hears the others.
    talk(&mut m, 0, 0, 10, 70);
    m.run_for(Duration::from_millis(200));
    assert_eq!(heard(&m, 9, 0), 10);
}

#[test]
fn with_more_talkers_than_n_a_listener_hears_the_loudest_three() {
    let mut m = gathered(8, 59);
    settle(&mut m, 3 * SECOND);
    assert_eq!(m.settled().unwrap().audio_top_n, 3);
    // Four talk at once for two seconds; node 4 is the quietest.
    for frame in 0..100 {
        for (speaker, level) in [(1, 90), (2, 85), (3, 80), (4, 40)] {
            assert!(m.speak(speaker, frame, level));
        }
        m.run_for(Duration::from_millis(20));
    }
    m.run_for(Duration::from_millis(200));
    // In the second half every listener hears exactly the loudest three, whole.
    let in_second_half = |m: &Meeting, listener: u32, speaker: u32| {
        let alias = m.microphone_alias(listener, speaker).unwrap();
        m.heard_by(listener)
            .iter()
            .filter(|(a, frame)| *a == alias && *frame >= 50)
            .count()
    };
    for listener in [0, 5, 6, 7] {
        for speaker in [1, 2, 3] {
            assert_eq!(
                in_second_half(&m, listener, speaker),
                50,
                "{listener} of {speaker}"
            );
        }
        assert_eq!(
            in_second_half(&m, listener, 4),
            0,
            "{listener} of the quiet one"
        );
    }
    // Getting there takes time when all four start at the same instant. Slots go to whoever
    // is heard first, a khef holds a selection for 250 ms before giving it up, and each
    // khef on the way can only choose among what the one before it passed on. So the third
    // loudest can be late by a hold time per hop.
    let lost: Vec<usize> = [0, 5, 6, 7]
        .iter()
        .map(|listener| 100 - heard(&m, *listener, 3))
        .collect();
    assert!(lost.iter().all(|frames| *frames <= 40), "{lost:?}");
    eprintln!("frames of the third loudest lost at each listener: {lost:?}");
}

#[test]
fn when_a_node_in_the_middle_crashes_audio_resumes_once_the_tree_is_rejoined() {
    let mut m = gathered(12, 61);
    settle(&mut m, 3 * SECOND);
    let plan = m.settled().unwrap().clone();
    let degree = |node: u32| {
        let id = Meeting::id_of(node);
        plan.audio_overlay
            .iter()
            .filter(|e| e.0 == id || e.1 == id)
            .count()
    };
    // A node other than the conductor and the speaker with more than one audio link.
    let middle = (2..12).find(|n| degree(*n) > 1).unwrap();
    let speaker = 1;
    // The speaker talks throughout: one second before, and four after.
    talk(&mut m, speaker, 0, 50, 70);
    m.crash(middle);
    talk(&mut m, speaker, 50, 200, 70);
    m.run_for(Duration::from_millis(200));
    m.settled().unwrap();
    assert_eq!(m.stats().sessions_closed, 0);
    for listener in (0..12).filter(|n| *n != speaker && *n != middle) {
        let got = heard(&m, listener, speaker);
        // Nobody lost more than the time it took to notice and re-plan: about 1.3 s, 65
        // frames. Those not behind the crashed node lost nothing.
        assert!(got >= 250 - 75, "node {listener} heard {got} of 250 frames");
    }
    // And the last second was heard by all, whole.
    let alias = m.microphone_alias(0, speaker).unwrap();
    for listener in (0..12).filter(|n| *n != speaker && *n != middle) {
        let late = m
            .heard_by(listener)
            .iter()
            .filter(|(a, frame)| *a == alias && *frame >= 200)
            .count();
        assert_eq!(late, 50, "node {listener}");
    }
}
