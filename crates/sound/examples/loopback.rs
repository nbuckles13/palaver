//! Hear yourself: the microphone, through the whole voice path, to the speaker.
//!
//! ```text
//! cargo run -p palaver-sound --example loopback [seconds] [stereo]
//! ```
//!
//! What the microphone gives is cut into frames, encoded with Opus, put through a jitter
//! buffer, decoded and played, as a voice from another node would be; only the network is
//! left out. Each second it prints how loud the microphone was and whether the speaker
//! ran dry or fell behind. Wear headphones, or the speaker feeds the microphone.

// This runs against real devices, which keep real time: the one place outside the e2e
// crate where a real sleep is the right thing.
#![allow(clippy::disallowed_methods)]

use palaver_media::jitter::Config as JitterConfig;
use palaver_media::level::audio_level;
use palaver_media::voice::{VoiceReceiver, VoiceSender};
use palaver_media::{Channels, EncoderConfig};
use palaver_proto::Timestamp;
use palaver_sound::{Device, Microphone, Speaker};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(20);

fn say(what: &str, device: &Device) {
    println!(
        "{what}: {:?}, {} Hz, {} channels, {} samples",
        device.name, device.format.rate, device.format.channels, device.sample_type
    );
}

fn run(seconds: u64, channels: Channels) -> Result<(), String> {
    // The speaker first: what the microphone gathers while the other opens would be
    // played late.
    let speaker = Speaker::open(channels).map_err(|error| format!("speaker: {error}"))?;
    let microphone = Microphone::open(channels).map_err(|error| format!("microphone: {error}"))?;
    say("microphone", microphone.device());
    say("speaker", speaker.device());

    let config = EncoderConfig {
        channels,
        ..EncoderConfig::default()
    };
    let codec = |error| format!("codec: {error:?}");
    let mut sender = VoiceSender::new(config).map_err(codec)?;
    let mut receiver: VoiceReceiver<Vec<u8>> =
        VoiceReceiver::new(JitterConfig::default(), channels).map_err(codec)?;

    let mut captured = vec![0.0; channels.frame_len()];
    let mut played = vec![0.0; channels.frame_len()];
    let started = Instant::now();
    let (mut frames, mut sent, mut loudest) = (0u64, 0u64, 127u8);
    for tick in 0..seconds.saturating_mul(50) {
        let now = Timestamp::from_micros(
            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        );
        // The microphone's clock is not this loop's: take whatever has gathered.
        while microphone.pop(&mut captured) {
            frames = frames.saturating_add(1);
            // The level counts down from silence at 127 to full scale at 0.
            loudest = loudest.min(audio_level(&captured));
            if let Some(packet) = sender.encode(&captured).map_err(codec)? {
                sent = sent.saturating_add(1);
                receiver.insert(packet.seq, Some(packet.level), packet.payload.to_vec(), now);
            }
        }
        receiver.pull(now, &mut played).map_err(codec)?;
        speaker.push(&played);

        if tick % 50 == 49 {
            let (starved, dropped) = speaker.starved_and_dropped();
            println!(
                "second {}: {frames} frames captured, {sent} sent, loudest -{loudest} dB; \
                 speaker starved {starved}, dropped {dropped}; errors {} in, {} out",
                tick / 50 + 1,
                microphone.errors(),
                speaker.errors()
            );
            loudest = 127;
        }
        let next = FRAME.saturating_mul(u32::try_from(tick.saturating_add(1)).unwrap_or(u32::MAX));
        std::thread::sleep(next.saturating_sub(started.elapsed()));
    }
    Ok(())
}

fn main() -> ExitCode {
    let mut seconds = 10;
    let mut channels = Channels::Mono;
    for arg in std::env::args().skip(1) {
        match (arg.as_str(), arg.parse()) {
            ("stereo", _) => channels = Channels::Stereo,
            (_, Ok(number)) => seconds = number,
            _ => {
                eprintln!("usage: loopback [seconds] [stereo]");
                return ExitCode::FAILURE;
            }
        }
    }
    match run(seconds, channels) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
