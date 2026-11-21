//! Engines over the QUIC transport on the loopback interface: real sockets and real time.
//! The one thing checked here is what the in-memory transport cannot show: that a node
//! finds the others at the addresses their reports give, knowing at first only the one it
//! joined through.

#![expect(clippy::unwrap_used, reason = "a test may stop where it stands")]

use palaver_khef_core::LinkWatchConfig;
use palaver_net::admission::{AdmissionKey, Admitted};
use palaver_net::identity::{Identity, meeting_tag};
use palaver_net::quic::{QuicEndpoint, QuicOptions};
use palaver_net::transport::Peer;
use palaver_proto::{MediaTrack, ParticipantId, Redacted, Secret, Signer, Source};
use palaver_sdk::{Engine, EngineConfig};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::time::timeout;

const PATIENCE: Duration = Duration::from_millis(1500);

struct Member {
    engine: Engine,
    address: Peer,
}

/// A node on the loopback interface, which tells the meeting its address or does not.
fn member(name: &str, advertise: bool) -> Member {
    let secret = Secret::new([7; 32]);
    let identity = Identity::generate().unwrap();
    let endpoint = QuicEndpoint::bind(
        &identity,
        QuicOptions {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            dial_timeout: Duration::from_millis(500),
            ..QuicOptions::default()
        },
    )
    .unwrap();
    let at = endpoint.local_address().unwrap();
    let address = Peer {
        key: identity.public_key(),
        addresses: vec![Redacted::new(at)],
    };
    let config = EngineConfig {
        meeting: meeting_tag(&secret).unwrap(),
        name: name.into(),
        tracks: vec![MediaTrack {
            audio: true,
            source: Source::Main,
            rendition: b"opus".to_vec(),
        }],
        addresses: if advertise { vec![at] } else { Vec::new() },
        link_watch: LinkWatchConfig::default(),
    };
    let admitted = Admitted::new(
        endpoint,
        AdmissionKey::from_meeting_secret(&secret).unwrap(),
    );
    Member {
        engine: Engine::start(admitted, identity, config),
        address,
    }
}

/// Four nodes, the last three joining through the first and knowing only its address.
/// Every node is given two peers at least, so each joiner must find another joiner.
async fn joiners_find_each_other(advertise: bool) {
    let members: Vec<Member> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|name| member(name, advertise))
        .collect();
    members[0].engine.create_meeting().await;
    // Each joiner knows only where the first is.
    for joiner in &members[1..] {
        joiner.engine.join(vec![members[0].address.clone()]).await;
    }
    let mut everyone: Vec<ParticipantId> = members.iter().map(|m| m.engine.me().clone()).collect();
    everyone.sort();
    for member in &members {
        let mut status = member.engine.status();
        let seen = timeout(
            PATIENCE,
            status.wait_for(|s| {
                let mut roster = s.roster.clone();
                roster.sort();
                // Every node is given two peers at least, and a joiner met only the
                // first through the ticket: its second it found by a report.
                roster == everyone && s.links.len() >= 2
            }),
        )
        .await
        .is_ok_and(|waited| waited.is_ok());
        assert!(seen, "last: {:?}", *member.engine.status().borrow());
    }
}

#[tokio::test]
async fn nodes_that_joined_through_one_find_each_other_by_their_reports() {
    joiners_find_each_other(true).await;
}

#[tokio::test]
async fn a_node_that_gives_no_address_is_found_where_its_neighbours_see_it() {
    joiners_find_each_other(false).await;
}
