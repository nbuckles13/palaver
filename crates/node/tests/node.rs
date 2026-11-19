//! What a node does that needs no bytes to cross a link. Whole meetings of nodes, joined by
//! simulated links, are in `palaver-sim`.
#![allow(clippy::unwrap_used)]

use palaver_khef_core::{LinkWatchConfig, Role};
use palaver_moq_wire::Setup;
use palaver_node::{Config, Node, Output};
use palaver_proto::sign::fake::{FakeIdentity, FakeVerifier};
use palaver_proto::{
    MediaTrack, MeetingTag, ParticipantId, PathKind, Redacted, Signer, Source, Timestamp, Verifier,
};
use std::time::Duration;

fn at(ms: u64) -> Timestamp {
    Timestamp::from_millis(ms)
}

fn id_of(seed: u8) -> ParticipantId {
    FakeVerifier.participant_id(&FakeIdentity(seed).public_key())
}

fn node(seed: u8) -> Node<FakeIdentity, FakeVerifier> {
    let config = Config {
        meeting: MeetingTag::new(b"m-7f3a").unwrap(),
        name: Redacted::new(format!("node {seed}")),
        first_sequence: 1,
        setup: Setup::default(),
        link_watch: LinkWatchConfig::default(),
        tracks: vec![MediaTrack {
            audio: true,
            source: Source::Main,
            rendition: b"opus".to_vec(),
        }],
        addresses: vec![],
        relay_url: None,
    };
    Node::new(config, FakeIdentity(seed), FakeVerifier)
}

fn outputs(node: &mut Node<FakeIdentity, FakeVerifier>) -> Vec<Output> {
    core::iter::from_fn(|| node.poll_output()).collect()
}

fn dial(seed: u8) -> Output {
    Output::Dial { peer: id_of(seed) }
}

#[test]
fn the_creator_is_conductor_with_a_plan_of_one_and_an_alias_for_its_microphone() {
    let mut n = node(1);
    assert_eq!(n.next_wake(at(0)), None, "nothing to do before it starts");
    n.create_meeting(at(0));
    assert!(n.is_conductor());
    let plan = n.plan().unwrap();
    assert_eq!(plan.roster.len(), 1);
    assert_eq!(plan.roster[0].tracks.len(), 1);
    assert_eq!(outputs(&mut n), [], "nobody to send anything to");
    // It can speak, to nobody.
    let mut datagram = vec![];
    let out = n
        .local_audio(0, 60, &[1; 80], &mut datagram, at(0))
        .unwrap();
    assert!(out.forward_to.is_empty() && !out.for_client);
    assert!(n.next_wake(at(0)).is_some());
}

#[test]
fn a_joiner_says_nothing_and_cannot_speak_until_it_is_connected() {
    let mut n = node(2);
    n.join(vec![id_of(1)], at(0));
    assert_eq!(outputs(&mut n), [dial(1)]);
    n.poll(at(5000));
    assert_eq!(outputs(&mut n), []);
    assert_eq!(n.plan(), None);
    let mut datagram = vec![];
    assert_eq!(n.local_audio(0, 60, &[1; 80], &mut datagram, at(0)), None);
    assert!(datagram.is_empty());
    assert_eq!(n.next_wake(at(5000)), None);
}

#[test]
fn a_joiner_tries_its_entry_points_in_order() {
    let mut n = node(5);
    n.join(vec![id_of(1), id_of(2), id_of(3)], at(0));
    assert_eq!(outputs(&mut n), [dial(1)], "one at a time");
    n.dial_failed(&id_of(1), at(500));
    assert_eq!(outputs(&mut n), [dial(2)]);
    n.dial_failed(&id_of(2), at(1000));
    assert_eq!(outputs(&mut n), [dial(3)]);
    n.dial_failed(&id_of(3), at(1500));
    assert_eq!(outputs(&mut n), [], "nowhere left to try");
    // A failure it did not ask about changes nothing.
    n.dial_failed(&id_of(9), at(1600));
    assert_eq!(outputs(&mut n), []);
}

#[test]
fn once_connected_a_joiner_opens_a_session_and_stops_dialling() {
    let mut n = node(5);
    n.join(vec![id_of(1), id_of(2)], at(0));
    outputs(&mut n);
    n.link_up(
        id_of(1),
        Role::Client,
        PathKind::Direct,
        Duration::from_millis(4),
        at(30),
    );
    let out = outputs(&mut n);
    assert!(
        out.iter()
            .all(|o| matches!(o, Output::Stream { peer, .. } if *peer == id_of(1))),
        "{out:?}"
    );
    assert!(!out.is_empty());
    assert_eq!(n.links().collect::<Vec<_>>(), [&id_of(1)]);
    // The same link again, or a link to itself, is ignored.
    n.link_up(
        id_of(1),
        Role::Client,
        PathKind::Direct,
        Duration::ZERO,
        at(31),
    );
    n.link_up(
        id_of(5),
        Role::Client,
        PathKind::Direct,
        Duration::ZERO,
        at(31),
    );
    assert_eq!(n.links().count(), 1);
}

#[test]
fn a_joiner_whose_only_link_goes_silent_hangs_up_and_tries_the_next_entry_point() {
    let mut n = node(5);
    n.join(vec![id_of(1), id_of(2)], at(0));
    n.link_up(
        id_of(1),
        Role::Client,
        PathKind::Direct,
        Duration::from_millis(4),
        at(0),
    );
    outputs(&mut n);
    // Nothing is ever heard from node 1.
    let mut seen = vec![];
    for ms in (50..=800).step_by(50) {
        n.poll(at(ms));
        seen.extend(outputs(&mut n));
    }
    let fillers = seen
        .iter()
        .filter(|o| matches!(o, Output::Datagram { .. }))
        .count();
    assert_eq!(fillers, 3, "at 200, 400 and 600 ms");
    let rest: Vec<&Output> = seen
        .iter()
        .filter(|o| !matches!(o, Output::Datagram { .. }))
        .collect();
    assert_eq!(rest, [&Output::Hangup { peer: id_of(1) }, &dial(2)]);
    assert_eq!(n.links().count(), 0);
}
