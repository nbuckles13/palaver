//! Hear yourself: the microphone, through the whole voice path, to the speaker.
//!
//! ```text
//! cargo run -p palaver-sound --example loopback [seconds] [stereo] [raw] [echo] [save]
//! ```
//!
//! What the microphone gives is cut into frames, cleaned of noise and brought to a level,
//! encoded with Opus, put through a jitter buffer, decoded and played, as a voice from
//! another node would be; only the network is left out. Wear headphones, or what is
//! played goes round and round.
//!
//! - `raw`: no cleaning at all.
//! - `echo`: echo cancelling too. It is off here because what is played is your own voice
//!   a moment ago, and an echo canceller shown your voice as "what the others said" takes
//!   what you say next for its echo and turns it down. In a call it is shown the others.
//! - `save`: write what was captured, what was sent on and what was played to
//!   `loopback-captured.wav`, `loopback-cleaned.wav` and `loopback-played.wav`.
//!
//! Each second it prints:
//!
//! - the loudest frame from the microphone, after cleaning, and played, in decibels
//!   below full scale (speech is around -30 to -15; a quiet room around -60);
//! - how many frames the microphone has given in all. Fifty a second is right. More means
//!   the microphone runs faster than the clock, and what is played falls behind and loses
//!   pieces (seen through `WSLg`: up to 55 a second);
//! - how the frames played were made, the jitter buffer's delay and its counts;
//! - whether the speaker ran dry or fell behind.

// This runs against real devices, which keep real time: the one place outside the e2e
// crate where a real sleep is the right thing.
#![expect(
    clippy::disallowed_methods,
    reason = "this runs against real devices, which keep real time"
)]

use palaver_media::jitter::Config as JitterConfig;
use palaver_media::level::audio_level;
use palaver_media::pcm::to_i16;
use palaver_media::process::{Processor, Settings};
use palaver_media::voice::{Played, VoiceReceiver, VoiceSender};
use palaver_media::{Channels, EncoderConfig, SAMPLE_RATE};
use palaver_proto::Timestamp;
use palaver_sound::{Device, Microphone, Speaker};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy)]
struct Options {
    seconds: u64,
    channels: Channels,
    /// `None` for no cleaning.
    cleaning: Option<Settings>,
    save: bool,
}

fn say(what: &str, device: &Device) {
    println!(
        "{what}: {:?}, {} Hz, {} channels, {} samples",
        device.name, device.format.rate, device.format.channels, device.sample_type
    );
}

/// `pcm` as a 16-bit WAV file.
fn save(name: &str, pcm: &[f32], channels: Channels) -> Result<(), String> {
    let count = u16::try_from(channels.count()).unwrap_or(u16::MAX);
    let data = u32::try_from(pcm.len().saturating_mul(2)).map_err(|_| "too long to save")?;
    let mut bytes = Vec::with_capacity(pcm.len().saturating_mul(2).saturating_add(44));
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&data.saturating_add(36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    // Plain samples.
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    let block = count.saturating_mul(2);
    bytes.extend_from_slice(&SAMPLE_RATE.saturating_mul(u32::from(block)).to_le_bytes());
    bytes.extend_from_slice(&block.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    bytes.extend(pcm.iter().flat_map(|sample| to_i16(*sample).to_le_bytes()));
    std::fs::write(name, bytes).map_err(|error| format!("{name}: {error}"))?;
    println!("saved {name}");
    Ok(())
}

/// What happened in one second.
#[derive(Debug, Default)]
struct Second {
    captured: u8,
    cleaned: u8,
    played: u8,
    decoded: u32,
    recovered: u32,
    concealed: u32,
    silent: u32,
}

/// A level as decibels below full scale.
fn decibels(level: u8) -> i32 {
    i32::from(level).saturating_sub(127)
}

fn run(options: Options) -> Result<(), String> {
    let channels = options.channels;
    // The speaker first: what the microphone gathers while the other opens would be
    // played late.
    let speaker = Speaker::open(channels).map_err(|error| format!("speaker: {error}"))?;
    let microphone = Microphone::open(channels).map_err(|error| format!("microphone: {error}"))?;
    say("microphone", microphone.device());
    say("speaker", speaker.device());
    println!("cleaning: {:?}", options.cleaning);

    let config = EncoderConfig {
        channels,
        ..EncoderConfig::default()
    };
    let codec = |error| format!("codec: {error:?}");
    let mut sender = VoiceSender::new(config).map_err(codec)?;
    let mut receiver: VoiceReceiver<Vec<u8>> =
        VoiceReceiver::new(JitterConfig::default(), channels).map_err(codec)?;
    let mut processor = options
        .cleaning
        .map(|settings| Processor::new(settings, channels, channels));
    let length = |_| String::from("a frame of the wrong length");

    let mut captured = vec![0.0; channels.frame_len()];
    let mut played = vec![0.0; channels.frame_len()];
    let (mut all_captured, mut all_cleaned, mut all_played) = (Vec::new(), Vec::new(), Vec::new());
    let began = Instant::now();
    let (mut frames, mut sent) = (0u64, 0u64);
    let mut second = Second::default();
    for tick in 0..options.seconds.saturating_mul(50) {
        let now =
            Timestamp::from_micros(u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX));
        // The microphone's clock is not this loop's: take whatever has gathered.
        while microphone.pop(&mut captured) {
            frames = frames.saturating_add(1);
            second.captured = second.captured.max(audio_level(&captured));
            if options.save {
                all_captured.extend_from_slice(&captured);
            }
            if let Some(processor) = &mut processor {
                processor.captured(&mut captured).map_err(length)?;
            }
            second.cleaned = second.cleaned.max(audio_level(&captured));
            if options.save {
                all_cleaned.extend_from_slice(&captured);
            }
            if let Some(packet) = sender.encode(&captured).map_err(codec)? {
                sent = sent.saturating_add(1);
                receiver.insert(packet.seq, Some(packet.level), packet.payload.to_vec(), now);
            }
        }
        let how = receiver.pull(now, &mut played).map_err(codec)?;
        let count = match how {
            Played::Decoded => &mut second.decoded,
            Played::Recovered => &mut second.recovered,
            Played::Concealed => &mut second.concealed,
            Played::Silent => &mut second.silent,
        };
        *count = count.saturating_add(1);
        second.played = second.played.max(audio_level(&played));
        if options.save {
            all_played.extend_from_slice(&played);
        }
        if let Some(processor) = &mut processor {
            processor.played(&played).map_err(length)?;
        }
        speaker.push(&played);

        if tick % 50 == 49 {
            let (starved, dropped) = speaker.starved_and_dropped();
            println!(
                "second {}: loudest {} dB captured, {} cleaned, {} played; {frames} frames, \
                 {sent} sent; played {} decoded, {} recovered, {} concealed, {} silent; \
                 buffer {} ms, {:?}; speaker starved {starved}, dropped {dropped}; errors {} in, {} out",
                tick / 50 + 1,
                decibels(second.captured),
                decibels(second.cleaned),
                decibels(second.played),
                second.decoded,
                second.recovered,
                second.concealed,
                second.silent,
                receiver.target_delay().as_millis(),
                receiver.stats(),
                microphone.errors(),
                speaker.errors()
            );
            second = Second::default();
        }
        let next = FRAME.saturating_mul(u32::try_from(tick.saturating_add(1)).unwrap_or(u32::MAX));
        std::thread::sleep(next.saturating_sub(began.elapsed()));
    }
    if options.save {
        save("loopback-captured.wav", &all_captured, channels)?;
        save("loopback-cleaned.wav", &all_cleaned, channels)?;
        save("loopback-played.wav", &all_played, channels)?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let mut options = Options {
        seconds: 10,
        channels: Channels::Mono,
        cleaning: Some(Settings {
            echo: false,
            ..Settings::default()
        }),
        save: false,
    };
    for arg in std::env::args().skip(1) {
        match (arg.as_str(), arg.parse()) {
            ("stereo", _) => options.channels = Channels::Stereo,
            ("raw", _) => options.cleaning = None,
            ("echo", _) => options.cleaning = Some(Settings::default()),
            ("save", _) => options.save = true,
            (_, Ok(number)) => options.seconds = number,
            _ => {
                eprintln!("usage: loopback [seconds] [stereo] [raw] [echo] [save]");
                return ExitCode::FAILURE;
            }
        }
    }
    match run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
