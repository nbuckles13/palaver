//! Bots over QUIC on the loopback interface, in real time: a ticket as text is all a
//! joiner is given, and a listener hears the talker.

#![allow(clippy::unwrap_used, clippy::panic)]

use palaver_bot::{Bot, FRAME, Script, entry_points};
use palaver_net::quic::QuicOptions;
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
    first.create_meeting().await;
    // The ticket crosses as text, as it would through a chat program.
    let text = first.ticket(&Secret::new([7; 32])).to_text();
    let ticket = Ticket::parse(text.expose()).unwrap();
    let talker = bot(&ticket.secret, Script::talking(1, 0.4));
    talker.join(entry_points(&ticket)).await;
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
}
