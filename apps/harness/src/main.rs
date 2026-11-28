//! Palaver with a sound card: a window, or a call from the command line.
//!
//! ```text
//! palaver-harness                           the window
//! palaver-harness window [--name <name>] [--ticket-file <path>]
//!                        [--floor <frames>] [--margin <frames>] [--lead <ms>]
//!                        [--noise <strength>] [--pattern] [--coded <kbit/s>]
//!                                           the window, with the name filled in, and a
//!                                           file that carries the ticket: written when
//!                                           a meeting is started, read into the join
//!                                           field when the window opens
//!                                           --pattern shows a moving pattern as this
//!                                           machine's own picture, in place of a camera;
//!                                           --coded shows it also as it would be sent,
//!                                           encoded at that rate and decoded again
//! palaver-harness shot <file.png> [name [pattern|coded|plain [ticket-file|- [tab]]]]
//!                                           a picture of the window, taken by itself:
//!                                           of the first screen, or with a name, of a
//!                                           meeting started under it, or joined by
//!                                           the ticket in the file; with a tab
//!                                           (audio, video, network, system), that
//!                                           tab of the details
//! palaver-harness create [options]          start a meeting; prints a ticket to hand on
//! palaver-harness join <ticket> [options]   join one
//!
//! options:
//!   --name <name>           the name the others see (default "someone")
//!   --bind <address>        where to listen (default 0.0.0.0:0)
//!   --advertise <address>   where to tell the meeting this node is; may be repeated
//!   --no-stun               ask no STUN server for this node's public address
//!   --ticket-file <path>    with create: also write the ticket to this file, alone on
//!                           its line, for carrying to another machine
//!   --raw                   send the microphone as it is: no echo, noise or level work
//!   --floor <frames>        the least delay a voice's jitter buffer may aim for, in
//!                           frames of 20 ms (default 0; it starts at 2 and comes down)
//!   --margin <frames>       what the jitter buffer adds to the lateness it has seen
//!                           (default 0)
//!   --lead <ms>             how much sound is kept waiting for the speaker (default 20)
//!   --noise <strength>      how hard the microphone's steady noise is turned down: low,
//!                           moderate, high or veryhigh; or auto (the default): moderate,
//!                           and high for a microphone with much noise of its own
//!   --seconds <n>           how long to stay (default: until stopped)
//! ```
//!
//! The microphone and the speaker are the system's defaults. Each second it says who is
//! in the meeting and how the sound is going:
//!
//! ```text
//! second 3: roster 2, links 1, hears 1 voice; microphone -28 dB, 150 frames, 97 sent;
//!     speaker 148 frames, dry 0 ms, behind 0 ms
//! ```
//!
//! A joiner nobody can answer prints a `reply` ticket to send back to whoever invited
//! it; paste that into the inviter's terminal and it reaches for the joiner.

use palaver_harness::details::Tab;
use palaver_harness::live::{Live, Tuning};
use palaver_harness::ui::{Setup, Shot};
use palaver_media::Channels;
use palaver_media::process::NoiseStrength;
use palaver_net::quic::QuicOptions;
use palaver_net::reach::{DEFAULT_STUN_SERVERS, Network};
use palaver_proto::{Secret, Ticket};
use palaver_sdk::{Options, Participant, entry_points};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

/// What the command line asks for.
#[derive(Debug)]
struct Asked {
    ticket: Option<Ticket>,
    name: String,
    bind: Option<SocketAddr>,
    advertise: Vec<SocketAddr>,
    stun: bool,
    ticket_file: Option<String>,
    /// Send the microphone as it is.
    raw: bool,
    tuning: Tuning,
    seconds: Option<u64>,
}

fn asked(mut args: impl Iterator<Item = String>) -> Result<Asked, String> {
    let ticket = match args.next().as_deref() {
        Some("create") => None,
        Some("join") => {
            let text = args.next().ok_or("join needs a ticket")?;
            Some(Ticket::parse(&text).map_err(|error| format!("bad ticket: {error:?}"))?)
        }
        _ => return Err(String::from("say `create`, or `join <ticket>`")),
    };
    let mut asked = Asked {
        ticket,
        name: String::from("someone"),
        bind: None,
        advertise: Vec::new(),
        stun: true,
        ticket_file: None,
        raw: false,
        tuning: Tuning::default(),
        seconds: None,
    };
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or_else(|| format!("{arg} needs {what}"));
        let address = |text: String| {
            text.parse::<SocketAddr>()
                .map_err(|_| format!("not an address: {text}"))
        };
        match arg.as_str() {
            "--name" => asked.name = value("a name")?,
            "--bind" => asked.bind = Some(address(value("an address")?)?),
            "--advertise" => asked.advertise.push(address(value("an address")?)?),
            "--no-stun" => asked.stun = false,
            "--ticket-file" => asked.ticket_file = Some(value("a path")?),
            "--raw" => asked.raw = true,
            "--floor" | "--margin" | "--lead" | "--noise" => {
                tune(&mut asked.tuning, &arg, &value("a setting")?)?;
            }
            "--seconds" => {
                let text = value("a number")?;
                asked.seconds = Some(text.parse().map_err(|_| format!("not a number: {text}"))?);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(asked)
}

/// Set one of the things being tried: `--floor` and `--margin` in frames of 20 ms,
/// `--lead` in milliseconds, `--noise` by name.
fn tune(tuning: &mut Tuning, option: &str, text: &str) -> Result<(), String> {
    if option == "--noise" {
        tuning.noise = match text {
            "auto" => None,
            "low" => Some(NoiseStrength::Low),
            "moderate" => Some(NoiseStrength::Moderate),
            "high" => Some(NoiseStrength::High),
            "veryhigh" => Some(NoiseStrength::VeryHigh),
            _ => return Err(format!("not auto, low, moderate, high or veryhigh: {text}")),
        };
        return Ok(());
    }
    let number: u32 = text.parse().map_err(|_| format!("not a number: {text}"))?;
    match option {
        "--floor" => tuning.floor_frames = number.min(25),
        "--margin" => tuning.margin_frames = number.min(25),
        // Less than the speaker's own piece would leave it nothing to play.
        _ => tuning.lead_ms = number.clamp(10, 500),
    }
    Ok(())
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

/// A level as decibels below full scale.
fn decibels(level: u8) -> i32 {
    i32::from(level).saturating_sub(127)
}

async fn run(asked: Asked) -> Result<(), String> {
    let secret = match &asked.ticket {
        Some(ticket) => Secret::new(*ticket.secret.expose()),
        None => palaver_net::identity::Identity::generate()
            .map_err(|_| "the system gave no random bytes")?
            .secret(),
    };
    let quic = QuicOptions {
        bind: asked.bind.unwrap_or_else(|| QuicOptions::default().bind),
        ..QuicOptions::default()
    };
    let playout = Channels::Stereo;
    let usual = Options::named(&asked.name);
    let options = Options {
        playout,
        jitter: asked.tuning.applied(usual.jitter),
        ..usual
    };
    let (mut participant, call) =
        Participant::on_quic(quic, asked.advertise.clone(), &secret, options)
            .map_err(|error| format!("could not start: {error:?}"))?;
    let names = if asked.stun {
        DEFAULT_STUN_SERVERS.map(String::from).to_vec()
    } else {
        Vec::new()
    };

    let cleaning = (!asked.raw).then(|| asked.tuning.cleaning());
    let live = match Live::start(call, cleaning, playout, asked.tuning) {
        Ok(live) => live,
        Err(failed) => {
            if let Some(call) = failed.call {
                call.stop().await;
            }
            return Err(format!("sound: {}", failed.error));
        }
    };
    for (what, device) in [
        ("microphone", live.microphone()),
        ("speaker", live.speaker()),
    ] {
        println!(
            "{what}: {:?}, {} Hz, {} channels",
            device.name, device.format.rate, device.format.channels
        );
    }

    if let Some(ticket) = &asked.ticket {
        participant.join(entry_points(ticket)).await;
    } else {
        // An invitation is about to be made: now is when the public address matters.
        say_network(participant.find_network_asking(&names).await);
        participant.create_meeting().await;
        // The ticket holds the meeting secret; showing it is the point of these lines.
        let ticket = participant.ticket(&secret).to_text();
        println!("ticket {}", ticket.expose());
        if let Some(path) = &asked.ticket_file {
            std::fs::write(path, format!("{}\n", ticket.expose()))
                .map_err(|error| format!("{path}: {error}"))?;
        }
    }

    stay(&mut participant, &live, &asked, &names).await;
    if let Some(call) = live.stop() {
        call.stop().await;
    }
    Ok(())
}

/// Stay in the meeting: say each second how it goes, answer tickets that come back, and
/// send one back if nobody answers this node.
async fn stay(participant: &mut Participant, live: &Live, asked: &Asked, names: &[String]) {
    // Tickets sent back by those who could not get in arrive as lines on standard input.
    let (paste, mut pasted) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            if paste.send(line).is_err() {
                return;
            }
        }
    });

    let mut beat = tokio::time::interval(Duration::from_secs(1));
    beat.tick().await;
    let mut second = 0u64;
    let mut replied = false;
    while asked.seconds.is_none_or(|seconds| second < seconds) && live.running() {
        tokio::select! {
            _ = beat.tick() => {}
            Some(text) = pasted.recv() => {
                match Ticket::parse(text.trim()) {
                    Ok(back) => {
                        println!("reaching for whoever sent a ticket back");
                        participant.reach(&back).await;
                    }
                    Err(error) => eprintln!("not a ticket: {error:?}"),
                }
                continue;
            }
        }
        second = second.saturating_add(1);
        let (roster, connected, nobody_answers) = {
            let status = participant.control().status();
            let status = status.borrow();
            (
                status.roster.len(),
                status.links.len(),
                status.nobody_answers,
            )
        };
        if let (true, false, Some(invitation)) = (nobody_answers, replied, &asked.ticket) {
            // It cannot get in on its own. Its public address is wanted now, for the
            // ticket that goes back to whoever invited it.
            replied = true;
            say_network(participant.find_network_asking(names).await);
            println!(
                "reply {}",
                participant.reply_to(invitation).to_text().expose()
            );
        }
        let sound = live.meter().read();
        println!(
            "second {second}: roster {roster}, links {connected}, hears {} voice{}; \
             microphone {} dB, {} frames, {} sent; speaker {} frames, dry {} ms, behind {} ms",
            sound.voices,
            if sound.voices == 1 { "" } else { "s" },
            decibels(sound.loudest),
            sound.captured,
            sound.sent,
            sound.played,
            sound.starved.as_millis(),
            sound.dropped.as_millis()
        );
    }
}

/// What the window is opened with: nothing, `shot <file.png> [name]`, or
/// `window [--name <name>] [--ticket-file <path>]`.
fn window(first: Option<&str>, mut args: impl Iterator<Item = String>) -> Result<Setup, String> {
    let mut setup = Setup::default();
    if first == Some("shot") {
        setup.shot = Some(Shot {
            path: args.next().ok_or("shot needs a file to write")?,
            name: args.next(),
            tab: Tab::default(),
        });
        match args.next().as_deref() {
            Some("pattern") => setup.pattern = true,
            Some("coded") => {
                setup.pattern = true;
                setup.coded = Some(500_000);
            }
            _ => {}
        }
        setup.ticket_file = args.next().filter(|path| path != "-");
        if let (Some(shot), Some(name)) = (setup.shot.as_mut(), args.next()) {
            shot.tab = Tab::named(&name).ok_or_else(|| format!("no such tab: {name}"))?;
        }
        return Ok(setup);
    }
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or_else(|| format!("{arg} needs {what}"));
        match arg.as_str() {
            "--name" => setup.name = Some(value("a name")?),
            "--floor" | "--margin" | "--lead" | "--noise" => {
                tune(&mut setup.tuning, &arg, &value("a setting")?)?;
            }
            "--ticket-file" => setup.ticket_file = Some(value("a path")?),
            "--pattern" => setup.pattern = true,
            "--coded" => {
                let text = value("a rate in kbit/s")?;
                let kbits: u32 = text.parse().map_err(|_| format!("not a number: {text}"))?;
                setup.pattern = true;
                setup.coded = Some(kbits.clamp(30, 20_000).saturating_mul(1000));
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(setup)
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    if first
        .as_deref()
        .is_none_or(|first| matches!(first, "shot" | "window"))
    {
        let setup = match window(first.as_deref(), args) {
            Ok(setup) => setup,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        };
        return match palaver_harness::ui::run(setup) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("no window: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let asked = match asked(std::env::args().skip(1)) {
        Ok(asked) => asked,
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
    match runtime.block_on(run(asked)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
