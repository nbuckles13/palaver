//! Echo, noise and level, in a made-up room: what the speaker plays comes back into the
//! microphone a moment later and quieter, beside whoever is talking there.
//!
//! What the speaker plays here is broadband sound in syllables, not the made-up voices
//! of the other tests. An echo canceller learns the room from the sound it is given, and
//! from a steady pitch with harmonics it learns almost nothing (measured: 3 dB taken out,
//! against 40 dB of this). Real voices are broadband.

#![allow(clippy::unwrap_used)]

use palaver_media::process::{Processor, Settings, WrongLength};
use palaver_media::synthetic::{Voice, hears, pitch_of, strength};
use palaver_media::{Channels, FRAME_SAMPLES, SAMPLE_RATE};
use std::collections::VecDeque;

/// How long the speaker's sound takes to reach the microphone: 30 ms.
const ROOM_DELAY: usize = 1440;
/// How much quieter it is when it gets there.
const ROOM_GAIN: f32 = 0.5;

/// What the microphone heard, and what was left of it after cleaning.
struct Heard {
    raw: Vec<f32>,
    clean: Vec<f32>,
}

impl Heard {
    /// The part from `from` seconds on.
    fn after(pcm: &[f32], from: usize) -> &[f32] {
        &pcm[from * SAMPLE_RATE as usize..]
    }
}

/// `seconds` in the room. `far(frame)` is what the speaker plays, in `played` channels;
/// `near(frame)` is what is said at the microphone.
fn room(
    settings: Settings,
    played: Channels,
    seconds: usize,
    mut far: impl FnMut(usize) -> Vec<f32>,
    mut near: impl FnMut(usize) -> Vec<f32>,
) -> Heard {
    let mut processor = Processor::new(settings, Channels::Mono, played);
    let mut air: VecDeque<f32> = std::iter::repeat_n(0.0, ROOM_DELAY).collect();
    let mut heard = Heard {
        raw: Vec::new(),
        clean: Vec::new(),
    };
    for frame in 0..seconds * 50 {
        let playing = far(frame);
        processor.played(&playing).unwrap();
        // Every channel of the speaker reaches the one microphone.
        air.extend(
            playing
                .chunks_exact(played.count())
                .map(|group| group.iter().sum::<f32>()),
        );
        let mut microphone = near(frame);
        for sample in &mut microphone {
            *sample += ROOM_GAIN * air.pop_front().unwrap();
        }
        heard.raw.extend_from_slice(&microphone);
        processor.captured(&mut microphone).unwrap();
        heard.clean.extend_from_slice(&microphone);
    }
    heard
}

fn silence(_: usize) -> Vec<f32> {
    vec![0.0; FRAME_SAMPLES]
}

fn talking(speaker: u32) -> impl FnMut(usize) -> Vec<f32> {
    let mut voice = Voice::new(pitch_of(speaker), 0.4);
    move |_| voice.next_frame(true)
}

fn energy(pcm: &[f32]) -> f32 {
    pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32
}

/// Broadband sound in syllables, more like speech to an echo canceller than a pitch is.
fn babble(seed: u32, loudness: f32) -> impl FnMut(usize) -> Vec<f32> {
    let mut state = seed;
    let mut low = 0.0f32;
    move |frame| {
        (0..FRAME_SAMPLES)
            .map(|n| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let white = (state >> 8) as f32 / 8_388_608.0 - 1.0;
                // Most of a voice's energy is low.
                low += 0.15 * (white - low);
                let t = (frame * FRAME_SAMPLES + n) as f32 / SAMPLE_RATE as f32;
                let syllable = (0.5 + 0.5 * (std::f32::consts::TAU * 4.0 * t).sin()).powi(2);
                loudness * 3.0 * low * syllable
            })
            .collect()
    }
}

/// The part of `pcm` from `from` seconds on, as energy.
fn energy_after(pcm: &[f32], from: usize) -> f32 {
    energy(Heard::after(pcm, from))
}

const ECHO_ONLY: Settings = Settings {
    echo: true,
    noise: false,
    level: false,
};

#[test]
fn what_the_speaker_plays_is_taken_out_of_the_microphone() {
    for (settings, name) in [
        (ECHO_ONLY, "echo only"),
        (Settings::default(), "everything"),
    ] {
        let heard = room(settings, Channels::Mono, 5, babble(7, 0.4), silence);
        // Twenty-five decibels down or more, after two seconds of learning the room.
        // (Measured: 40.)
        let left = energy_after(&heard.clean, 2) / energy_after(&heard.raw, 2);
        assert!(left < 0.003, "{name}: {left} of the echo is left");
    }
    // With echo cancelling off it stays.
    let off = Settings {
        echo: false,
        ..ECHO_ONLY
    };
    let heard = room(off, Channels::Mono, 3, babble(7, 0.4), silence);
    let left = energy_after(&heard.clean, 1) / energy_after(&heard.raw, 1);
    assert!(left > 0.5, "{left}");
}

#[test]
fn the_voice_at_the_microphone_is_kept_alone_and_while_the_speaker_plays() {
    let alone = room(ECHO_ONLY, Channels::Mono, 3, silence, talking(5));
    let kept = strength(Heard::after(&alone.clean, 1), pitch_of(5))
        / strength(Heard::after(&alone.raw, 1), pitch_of(5));
    assert!(kept > 0.7, "alone, {kept} of the voice is kept");

    // Both at once, after two seconds of the speaker alone.
    let mut near = talking(5);
    let both = room(ECHO_ONLY, Channels::Mono, 5, babble(7, 0.4), |frame| {
        if frame < 100 {
            silence(frame)
        } else {
            near(frame)
        }
    });
    let clean = Heard::after(&both.clean, 3);
    assert!(hears(clean, pitch_of(5)));
    // What the near voice would have been with nothing playing.
    let near = talking(5);
    let wanted: Vec<f32> = (0..250).flat_map(near).collect();
    let kept = strength(clean, pitch_of(5)) / strength(&wanted[..clean.len()], pitch_of(5));
    assert!(
        kept > 0.5,
        "with the speaker playing, {kept} of the voice is kept"
    );
    // And most of what is left is the voice: the echo alone was as loud as it.
    let ratio = energy(clean) / energy(&wanted[..clean.len()]);
    assert!(
        (0.3..1.5).contains(&ratio),
        "{ratio} of the voice's energy came out"
    );
}

#[test]
fn with_headphones_whoever_talks_at_the_microphone_is_not_taken_for_an_echo() {
    // Nothing the speaker plays reaches the microphone. The others talk throughout; the
    // one at the microphone from three seconds in.
    let mut processor = Processor::new(ECHO_ONLY, Channels::Mono, Channels::Mono);
    let (mut far, mut near) = (babble(9, 0.3), babble(3, 0.3));
    let (mut raw, mut clean) = (Vec::new(), Vec::new());
    for frame in 0..250 {
        processor.played(&far(frame)).unwrap();
        let mut microphone = if frame < 150 {
            silence(frame)
        } else {
            near(frame)
        };
        raw.extend_from_slice(&microphone);
        processor.captured(&mut microphone).unwrap();
        clean.extend_from_slice(&microphone);
    }
    // All of it comes through, from the first half second on. (With the processor's
    // other way of telling that there is no echo, a millionth did, for eight seconds.)
    for half in 6..10 {
        let at = half * 24_000..(half + 1) * 24_000;
        let kept = energy(&clean[at.clone()]) / energy(&raw[at]);
        assert!(
            kept > 0.5,
            "half second {half}: {kept} of the voice is kept"
        );
    }
}

#[test]
fn a_speaker_of_two_channels_playing_two_things_is_taken_out_as_well() {
    let (mut left, mut right) = (babble(1, 0.3), babble(2, 0.3));
    let far = |frame: usize| {
        let (l, r) = (left(frame), right(frame));
        l.iter().zip(&r).flat_map(|(l, r)| [*l, *r]).collect()
    };
    let heard = room(ECHO_ONLY, Channels::Stereo, 5, far, silence);
    let left = energy_after(&heard.clean, 2) / energy_after(&heard.raw, 2);
    assert!(left < 0.003, "{left} of the echo is left");
}

#[test]
fn steady_noise_is_turned_down_and_a_voice_in_it_is_kept() {
    // A hiss: the same every time, from a counter.
    let hiss = || {
        let mut state = 1u32;
        move |_: usize| -> Vec<f32> {
            (0..FRAME_SAMPLES)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    ((state >> 8) as f32 / 8_388_608.0 - 1.0) * 0.02
                })
                .collect()
        }
    };
    let only_noise = Settings {
        echo: false,
        level: false,
        noise: true,
    };
    let heard = room(only_noise, Channels::Mono, 4, silence, hiss());
    let left = energy(Heard::after(&heard.clean, 2)) / energy(Heard::after(&heard.raw, 2));
    // Ten decibels down, or more.
    assert!(left < 0.1, "{left} of the noise is left");

    let (mut noise, mut voice) = (hiss(), talking(3));
    let heard = room(only_noise, Channels::Mono, 4, silence, |frame| {
        let mut mixed = voice(frame);
        for (sample, hiss) in mixed.iter_mut().zip(noise(frame)) {
            *sample += hiss;
        }
        mixed
    });
    assert!(hears(Heard::after(&heard.clean, 2), pitch_of(3)));
}

#[test]
fn a_quiet_voice_is_brought_up_and_nothing_goes_past_full_scale() {
    let only_level = Settings {
        echo: false,
        noise: false,
        level: true,
    };
    let mut quiet = Voice::new(pitch_of(4), 0.02);
    let heard = room(only_level, Channels::Mono, 3, silence, move |_| {
        quiet.next_frame(true)
    });
    // Ten decibels or more. (Measured: 14.)
    let gain = energy_after(&heard.clean, 1) / energy_after(&heard.raw, 1);
    assert!(gain > 10.0, "the voice came out {gain} times its energy");

    let mut loud = Voice::new(pitch_of(4), 3.0);
    let heard = room(only_level, Channels::Mono, 2, silence, move |_| {
        loud.next_frame(true)
    });
    assert!(heard.raw.iter().any(|sample| sample.abs() > 1.0));
    assert!(heard.clean.iter().all(|sample| sample.abs() <= 1.0));
}

#[test]
fn a_frame_of_the_wrong_length_is_refused() {
    let mut processor = Processor::new(Settings::default(), Channels::Mono, Channels::Stereo);
    assert_eq!(processor.played(&[0.0; FRAME_SAMPLES]), Err(WrongLength));
    assert_eq!(processor.played(&[0.0; 2 * FRAME_SAMPLES]), Ok(()));
    assert_eq!(
        processor.captured(&mut [0.0; 2 * FRAME_SAMPLES]),
        Err(WrongLength)
    );
    assert_eq!(processor.captured(&mut [0.0; FRAME_SAMPLES]), Ok(()));
}
