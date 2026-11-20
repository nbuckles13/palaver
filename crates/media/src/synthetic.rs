//! Made-up voices, for bots and tests.
//!
//! A [`Voice`] sounds enough like speech for Opus to treat it as speech: a pitch with
//! harmonics that fall away, its loudness rising and falling a few times a second like
//! syllables. Each voice has its own pitch, which is how [`hears`] can tell whose voice is
//! in what a listener plays, even after encoding, loss and mixing. A [`Rhythm`] says when
//! a voice talks and when it pauses.
//!
//! Everything here is a pure function of its inputs: the same voice gives the same samples
//! on every run.

use palaver_opus::{FRAME_SAMPLES, SAMPLE_RATE};
use std::f32::consts::TAU;
use std::time::Duration;

/// How many different pitches [`pitch_of`] gives before they repeat.
pub const PITCHES: u32 = 8;

/// The pitch of a speaker's voice: eight pitches a semitone and a half apart, from 110 Hz
/// up to just under 220 Hz. All lie within one octave, so no voice's harmonics fall on
/// another's pitch, and they are far enough apart to tell one from the next in half a
/// second. Speakers beyond the eighth share a pitch with an earlier one.
pub fn pitch_of(speaker: u32) -> f32 {
    const LOWEST: f32 = 110.0;
    LOWEST * 2f32.powf((speaker % PITCHES) as f32 * 1.5 / 12.0)
}

/// A made-up voice.
#[derive(Debug, Clone)]
pub struct Voice {
    pitch: f32,
    loudness: f32,
    /// Samples made so far.
    position: u64,
}

impl Voice {
    /// A voice at `pitch` Hz whose loudest moments reach `loudness` (0 to 1).
    pub fn new(pitch: f32, loudness: f32) -> Self {
        Self {
            pitch,
            loudness,
            position: 0,
        }
    }

    pub fn pitch(&self) -> f32 {
        self.pitch
    }

    /// The next 20 ms: speech if `talking`, silence if not. Time passes either way.
    pub fn frame(&mut self, talking: bool, out: &mut [f32]) {
        for sample in out.iter_mut() {
            let t = self.position as f32 / SAMPLE_RATE as f32;
            self.position = self.position.wrapping_add(1);
            if !talking {
                *sample = 0.0;
                continue;
            }
            // Four syllables a second, never quite falling to nothing.
            let syllable = 0.55 + 0.45 * (TAU * 4.0 * t).sin();
            let mut value = 0.0;
            for harmonic in 1..=6u8 {
                let h = f32::from(harmonic);
                value += (TAU * self.pitch * h * t).sin() / h;
            }
            // The harmonics sum to at most about 2.45.
            *sample = self.loudness * syllable * value / 2.45;
        }
    }

    /// The next 20 ms as a new buffer.
    pub fn next_frame(&mut self, talking: bool) -> Vec<f32> {
        let mut out = vec![0.0; FRAME_SAMPLES];
        self.frame(talking, &mut out);
        out
    }
}

/// When a voice talks: for `talk`, then quiet for `pause`, over and over, starting
/// `offset` into the cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rhythm {
    pub talk: Duration,
    pub pause: Duration,
    pub offset: Duration,
}

impl Rhythm {
    /// Talks all the time.
    pub const ALWAYS: Self = Self {
        talk: Duration::from_secs(1),
        pause: Duration::ZERO,
        offset: Duration::ZERO,
    };

    /// Whether the voice is talking `elapsed` after it started.
    pub fn talking(&self, elapsed: Duration) -> bool {
        let cycle = self.talk.saturating_add(self.pause).as_micros();
        if cycle == 0 {
            return false;
        }
        let at = elapsed.saturating_add(self.offset).as_micros() % cycle;
        at < self.talk.as_micros()
    }
}

/// How strongly `pitch` is present in `pcm`: the amplitude of that one frequency (the
/// Goertzel algorithm, over a Hann window so that a nearby pitch does not leak in). Zero
/// for silence; for a [`Voice`] of loudness `a` at that pitch, around `0.2 * a`.
pub fn strength(pcm: &[f32], pitch: f32) -> f32 {
    if pcm.len() < 2 {
        return 0.0;
    }
    let coefficient = 2.0 * f64::from(TAU * pitch / SAMPLE_RATE as f32).cos();
    let last = (pcm.len() - 1) as f64;
    let (mut before, mut before_that) = (0.0f64, 0.0f64);
    for (i, sample) in pcm.iter().enumerate() {
        let window = 0.5 - 0.5 * (f64::from(TAU) * i as f64 / last).cos();
        let now = f64::from(*sample) * window + coefficient * before - before_that;
        before_that = before;
        before = now;
    }
    let power = before * before + before_that * before_that - coefficient * before * before_that;
    // The window halves the amplitude; give it back.
    (4.0 * power.max(0.0).sqrt() / pcm.len() as f64) as f32
}

/// Whether a voice at `pitch` can be heard in `pcm`. Give it half a second or more: over a
/// shorter stretch neighbouring pitches blur together.
pub fn hears(pcm: &[f32], pitch: f32) -> bool {
    /// A tenth of what the quietest voice a test is likely to use would give.
    const AUDIBLE: f32 = 0.004;
    strength(pcm, pitch) > AUDIBLE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(voice: &mut Voice, talking: bool, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|_| voice.next_frame(talking))
            .collect()
    }

    #[test]
    fn a_voice_is_heard_at_its_own_pitch_and_not_at_anothers() {
        for speaker in 0..PITCHES {
            let mut voice = Voice::new(pitch_of(speaker), 0.5);
            let pcm = seconds(&mut voice, true, 50);
            for other in 0..PITCHES {
                assert_eq!(
                    hears(&pcm, pitch_of(other)),
                    other == speaker,
                    "speaker {speaker} heard at the pitch of {other}: {}",
                    strength(&pcm, pitch_of(other))
                );
            }
        }
    }

    #[test]
    fn a_voice_that_is_not_talking_is_silent_and_time_still_passes() {
        let mut paused = Voice::new(220.0, 0.5);
        assert!(seconds(&mut paused, false, 5).iter().all(|s| *s == 0.0));
        let after_pause = paused.next_frame(true);
        let mut straight = Voice::new(220.0, 0.5);
        seconds(&mut straight, true, 5);
        assert_eq!(after_pause, straight.next_frame(true));
    }

    #[test]
    fn a_voice_stays_within_its_loudness_and_is_the_same_every_run() {
        let mut voice = Voice::new(150.0, 0.5);
        let pcm = seconds(&mut voice, true, 100);
        let peak = pcm.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 0.5 && peak > 0.3, "{peak}");
        assert_eq!(pcm, seconds(&mut Voice::new(150.0, 0.5), true, 100));
    }

    #[test]
    fn two_voices_mixed_are_both_heard_and_a_third_is_not() {
        let mut a = Voice::new(pitch_of(0), 0.3);
        let mut b = Voice::new(pitch_of(5), 0.3);
        let mut mixed = seconds(&mut a, true, 50);
        crate::mix::add(&mut mixed, &seconds(&mut b, true, 50));
        assert!(hears(&mixed, pitch_of(0)));
        assert!(hears(&mixed, pitch_of(5)));
        assert!(!hears(&mixed, pitch_of(2)));
    }

    #[test]
    fn a_rhythm_talks_then_pauses_and_repeats() {
        let rhythm = Rhythm {
            talk: Duration::from_secs(2),
            pause: Duration::from_secs(3),
            offset: Duration::from_secs(1),
        };
        let talking = |ms| rhythm.talking(Duration::from_millis(ms));
        // One second in already: one more second of talk, three of pause, then round again.
        assert!(talking(0) && talking(999));
        assert!(!talking(1000) && !talking(3999));
        assert!(talking(4000) && talking(5999));
        assert!(!talking(6000));
        assert!(Rhythm::ALWAYS.talking(Duration::from_secs(1234)));
        let never = Rhythm {
            talk: Duration::ZERO,
            pause: Duration::ZERO,
            offset: Duration::ZERO,
        };
        assert!(!never.talking(Duration::ZERO));
    }
}
