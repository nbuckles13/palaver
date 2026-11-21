//! Rooms of bots in one process: real nodes, real Opus, real jitter buffers, over the
//! in-memory transport. What is checked is what a listener would notice.
//!
//! Time is tokio's paused clock, so a few seconds of meeting take a fraction of one.

#![expect(clippy::unwrap_used, reason = "a test may stop where it stands")]

use palaver_bot::{Bot, FRAME, Script};
use palaver_net::memory::MemoryNetwork;
use palaver_proto::Secret;
use std::time::Duration;
use tokio::time::timeout;

/// A room: the first bot creates the meeting, the rest join through it. Returns once
/// every bot's plan lists them all.
#[expect(
    clippy::future_not_send,
    reason = "a bot holds Opus coders, which are not `Sync`; the tests run on a current-thread runtime"
)]
async fn room(scripts: &[Script]) -> Vec<Bot> {
    let network = MemoryNetwork::new();
    let secret = Secret::new([7; 32]);
    let mut bots: Vec<Bot> = Vec::new();
    for (n, script) in scripts.iter().enumerate() {
        let bot = Bot::on_memory(&network, &secret, &format!("bot {n}"), *script).unwrap();
        match bots.first() {
            None => bot.create_meeting().await,
            Some(first) => bot.join(vec![first.address().clone()]).await,
        }
        bots.push(bot);
    }
    for bot in &bots {
        let mut status = bot.engine().status();
        let settled = status
            .wait_for(|s| s.roster.len() == scripts.len() && s.microphones.len() == scripts.len());
        assert!(
            timeout(Duration::from_secs(30), settled).await.is_ok(),
            "the room never formed: {:?}",
            *bot.engine().status().borrow()
        );
    }
    bots
}

/// Let every bot talk and listen for `seconds`, all on the same 20 ms beat.
async fn run(bots: &mut [Bot], seconds: u64) {
    for _ in 0..seconds * 50 {
        for bot in bots.iter_mut() {
            bot.tick().unwrap();
        }
        tokio::time::sleep(FRAME).await;
    }
}

#[tokio::test(start_paused = true)]
async fn one_talks_and_everyone_else_hears_that_voice_and_no_other() {
    let scripts = [
        Script::silent(0),
        Script::talking(1, 0.5),
        Script::silent(2),
        Script::silent(3),
    ];
    let mut bots = room(&scripts).await;
    run(&mut bots, 3).await;
    for (n, bot) in bots.iter().enumerate() {
        let heard = bot.hears_among(0..4);
        if n == 1 {
            assert_eq!(heard, [], "the talker hears nobody, itself included");
        } else {
            assert_eq!(heard, [1], "bot {n}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn when_five_talk_a_listener_hears_the_loudest_three() {
    // Loudest to quietest: 1, 3, 5, 4, 2. The third, fourth and fifth are 10 dB apart. A
    // khef ranks each voice by its latest packet, and a voice's level moves several
    // decibels from one packet to the next as syllables rise and fall, so voices closer
    // than that can swap places for a moment, and the one that gets in then stays for the
    // hold.
    let scripts = [
        Script::silent(0),
        Script::talking(1, 0.6),
        Script::talking(2, 0.03),
        Script::talking(3, 0.45),
        Script::talking(4, 0.1),
        Script::talking(5, 0.35),
    ];
    let mut bots = room(&scripts).await;
    run(&mut bots, 3).await;
    assert_eq!(bots[0].hears_among(0..6), [1, 3, 5]);
    // One of the loud ones does not hear itself, so its third voice is the next loudest.
    assert_eq!(bots[1].hears_among(0..6), [3, 4, 5]);
    // A quiet one hears the same three as the listener.
    assert_eq!(bots[2].hears_among(0..6), [1, 3, 5]);
}

#[tokio::test(start_paused = true)]
async fn a_voice_that_stops_is_no_longer_heard_and_one_that_starts_is() {
    // Bot 1 talks for two seconds and pauses for two; bot 2 does the opposite.
    let turn = |speaker, offset| Script {
        speaker,
        loudness: 0.4,
        rhythm: palaver_media::synthetic::Rhythm {
            talk: Duration::from_secs(2),
            pause: Duration::from_secs(2),
            offset: Duration::from_secs(offset),
        },
    };
    let mut bots = room(&[Script::silent(0), turn(1, 0), turn(2, 2)]).await;
    run(&mut bots, 2).await;
    assert_eq!(bots[0].hears_among(0..3), [1]);
    run(&mut bots, 2).await;
    assert_eq!(bots[0].hears_among(0..3), [2]);
    run(&mut bots, 2).await;
    assert_eq!(bots[0].hears_among(0..3), [1]);
}

#[tokio::test(start_paused = true)]
async fn a_bot_that_leaves_is_dropped_and_the_rest_carry_on() {
    let scripts = [
        Script::silent(0),
        Script::talking(1, 0.5),
        Script::silent(2),
        Script::talking(3, 0.4),
    ];
    let mut bots = room(&scripts).await;
    run(&mut bots, 2).await;
    assert_eq!(bots[0].hears_among(0..4), [1, 3]);
    bots.pop().unwrap().stop().await;
    run(&mut bots, 3).await;
    assert_eq!(bots[0].hears_among(0..4), [1]);
    assert_eq!(bots[2].hears_among(0..4), [1]);
    assert_eq!(bots[0].engine().status().borrow().roster.len(), 3);
}
