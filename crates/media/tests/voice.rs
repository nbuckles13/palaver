//! One voice end to end: made-up speech, encoded, carried (or lost), buffered, decoded,
//! mixed. What is checked is what a listener would notice: whose voice is heard, and when.

#![allow(clippy::unwrap_used)]

use palaver_media::jitter::Config as JitterConfig;
use palaver_media::level::audio_level;
use palaver_media::mix;
use palaver_media::synthetic::{Voice, hears, pitch_of, strength};
use palaver_media::voice::{Played, VoiceReceiver, VoiceSender};
use palaver_media::{FRAME_SAMPLES, SAMPLE_RATE};
use palaver_opus::EncoderConfig;
use palaver_proto::Timestamp;

const FRAME_MS: u64 = 20;
const LOUDNESS: f32 = 0.5;

/// What came of running one voice over one path.
struct Heard {
    /// Everything played, frame after frame.
    pcm: Vec<f32>,
    played: Vec<Played>,
    /// The level on each packet sent, by frame; `None` where nothing was sent.
    sent: Vec<Option<u8>>,
}

impl Heard {
    /// What was played between two times, in milliseconds.
    fn between(&self, from_ms: u64, to_ms: u64) -> &[f32] {
        let at = |ms: u64| (ms * u64::from(SAMPLE_RATE) / 1000) as usize;
        &self.pcm[at(from_ms)..at(to_ms)]
    }

    fn count(&self, how: Played) -> usize {
        self.played.iter().filter(|p| **p == how).count()
    }
}

/// Speaker `speaker` talks when `talking(frame)` says so; packet `frame` is lost when
/// `lost(frame)` says so. Every frame interval the listener plays 20 ms.
fn carry(
    speaker: u32,
    frames: u64,
    talking: impl Fn(u64) -> bool,
    lost: impl Fn(u64) -> bool,
) -> Heard {
    let mut voice = Voice::new(pitch_of(speaker), LOUDNESS);
    let mut sender = VoiceSender::new(EncoderConfig::default()).unwrap();
    let mut receiver: VoiceReceiver<Vec<u8>> = VoiceReceiver::new(JitterConfig::default()).unwrap();
    let mut heard = Heard {
        pcm: Vec::new(),
        played: Vec::new(),
        sent: Vec::new(),
    };
    let mut frame_out = vec![0.0; FRAME_SAMPLES];
    for frame in 0..frames {
        let now = Timestamp::from_millis(frame * FRAME_MS);
        let pcm = voice.next_frame(talking(frame));
        let packet = sender.encode(&pcm).unwrap();
        heard.sent.push(packet.as_ref().map(|p| p.level));
        if let Some(packet) = packet {
            assert_eq!(packet.seq, frame);
            if !lost(frame) {
                receiver.insert(packet.seq, Some(packet.level), packet.payload.to_vec(), now);
            }
        }
        heard
            .played
            .push(receiver.pull(now, &mut frame_out).unwrap());
        heard.pcm.extend_from_slice(&frame_out);
    }
    heard
}

/// A different sequence of yes and no for every seed, the same on every run; yes about
/// one time in `one_in`.
fn sometimes(seed: u64, n: u64, one_in: u64) -> bool {
    let mut x = seed ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x.is_multiple_of(one_in)
}

#[test]
fn a_voice_is_heard_at_the_other_end_and_no_other_voice_is() {
    let heard = carry(2, 100, |_| true, |_| false);
    assert!(hears(heard.between(500, 2000), pitch_of(2)));
    for other in [0, 1, 3, 4] {
        assert!(!hears(heard.between(500, 2000), pitch_of(other)), "{other}");
    }
    // About as loud as it went in.
    let got = strength(heard.between(500, 2000), pitch_of(2));
    let mut voice = Voice::new(pitch_of(2), LOUDNESS);
    let sent: Vec<f32> = (0..100).flat_map(|_| voice.next_frame(true)).collect();
    let want = strength(&sent[24_000..96_000], pitch_of(2));
    assert!((got / want - 1.0).abs() < 0.25, "{got} against {want}");
    // On a perfect path, once the buffer has settled, every frame is the frame itself.
    assert!(
        heard.played[10..].iter().all(|p| *p == Played::Decoded),
        "{:?}",
        heard.played
    );
}

#[test]
fn every_packet_of_speech_carries_a_level_in_the_range_of_speech() {
    let heard = carry(0, 50, |_| true, |_| false);
    let levels: Vec<u8> = heard.sent.iter().flatten().copied().collect();
    assert_eq!(levels.len(), 50);
    // Half scale at its loudest, with syllables: between -37 and -12 dBFS.
    assert!(levels.iter().all(|l| (90..=115).contains(l)), "{levels:?}");
    // And it is the level of what went in.
    let mut voice = Voice::new(pitch_of(0), LOUDNESS);
    assert_eq!(levels[0], audio_level(&voice.next_frame(true)));
}

#[test]
fn silence_is_mostly_not_sent_and_plays_as_silence() {
    // Half a second of speech, then three seconds of nothing.
    let heard = carry(1, 175, |frame| frame < 25, |_| false);
    let sent_in_silence = heard.sent[25..].iter().flatten().count();
    assert!(
        sent_in_silence <= 30,
        "{sent_in_silence} packets in 150 frames of silence"
    );
    // The counter went on through the silence: checked in `carry`, packet by packet.
    let quiet = heard.between(1500, 3500);
    assert!(!hears(quiet, pitch_of(1)));
    let peak = quiet.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak < 0.02, "{peak}");
}

#[test]
fn a_voice_that_pauses_comes_back() {
    // One second on, one off, one on.
    let heard = carry(3, 150, |frame| !(50..100).contains(&frame), |_| false);
    assert!(hears(heard.between(200, 1000), pitch_of(3)));
    assert!(!hears(heard.between(1300, 2000), pitch_of(3)));
    assert!(hears(heard.between(2200, 3000), pitch_of(3)));
}

#[test]
fn lost_packets_are_rebuilt_from_the_ones_after_them() {
    let lost = |frame: u64| frame > 10 && sometimes(7, frame, 20);
    let heard = carry(4, 150, |_| true, lost);
    let losses = (0..150).filter(|f| lost(*f)).count();
    assert!(losses >= 4, "the scenario lost too little: {losses}");
    // Single losses are rebuilt from the next packet; only a run of two leaves a guess.
    assert!(
        heard.count(Played::Recovered) >= losses - 2,
        "{} recovered of {losses}",
        heard.count(Played::Recovered)
    );
    // The listener still hears the voice, about as loud.
    let clean = carry(4, 150, |_| true, |_| false);
    let (got, want) = (
        strength(heard.between(500, 3000), pitch_of(4)),
        strength(clean.between(500, 3000), pitch_of(4)),
    );
    assert!((got / want - 1.0).abs() < 0.2, "{got} against {want}");
}

#[test]
fn a_long_outage_is_guessed_briefly_then_silent_and_the_voice_returns_after_it() {
    // Nothing gets through for a second.
    let heard = carry(5, 150, |_| true, |frame| (50..100).contains(&frame));
    assert!(hears(heard.between(300, 1000), pitch_of(5)));
    assert!(!hears(heard.between(1400, 2000), pitch_of(5)));
    assert!(hears(heard.between(2300, 3000), pitch_of(5)));
    assert!(heard.count(Played::Silent) >= 30, "{:?}", heard.played);
}

#[test]
fn two_voices_mixed_are_both_heard_and_the_mix_stays_in_range() {
    let a = carry(0, 100, |_| true, |_| false);
    let b = carry(6, 100, |_| true, |_| false);
    let mut mixed = a.pcm.clone();
    mix::add(&mut mixed, &b.pcm);
    mix::limit(&mut mixed);
    assert!(hears(&mixed[24_000..], pitch_of(0)));
    assert!(hears(&mixed[24_000..], pitch_of(6)));
    assert!(!hears(&mixed[24_000..], pitch_of(3)));
    assert!(mixed.iter().all(|s| s.abs() < 1.0));
}

#[test]
fn a_frame_of_the_wrong_length_is_refused() {
    let mut sender = VoiceSender::new(EncoderConfig::default()).unwrap();
    assert!(sender.encode(&[0.0; 100]).is_err());
    // A refused frame does not use up a number.
    assert_eq!(sender.next_seq(), 0);
    let mut receiver: VoiceReceiver<Vec<u8>> = VoiceReceiver::new(JitterConfig::default()).unwrap();
    assert!(receiver.pull(Timestamp::ZERO, &mut [0.0; 100]).is_err());
}
