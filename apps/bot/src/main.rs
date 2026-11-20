//! Fills a room with bots in this one process and says, second by second, who each hears.
//!
//! ```text
//! palaver-bot [bots] [seconds]      (defaults: 6 bots, 10 seconds)
//! ```
//!
//! The first bot only listens; the others take turns talking, at different loudness. It
//! runs in real time over the in-memory transport: no network is touched. (Joining a real
//! meeting needs the QUIC transport, which is to come.)

use palaver_bot::{Bot, BotError, FRAME, Script};
use palaver_media::synthetic::Rhythm;
use palaver_net::memory::MemoryNetwork;
use palaver_proto::Secret;
use std::process::ExitCode;
use std::time::Duration;

async fn room(count: u32, seconds: u64) -> Result<(), BotError> {
    let network = MemoryNetwork::new();
    let mut secret = [0u8; 32];
    getrandom_fill(&mut secret)?;
    let secret = Secret::new(secret);

    let mut bots: Vec<Bot> = Vec::new();
    for n in 0..count {
        let script = if n == 0 {
            Script::silent(0)
        } else {
            // Each talks for three seconds in every eight, starting a second after the last.
            Script {
                speaker: n,
                loudness: 0.5 / (1.0 + n as f32 * 0.3),
                rhythm: Rhythm {
                    talk: Duration::from_secs(3),
                    pause: Duration::from_secs(5),
                    offset: Duration::from_secs(u64::from(8 - n % 8)),
                },
            }
        };
        let bot = Bot::on_memory(&network, &secret, &format!("bot {n}"), script)?;
        match bots.first() {
            None => bot.create_meeting().await,
            Some(first) => bot.join(vec![first.address().clone()]).await,
        }
        bots.push(bot);
    }

    let mut beat = tokio::time::interval(FRAME);
    for frame in 0..seconds.saturating_mul(50) {
        beat.tick().await;
        for bot in &mut bots {
            bot.tick().map_err(BotError::Codec)?;
        }
        if frame % 50 == 49 {
            let second = frame / 50 + 1;
            let roster = bots
                .first()
                .map_or(0, |bot| bot.engine().status().borrow().roster.len());
            println!("second {second}: {roster} in the meeting");
            for (n, bot) in bots.iter().enumerate() {
                println!("  bot {n} hears {:?}", bot.hears_among(0..count));
            }
        }
    }
    for bot in bots {
        bot.stop().await;
    }
    Ok(())
}

/// A meeting secret for this run.
fn getrandom_fill(secret: &mut [u8; 32]) -> Result<(), BotError> {
    let fresh = palaver_net::identity::Identity::generate().map_err(|_| BotError::NoRandomness)?;
    *secret = *fresh.secret().expose();
    Ok(())
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let count = args.next().and_then(|a| a.parse().ok()).unwrap_or(6u32);
    let seconds = args.next().and_then(|a| a.parse().ok()).unwrap_or(10u64);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(room(count.clamp(1, 50), seconds)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stopped: {error:?}");
            ExitCode::FAILURE
        }
    }
}
