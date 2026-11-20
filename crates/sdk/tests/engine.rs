//! Real nodes, each run by an engine, talking over the in-memory transport: the same node
//! the simulator runs, now with real tasks, real streams and real signatures.
//!
//! Time is tokio's paused clock, which jumps ahead whenever every task is waiting, so
//! these run in milliseconds and the same way every time.

#![allow(clippy::unwrap_used, clippy::panic)]

use bytes::Bytes;
use palaver_khef_core::LinkWatchConfig;
use palaver_net::identity::{Identity, meeting_tag};
use palaver_net::memory::MemoryNetwork;
use palaver_net::transport::Peer;
use palaver_proto::{MediaTrack, ParticipantId, Secret, Signer, Source};
use palaver_sdk::{Engine, EngineConfig, Status};
use std::time::Duration;
use tokio::time::timeout;

/// Long enough for anything a meeting does; virtual, so it costs nothing.
const PATIENCE: Duration = Duration::from_secs(30);

struct Member {
    engine: Engine,
    address: Peer,
}

fn config(name: &str, secret: u8) -> EngineConfig {
    EngineConfig {
        meeting: meeting_tag(&Secret::new([secret; 32])).unwrap(),
        name: name.into(),
        tracks: vec![MediaTrack {
            audio: true,
            source: Source::Main,
            rendition: b"opus".to_vec(),
        }],
        addresses: Vec::new(),
        relay_url: None,
        link_watch: LinkWatchConfig::default(),
    }
}

fn member(network: &MemoryNetwork, name: &str) -> Member {
    let identity = Identity::generate().unwrap();
    let endpoint = network.endpoint(identity.public_key()).unwrap();
    let address = Peer::by_key(identity.public_key());
    Member {
        engine: Engine::start(endpoint, identity, config(name, 7)),
        address,
    }
}

impl Member {
    fn id(&self) -> ParticipantId {
        self.engine.me().clone()
    }

    /// Wait until what this node knows of the meeting satisfies `test`.
    async fn until(&self, what: &str, test: impl Fn(&Status) -> bool) -> Status {
        let mut status = self.engine.status();
        let seen = timeout(PATIENCE, status.wait_for(|s| test(s))).await;
        match seen {
            Ok(Ok(status)) => status.clone(),
            _ => panic!(
                "never saw: {what}; last: {:?}",
                *self.engine.status().borrow()
            ),
        }
    }

    /// Wait until this node's plan lists exactly these members.
    async fn until_roster(&self, members: &[&Member]) -> Status {
        let mut want: Vec<ParticipantId> = members.iter().map(|m| m.id()).collect();
        want.sort();
        self.until("the roster", |status| {
            let mut have = status.roster.clone();
            have.sort();
            have == want
        })
        .await
    }
}

/// A meeting of `size`: the first creates it, the rest join through the first.
async fn meeting(network: &MemoryNetwork, size: usize) -> Vec<Member> {
    let mut members = vec![member(network, "node 0")];
    members[0].engine.create_meeting().await;
    for n in 1..size {
        let joiner = member(network, &format!("node {n}"));
        joiner.engine.join(vec![members[0].address.clone()]).await;
        members.push(joiner);
    }
    let all: Vec<&Member> = members.iter().collect();
    for m in &members {
        m.until_roster(&all).await;
    }
    members
}

#[tokio::test(start_paused = true)]
async fn a_node_alone_is_its_own_meeting() {
    let network = MemoryNetwork::new();
    let a = member(&network, "a");
    a.engine.create_meeting().await;
    let status = a.until_roster(&[&a]).await;
    assert!(status.conductor);
    assert!(status.links.is_empty());
}

#[tokio::test(start_paused = true)]
async fn three_nodes_form_a_meeting_and_agree_on_the_plan() {
    let network = MemoryNetwork::new();
    let members = meeting(&network, 3).await;
    // They settle on one plan, and each is connected to both others.
    let plan = members[0]
        .until("links to both", |s| s.links.len() == 2)
        .await
        .plan;
    for m in &members[1..] {
        let status = m
            .until("the same plan and both links", |s| {
                s.plan == plan && s.links.len() == 2
            })
            .await;
        assert!(!status.conductor);
    }
    assert!(members[0].engine.status().borrow().conductor);
}

#[tokio::test(start_paused = true)]
async fn audio_from_one_is_played_by_the_others_and_not_by_itself() {
    let network = MemoryNetwork::new();
    let mut members = meeting(&network, 3).await;
    for m in &members {
        m.until("links to both", |s| s.links.len() == 2).await;
    }
    // Speak until it gets through: the audio tree is set up just after the plan arrives.
    let frame = Bytes::from_static(&[42; 80]);
    let mut heard = None;
    for group in 0..200 {
        members[1].engine.send_audio(group, 100, frame.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(Some(audio)) =
            timeout(Duration::from_millis(1), members[2].engine.next_audio()).await
        {
            heard = Some(audio);
            break;
        }
    }
    let heard = heard.expect("node 2 never heard node 1");
    assert_eq!(heard.payload, frame);
    assert_eq!(heard.level, 100);
    // The conductor hears it too, under the same alias.
    let also = timeout(PATIENCE, members[0].engine.next_audio())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((also.alias, &also.payload), (heard.alias, &frame));
    // The speaker is not played its own voice.
    let own = timeout(Duration::from_millis(200), members[1].engine.next_audio()).await;
    assert!(own.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_node_that_stops_is_dropped_from_the_plan() {
    let network = MemoryNetwork::new();
    let mut members = meeting(&network, 3).await;
    let gone = members.pop().unwrap();
    gone.engine.stop().await;
    let rest: Vec<&Member> = members.iter().collect();
    for m in &members {
        let status = m.until_roster(&rest).await;
        assert_eq!(status.links.len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_node_with_the_wrong_meeting_secret_does_not_get_into_the_plan() {
    let network = MemoryNetwork::new();
    let members = meeting(&network, 2).await;
    let identity = Identity::generate().unwrap();
    let endpoint = network.endpoint(identity.public_key()).unwrap();
    let stranger = Engine::start(endpoint, identity, config("stranger", 8));
    stranger.join(vec![members[0].address.clone()]).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    for m in &members {
        assert_eq!(m.engine.status().borrow().roster.len(), 2);
    }
    assert!(stranger.status().borrow().plan.is_none());
}

#[tokio::test(start_paused = true)]
async fn joining_through_a_node_that_is_not_there_keeps_trying_the_next() {
    let network = MemoryNetwork::new();
    let members = meeting(&network, 2).await;
    let nobody = Peer::by_key(Identity::generate().unwrap().public_key());
    let late = member(&network, "late");
    late.engine
        .join(vec![nobody, members[1].address.clone()])
        .await;
    let all = [&members[0], &members[1], &late];
    late.until_roster(&all).await;
    members[0].until_roster(&all).await;
}
