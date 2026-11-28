//! Bots over QUIC on the loopback interface, in real time: a ticket as text is all a
//! joiner is given, and a listener hears the talker.

#![expect(clippy::unwrap_used, reason = "a test may stop where it stands")]

use palaver_bot::{Bot, FRAME, Script, entry_points};
use palaver_net::quic::QuicOptions;
use palaver_net::reach::{Network, StunConfig, StunMode};
use palaver_net::stun::binding_response;
use palaver_proto::{Secret, Ticket};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

fn bot(secret: &Secret<[u8; 32]>, script: Script) -> Bot {
    let options = QuicOptions {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        dial_timeout: Duration::from_millis(500),
        ..QuicOptions::default()
    };
    Bot::on_quic(options, Vec::new(), secret, "bot", script).unwrap()
}

#[tokio::test]
async fn a_bot_given_only_a_ticket_joins_and_hears_the_one_who_talks() {
    let first = bot(&Secret::new([7; 32]), Script::silent(0));
    first.participant().create_meeting().await;
    // The ticket crosses as text, as it would through a chat program.
    let text = first.participant().ticket(&Secret::new([7; 32])).to_text();
    let ticket = Ticket::parse(text.expose()).unwrap();
    let talker = bot(&ticket.secret, Script::talking(1, 0.4));
    talker.participant().join(entry_points(&ticket)).await;
    let mut bots = [first, talker];

    let mut beat = tokio::time::interval(FRAME);
    // Three seconds at the most; a second of sound is needed to tell a voice.
    for _ in 0..150 {
        beat.tick().await;
        for bot in &mut bots {
            bot.tick().unwrap();
        }
        if bots[0].hears(1) {
            break;
        }
    }
    assert!(bots[0].hears(1), "strength {}", bots[0].strength_of(1));
    assert!(!bots[0].hears(2));
    assert!(!bots[1].hears(1), "a bot does not hear itself");
    // Each counts what its one link has carried, each way.
    for bot in &bots {
        let links = bot.participant().control().links().await;
        assert_eq!(links.len(), 1);
        assert!(links[0].bytes_sent > 0 && links[0].bytes_received > 0);
    }
}

/// A STUN server on the loopback interface that says a question came from `shift` ports
/// above where it did, as it would look through a router.
async fn stun_server(shift: u16) -> SocketAddr {
    let socket = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut packet = [0u8; 1500];
        while let Ok((len, from)) = socket.recv_from(&mut packet).await {
            if len < 20 {
                continue;
            }
            let id = <[u8; 12]>::try_from(&packet[8..20]).unwrap();
            let seen = SocketAddr::new(from.ip(), from.port().wrapping_add(shift));
            let _ = socket.send_to(&binding_response(&id, seen), from).await;
        }
    });
    address
}

const fn asking(servers: Vec<SocketAddr>, mode: StunMode) -> StunConfig {
    StunConfig {
        mode,
        servers,
        patience: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn a_bot_finds_what_network_it_is_on_and_its_ticket_gains_its_public_address() {
    let secret = Secret::new([7; 32]);
    let (plain, shifted, other) = (
        stun_server(0).await,
        stun_server(5).await,
        stun_server(9).await,
    );

    // Seen where it is: nothing stands between.
    let mut open = bot(&secret, Script::silent(0));
    let own = *open.participant().address().addresses[0].reveal();
    let found = open
        .participant_mut()
        .find_network(&asking(vec![plain], StunMode::WhenNeeded))
        .await;
    assert_eq!(found, Network::Open(own));
    assert_eq!(
        open.participant().ticket(&secret).entry_points[0]
            .addresses
            .len(),
        1
    );

    // Seen elsewhere, the same by all who are asked: a router, and the ticket says where.
    let mut behind = bot(&secret, Script::silent(0));
    let own = *behind.participant().address().addresses[0].reveal();
    let public = SocketAddr::new(own.ip(), own.port().wrapping_add(5));
    let found = behind
        .participant_mut()
        .find_network(&asking(vec![shifted], StunMode::WhenNeeded))
        .await;
    assert_eq!(found, Network::BehindRouter(public));
    let listed: Vec<SocketAddr> = behind.participant().ticket(&secret).entry_points[0]
        .addresses
        .iter()
        .map(|a| *a.reveal())
        .collect();
    assert_eq!(listed, [own, public]);

    // Seen differently by each: no address to hand out.
    let mut symmetric = bot(&secret, Script::silent(0));
    let found = symmetric
        .participant_mut()
        .find_network(&asking(vec![shifted, other], StunMode::WhenNeeded))
        .await;
    assert_eq!(found, Network::PerDestination);
    assert_eq!(
        symmetric.participant().ticket(&secret).entry_points[0]
            .addresses
            .len(),
        1
    );

    // Told never to ask, it does not, and knows nothing.
    let mut private = bot(&secret, Script::silent(0));
    let found = private
        .participant_mut()
        .find_network(&asking(vec![shifted], StunMode::Never))
        .await;
    assert_eq!(found, Network::Unknown);
}
