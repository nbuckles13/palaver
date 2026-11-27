//! A headless participant, or a room of them.
//!
//! ```text
//! palaver-bot [room] [bots] [seconds]     a room in this one process (6 bots, 10 seconds)
//! palaver-bot create [options]            start a meeting on the network; prints a ticket
//! palaver-bot join <ticket> [options]     join one
//!
//! A bot that cannot get in by its ticket prints `reply <ticket>`: a ticket with itself
//! in it. Given to a bot in the meeting as a line on its standard input, that bot dials
//! back, and with both ends dialling the two get through their routers.
//! palaver-bot stun-server <address>       answer STUN questions there, for tests
//!
//! options:
//!   --bind <address>        where to listen (default 0.0.0.0:0)
//!   --advertise <address>   where to tell the meeting it is; may be repeated
//!                           (default: the address it is bound to)
//!   --speaker <n>           which made-up voice it has (default 1)
//!   --silent                only listen
//!   --pattern               offer a camera: a moving pattern, coded while watched
//!   --seconds <n>           how long to stay (default: until stopped)
//!   --stun <address>        a STUN server to ask for this node's public address; may be
//!                           repeated (default: two public servers). A node asks only
//!                           if it has no public address of its own, and only when it
//!                           makes a ticket: at create, or when it cannot get in
//!   --no-stun               ask no STUN server
//!   --dial-seconds <n>      how long one attempt to connect may take (default 10)
//! ```
//!
//! A room runs over the in-memory transport and touches no network: the first bot only
//! listens; the others take turns talking, at different loudness. A bot on the network
//! runs over QUIC and says each second who is in the meeting, how many links it has and
//! whose voices it hears:
//!
//! ```text
//! network behind a router
//! ticket palaver:...
//! second 1: roster 2, links 1, hears [2]
//! ```

use palaver_bot::{Bot, BotError, FRAME, Script, entry_points};
use palaver_media::synthetic::{PITCHES, Rhythm};
use palaver_net::memory::MemoryNetwork;
use palaver_net::quic::{BindError, QuicOptions};
use palaver_net::reach::{DEFAULT_STUN_SERVERS, Network};
use palaver_net::stun::{TransactionId, binding_response, is_stun};
use palaver_proto::{Secret, Ticket};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

async fn room(count: u32, seconds: u64) -> Result<(), BotError> {
    let network = MemoryNetwork::new();
    let secret = fresh_secret()?;

    let mut bots: Vec<Bot> = Vec::new();
    for n in 0..count {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a bot's number, exact up to 2^24, and it only sets a loudness"
        )]
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
                camera: false,
            }
        };
        let bot = Bot::on_memory(&network, &secret, &format!("bot {n}"), script)?;
        match bots.first() {
            None => bot.participant().create_meeting().await,
            Some(first) => {
                bot.participant()
                    .join(vec![first.participant().address().clone()])
                    .await;
            }
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
            let roster = bots.first().map_or(0, |bot| {
                bot.participant().control().status().borrow().roster.len()
            });
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
fn fresh_secret() -> Result<Secret<[u8; 32]>, BotError> {
    let fresh = palaver_net::identity::Identity::generate().map_err(|_| BotError::NoRandomness)?;
    Ok(fresh.secret())
}

/// What a bot on the network was asked to do.
#[derive(Debug, Default)]
struct Options {
    bind: Option<SocketAddr>,
    advertise: Vec<SocketAddr>,
    speaker: Option<u32>,
    silent: bool,
    pattern: bool,
    seconds: Option<u64>,
    stun: Vec<String>,
    no_stun: bool,
    dial_seconds: Option<u64>,
}

fn options(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or_else(|| format!("{arg} needs {what}"));
        match arg.as_str() {
            "--bind" => {
                let text = value("an address")?;
                options.bind = Some(
                    text.parse()
                        .map_err(|_| format!("not an address: {text}"))?,
                );
            }
            "--advertise" => {
                let text = value("an address")?;
                let address = text
                    .parse()
                    .map_err(|_| format!("not an address: {text}"))?;
                options.advertise.push(address);
            }
            "--speaker" => {
                let text = value("a number")?;
                options.speaker = Some(text.parse().map_err(|_| format!("not a number: {text}"))?);
            }
            "--seconds" => {
                let text = value("a number")?;
                options.seconds = Some(text.parse().map_err(|_| format!("not a number: {text}"))?);
            }
            "--silent" => options.silent = true,
            "--pattern" => options.pattern = true,
            "--stun" => options.stun.push(value("an address")?),
            "--no-stun" => options.no_stun = true,
            "--dial-seconds" => {
                let text = value("a number")?;
                options.dial_seconds =
                    Some(text.parse().map_err(|_| format!("not a number: {text}"))?);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(options)
}

/// One bot on the network: it creates a meeting, or joins the one `ticket` is for.
async fn networked(ticket: Option<Ticket>, options: Options) -> Result<(), BotError> {
    let speaker = options.speaker.unwrap_or(1);
    let script = if options.silent {
        Script::silent(speaker)
    } else {
        Script::talking(speaker, 0.4)
    };
    let (script, camera) = if options.pattern {
        (script.filmed(), true)
    } else {
        (script, false)
    };
    let secret = match &ticket {
        Some(ticket) => Secret::new(*ticket.secret.expose()),
        None => fresh_secret()?,
    };
    let quic = QuicOptions {
        bind: options.bind.unwrap_or_else(|| QuicOptions::default().bind),
        dial_timeout: options
            .dial_seconds
            .map_or_else(|| QuicOptions::default().dial_timeout, Duration::from_secs),
        ..QuicOptions::default()
    };
    let name = format!("bot {speaker}");
    let mut bot = Bot::on_quic(quic, options.advertise, &secret, &name, script)?;

    // Whom to ask for this node's public address, when the time comes. Their names are
    // looked up only then: a lookup can take seconds, and most runs never need one.
    let names = if options.no_stun {
        Vec::new()
    } else if options.stun.is_empty() {
        DEFAULT_STUN_SERVERS.map(String::from).to_vec()
    } else {
        options.stun
    };

    if let Some(ticket) = &ticket {
        bot.participant().join(entry_points(ticket)).await;
    } else {
        // An invitation is about to be made: now is when the public address matters.
        say_network(bot.participant_mut().find_network_asking(&names).await);
        bot.participant().create_meeting().await;
        // The ticket holds the meeting secret; showing it is the point of this line.
        println!(
            "ticket {}",
            bot.participant().ticket(&secret).to_text().expose()
        );
    }

    // Tickets sent back by those who could not get in arrive as lines on standard input.
    let (paste, mut pasted) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            if paste.send(line).is_err() {
                return;
            }
        }
    });

    let mut beat = tokio::time::interval(FRAME);
    let frames = options.seconds.map(|seconds| seconds.saturating_mul(50));
    let mut frame = 0u64;
    let mut replied = false;
    while frames.is_none_or(|frames| frame < frames) {
        tokio::select! {
            _ = beat.tick() => {}
            Some(line) = pasted.recv() => {
                match Ticket::parse(line.trim()) {
                    Ok(back) => {
                        println!("reaching for whoever sent a ticket back");
                        bot.participant().reach(&back).await;
                    }
                    Err(error) => eprintln!("not a ticket: {error:?}"),
                }
                continue;
            }
        }
        bot.tick().map_err(BotError::Codec)?;
        let nobody_answers = bot.participant().control().status().borrow().nobody_answers;
        if let (true, false, Some(invitation)) = (nobody_answers, replied, &ticket) {
            // It cannot get in on its own. Its public address is wanted now, for the
            // ticket that goes back to whoever invited it.
            replied = true;
            say_network(bot.participant_mut().find_network_asking(&names).await);
            println!(
                "reply {}",
                bot.participant().reply_to(invitation).to_text().expose()
            );
        }
        if frame % 50 == 49 {
            say_second(&bot, frame / 50 + 1, camera);
        }
        frame = frame.saturating_add(1);
    }
    bot.stop().await;
    Ok(())
}

/// What a bot on the network says each second.
fn say_second(bot: &Bot, second: u64, camera: bool) {
    let (roster, links) = {
        let status = bot.participant().control().status();
        let status = status.borrow();
        (status.roster.len(), status.links.len())
    };
    let filmed = if camera {
        format!(", pictures sent {}", bot.filmed())
    } else {
        String::new()
    };
    println!(
        "second {second}: roster {roster}, links {links}, hears {:?}{filmed}",
        bot.hears_among(0..PITCHES)
    );
    if let Some(why) = bot.camera_trouble() {
        eprintln!("camera: {why}");
    }
}

fn say_network(network: Network) {
    println!(
        "network {}",
        match network {
            Network::Open(_) => "open",
            Network::BehindRouter(_) => "behind a router",
            Network::PerDestination => "gives a port per destination",
            Network::Unknown => "unknown",
        }
    );
}

/// Answer every STUN question that arrives at `bind` with the address it came from.
async fn stun_server(bind: SocketAddr) -> Result<(), BotError> {
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .map_err(|error| BotError::Bind(BindError::Socket(error.to_string())))?;
    let mut packet = [0u8; 1500];
    loop {
        let Ok((len, from)) = socket.recv_from(&mut packet).await else {
            continue;
        };
        let question = packet.get(..len).unwrap_or_default();
        if let Some(id) = question
            .get(8..20)
            .and_then(|id| TransactionId::try_from(id).ok())
            && is_stun(question)
        {
            let _ = socket.send_to(&binding_response(&id, from), from).await;
        }
    }
}

/// What the command line asks for.
enum Task {
    StunServer(SocketAddr),
    Room {
        count: u32,
        seconds: u64,
    },
    Networked {
        ticket: Option<Ticket>,
        options: Options,
    },
}

fn task(mut args: impl Iterator<Item = String>) -> Result<Task, String> {
    let first = args.next();
    match first.as_deref() {
        Some("stun-server") => {
            let text = args.next().ok_or("stun-server needs an address")?;
            let bind = text
                .parse()
                .map_err(|_| format!("not an address: {text}"))?;
            Ok(Task::StunServer(bind))
        }
        Some("create") => Ok(Task::Networked {
            ticket: None,
            options: options(args)?,
        }),
        Some("join") => {
            let text = args.next().ok_or("join needs a ticket")?;
            let ticket = Ticket::parse(&text).map_err(|error| format!("bad ticket: {error:?}"))?;
            Ok(Task::Networked {
                ticket: Some(ticket),
                options: options(args)?,
            })
        }
        _ => {
            // `room` may be left out: the numbers alone are the older form.
            let count = if first.as_deref() == Some("room") {
                args.next()
            } else {
                first
            };
            let number = |text: Option<String>, what: &str| match text {
                None => Ok(None),
                Some(text) => text
                    .parse::<u64>()
                    .map(Some)
                    .map_err(|_| format!("{what}: not a number: {text}")),
            };
            let count = number(count, "bots")?.unwrap_or(6).clamp(1, 50);
            let seconds = number(args.next(), "seconds")?.unwrap_or(10);
            Ok(Task::Room {
                count: u32::try_from(count).unwrap_or(50),
                seconds,
            })
        }
    }
}

fn main() -> ExitCode {
    let task = match task(std::env::args().skip(1)) {
        Ok(task) => task,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start: {error}");
            return ExitCode::FAILURE;
        }
    };
    let done = runtime.block_on(async {
        match task {
            Task::StunServer(bind) => stun_server(bind).await,
            Task::Room { count, seconds } => room(count, seconds).await,
            Task::Networked { ticket, options } => networked(ticket, options).await,
        }
    });
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stopped: {error:?}");
            ExitCode::FAILURE
        }
    }
}
