//! Video across a whole meeting: the real nodes, the conductor's plan, simulated links.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test may stop where it stands"
)]

use palaver_sim::NodeId;
use palaver_sim::meeting::Meeting;
use std::time::Duration;

const SECOND: Duration = Duration::from_secs(1);

/// A settled meeting of `nodes`, created by node 0 and joined through it.
fn gathered(nodes: u32, seed: u64) -> Meeting {
    let mut m = Meeting::new(seed).unwrap();
    assert!(m.create(0));
    for node in 1..nodes {
        m.run_for(Duration::from_millis(100));
        assert!(m.join(node, 0));
    }
    settle(&mut m);
    m
}

fn settle(m: &mut Meeting) {
    for _ in 0..400 {
        if m.settled().is_ok() {
            return;
        }
        m.run_for(Duration::from_millis(50));
    }
    panic!("not settled: {}", m.settled().unwrap_err());
}

/// The frames `node` was shown of `of`'s camera: (group, frame).
fn shown(m: &Meeting, node: NodeId, of: NodeId) -> Vec<(u64, Vec<u8>)> {
    let alias = m.camera_alias(node, of).unwrap();
    m.seen_by(node)
        .iter()
        .filter(|(a, _, _)| *a == alias)
        .map(|(_, group, frame)| (*group, frame.clone()))
        .collect()
}

/// The nodes joined to `node` by the audio tree, in the plan it follows.
fn beside(m: &Meeting, node: NodeId) -> Vec<NodeId> {
    let me = Meeting::id_of(node);
    m.plan_of(node)
        .unwrap()
        .audio_overlay
        .iter()
        .filter_map(|edge| {
            if edge.0 == me {
                m.node_of(&edge.1)
            } else if edge.1 == me {
                m.node_of(&edge.0)
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn a_camera_is_made_only_while_someone_in_the_meeting_watches_it() {
    let mut m = gathered(6, 11);
    m.run_for(SECOND);
    assert!((0..6).all(|node| !m.filming(node)), "nobody watches yet");
    assert!(m.watch(5, 2, true));
    m.run_for(SECOND);
    let filming: Vec<NodeId> = (0..6).filter(|node| m.filming(*node)).collect();
    assert_eq!(filming, [2]);
    assert!(m.watch(5, 2, false));
    m.run_for(SECOND);
    assert!(!m.filming(2));
}

#[test]
fn every_node_can_watch_every_other_and_sees_whole_frames_in_order() {
    let mut m = gathered(6, 12);
    for watcher in 0..6 {
        for of in (0..6).filter(|of| *of != watcher) {
            assert!(m.watch(watcher, of, true));
        }
    }
    m.run_for(SECOND);
    assert!((0..6).all(|node| m.filming(node)));
    for of in 0..6 {
        let big = vec![u8::try_from(of).unwrap(); 30_000];
        assert!(m.film(of, 0, &[&big, b"delta one"], false));
        m.run_for(Duration::from_millis(33));
        assert!(m.film(of, 0, &[b"delta two"], true));
    }
    m.run_for(SECOND);
    for watcher in 0..6 {
        for of in (0..6).filter(|of| *of != watcher) {
            let got = shown(&m, watcher, of);
            let big = vec![u8::try_from(of).unwrap(); 30_000];
            assert_eq!(
                got,
                [
                    (0, big),
                    (0, b"delta one".to_vec()),
                    (0, b"delta two".to_vec())
                ],
                "{watcher} watching {of}"
            );
        }
        assert_eq!(m.seen_by(watcher).len(), 15, "and nothing of its own");
    }
    assert_eq!(m.stats().sessions_closed, 0);
}

#[test]
fn a_later_watcher_costs_one_key_frame_and_then_sees_the_picture() {
    let mut m = gathered(5, 13);
    assert!(m.watch(1, 0, true));
    m.run_for(SECOND);
    assert!(m.film(0, 0, &[b"key"], false));
    m.run_for(SECOND);
    assert_eq!(m.key_frames_asked(0), 0);
    assert!(m.watch(2, 0, true));
    assert!(m.watch(3, 0, true));
    m.run_for(SECOND);
    assert_eq!(m.key_frames_asked(0), 1, "one for the two of them");
    assert_eq!(shown(&m, 2, 0), [], "the group under way is not theirs");
    assert!(m.film(0, 1, &[b"key again"], true));
    m.run_for(SECOND);
    for watcher in [1, 2, 3] {
        assert_eq!(
            shown(&m, watcher, 0).last(),
            Some(&(1, b"key again".to_vec())),
            "{watcher}"
        );
    }
}

#[test]
fn when_a_node_on_the_way_crashes_the_picture_returns_by_the_new_plan() {
    let mut m = gathered(6, 14);
    let conductor = m.node_of(&m.plan_of(0).unwrap().conductor).unwrap();
    // A node in the middle of the tree that is not the conductor, and two it joins.
    let middle = (0..6)
        .find(|node| *node != conductor && beside(&m, *node).len() >= 2)
        .unwrap();
    let ends = beside(&m, middle);
    let (watcher, of) = (ends[0], ends[1]);
    assert!(m.watch(watcher, of, true));
    m.run_for(SECOND);
    assert!(m.film(of, 0, &[b"before"], true));
    m.run_for(SECOND);
    assert_eq!(shown(&m, watcher, of), [(0, b"before".to_vec())]);

    m.crash(middle);
    m.run_for(5 * SECOND);
    settle(&mut m);
    m.run_for(2 * SECOND);
    assert!(m.filming(of), "the watching is taken up again by itself");
    assert!(m.film(of, 1, &[b"after"], true));
    m.run_for(SECOND);
    assert_eq!(shown(&m, watcher, of).last(), Some(&(1, b"after".to_vec())));
}
