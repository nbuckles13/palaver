//! Changing the sample rate of a stream of mono samples.
//!
//! Sound devices run at the rate they like (44.1 kHz is common); everything between the
//! microphone and the speaker here runs at 48 kHz. A [`Resampler`] converts a stream from
//! one rate to another, a piece at a time, with no gap or click where the pieces join.
//!
//! How: each output sample is a weighted sum of the [`TAPS`] input samples around the
//! moment it falls on. The weights are a sinc curve under a window, which is the standard
//! way to rebuild the signal between its samples while removing what the lower of the two
//! rates cannot carry. The weights are worked out ahead for [`PHASES`] positions between
//! two input samples, and blended for positions in between.
//!
//! The output lags the input by [`Resampler::delay`] input samples.

use std::f64::consts::PI;

/// How many input samples go into each output sample when the rate goes up or stays.
/// Going down, it is this times the ratio (rounded up), so that the filter is as sharp
/// measured against the new rate as it is against the old.
pub const TAPS: usize = 32;
/// The most the ratio may multiply [`TAPS`] by.
const MOST_WIDENING: u32 = 16;
/// How finely the gap between two input samples is divided for the table of weights.
pub const PHASES: usize = 256;
/// The share of the lower rate's bandwidth that is kept; the rest is where the filter
/// goes from passing to stopping.
const KEEP: f64 = 0.9;

/// A stream at one sample rate turned into the same sound at another.
#[derive(Debug, Clone)]
pub struct Resampler {
    from: u64,
    to: u64,
    /// How many input samples go into each output sample.
    taps: usize,
    /// `PHASES + 1` rows of `taps` weights; the last row is the first of the next sample.
    table: Vec<f32>,
    /// Input not yet used up, oldest first. Begins with silence, so that the first
    /// output samples have a past to draw on.
    held: Vec<f32>,
    /// How far the next output sample lies past `held[0]`'s successor, in units of
    /// `1 / to` of an input sample; always less than `to`.
    fraction: u64,
    /// Input samples still to come that the next output sample lies beyond: they are
    /// dropped as they arrive. Not zero only when going down by a very large factor.
    skip: usize,
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

impl Resampler {
    /// From `from` samples a second to `to`. `None` if either is zero.
    pub fn new(from: u32, to: u32) -> Option<Self> {
        if from == 0 || to == 0 {
            return None;
        }
        let widening = from.div_ceil(to).clamp(1, MOST_WIDENING);
        let taps = TAPS.saturating_mul(widening as usize);
        let half = (taps / 2) as f64;
        // Going down, everything above half the new rate must go.
        let cutoff = (f64::from(to) / f64::from(from)).min(1.0) * KEEP;
        let mut table = Vec::with_capacity((PHASES + 1) * taps);
        for phase in 0..=PHASES {
            let offset = phase as f64 / PHASES as f64;
            let row: Vec<f64> = (0..taps)
                .map(|tap| {
                    // Where this input sample lies relative to the output sample.
                    let x = tap as f64 - (half - 1.0) - offset;
                    let window = if x.abs() >= half {
                        0.0
                    } else {
                        // Blackman: low leakage for its width.
                        0.42 + 0.5 * (PI * x / half).cos() + 0.08 * (2.0 * PI * x / half).cos()
                    };
                    cutoff * sinc(cutoff * x) * window
                })
                .collect();
            // A steady input must come out at the same level whatever the phase.
            let sum: f64 = row.iter().sum();
            table.extend(row.iter().map(|weight| (weight / sum) as f32));
        }
        Some(Self {
            from: u64::from(from),
            to: u64::from(to),
            taps,
            table,
            held: vec![0.0; taps - 1],
            fraction: 0,
            skip: 0,
        })
    }

    /// How far the output lags the input, in input samples.
    pub fn delay(&self) -> usize {
        self.taps / 2
    }

    /// Take `input` and append to `out` every output sample that can now be made.
    /// Over time, `to / from` samples come out for each one that goes in.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let skipped = self.skip.min(input.len());
        self.skip = self.skip.saturating_sub(skipped);
        self.held
            .extend_from_slice(input.get(skipped..).unwrap_or_default());
        let mut at = 0usize;
        let taps = self.taps;
        while let Some(window) = self.held.get(at..at.saturating_add(taps)) {
            // Between which two rows of the table the moment falls, and how far along.
            let scaled = self.fraction.saturating_mul(PHASES as u64);
            let row = usize::try_from(scaled / self.to).unwrap_or(0);
            let blend = (scaled % self.to) as f32 / self.to as f32;
            let weights = self
                .table
                .get(row.saturating_mul(taps)..row.saturating_add(2).saturating_mul(taps))
                .unwrap_or_default();
            let (here, next) = weights.split_at(weights.len() / 2);
            let sample: f32 = window
                .iter()
                .zip(here.iter().zip(next))
                .map(|(sample, (here, next))| sample * (here + (next - here) * blend))
                .sum();
            out.push(sample);

            self.fraction = self.fraction.saturating_add(self.from);
            at = at.saturating_add(usize::try_from(self.fraction / self.to).unwrap_or(0));
            self.fraction %= self.to;
        }
        // Going down by a large factor, `at` may have stepped past what is held: what
        // is owed is dropped from the input still to come.
        let used = at.min(self.held.len());
        self.held.drain(..used);
        self.skip = at.saturating_sub(used);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(hz: f32, rate: u32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|n| 0.5 * (TAU * hz * n as f32 / rate as f32).sin())
            .collect()
    }

    /// The amplitude of `hz` in `pcm` at `rate`, by correlation with a sine and a cosine.
    fn amplitude(pcm: &[f32], hz: f32, rate: u32) -> f32 {
        let (mut sin, mut cos) = (0.0f64, 0.0f64);
        for (n, sample) in pcm.iter().enumerate() {
            let angle = f64::from(TAU * hz) * n as f64 / f64::from(rate);
            sin += f64::from(*sample) * angle.sin();
            cos += f64::from(*sample) * angle.cos();
        }
        (2.0 * (sin * sin + cos * cos).sqrt() / pcm.len() as f64) as f32
    }

    /// Everything in `pcm` that is not `hz`, as a share of the tone's amplitude.
    fn everything_else(pcm: &[f32], hz: f32, rate: u32) -> f32 {
        let tone = amplitude(pcm, hz, rate);
        let total: f64 = pcm.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
        let rms = (total / pcm.len() as f64).sqrt() as f32;
        // A sine of amplitude a has an rms of a / sqrt(2).
        let tone_rms = tone / 2f32.sqrt();
        (rms * rms - tone_rms * tone_rms).max(0.0).sqrt() / tone_rms
    }

    fn convert(from: u32, to: u32, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        Resampler::new(from, to).unwrap().process(input, &mut out);
        out
    }

    #[test]
    fn a_tone_keeps_its_pitch_and_its_level_between_the_usual_rates() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (16_000, 48_000),
            (48_000, 16_000),
        ] {
            // A whole number of cycles in the stretch measured, at either rate.
            let out = convert(from, to, &tone(1000.0, from, from as usize));
            let steady = &out[to as usize / 4..to as usize * 3 / 4];
            let level = amplitude(steady, 1000.0, to);
            assert!((level - 0.5).abs() < 0.005, "{from} to {to}: level {level}");
            let rest = everything_else(steady, 1000.0, to);
            assert!(rest < 0.003, "{from} to {to}: {rest} of it is not the tone");
        }
    }

    #[test]
    fn as_many_samples_come_out_as_the_rates_say() {
        for (from, to) in [
            (44_100u32, 48_000u32),
            (48_000, 44_100),
            (8_000, 48_000),
            (48_000, 8_000),
        ] {
            let out = convert(from, to, &vec![0.0; from as usize]);
            let want = to as usize;
            // Less what is still held waiting for the samples that follow.
            let held_back = TAPS * MOST_WIDENING as usize;
            assert!(
                out.len() <= want && out.len() + held_back + 1 >= want,
                "{from} to {to}: {} for {want}",
                out.len()
            );
        }
    }

    #[test]
    fn pieces_of_any_size_give_what_the_whole_gives() {
        let input = tone(700.0, 44_100, 20_000);
        let whole = convert(44_100, 48_000, &input);
        for size in [1, 7, 441, 1024, 4097] {
            let mut resampler = Resampler::new(44_100, 48_000).unwrap();
            let mut pieces = Vec::new();
            for piece in input.chunks(size) {
                resampler.process(piece, &mut pieces);
            }
            assert_eq!(pieces, whole, "in pieces of {size}");
        }
    }

    #[test]
    fn the_same_rate_gives_the_input_back_later_by_the_delay() {
        let input = tone(440.0, 48_000, 4800);
        let mut resampler = Resampler::new(48_000, 48_000).unwrap();
        let delay = resampler.delay();
        let mut out = Vec::new();
        resampler.process(&input, &mut out);
        // The filter still trims the very top of the band, which a 440 Hz tone is far
        // below: what comes out is what went in.
        for (n, sample) in out.iter().enumerate().skip(TAPS) {
            let was = input[n - delay];
            assert!(
                (sample - was).abs() < 0.002,
                "sample {n}: {sample} for {was}"
            );
        }
    }

    #[test]
    fn going_down_removes_what_the_lower_rate_cannot_carry() {
        // 10 kHz cannot exist at 16 kHz; left in, it would come out as a false 6 kHz.
        let out = convert(48_000, 16_000, &tone(10_000.0, 48_000, 48_000));
        let steady = &out[4000..12_000];
        let loudest = steady.iter().fold(0.0f32, |most, s| most.max(s.abs()));
        // Fifty decibels below the tone that went in, or lower.
        assert!(loudest < 0.5 / 316.0, "{loudest}");
    }

    #[test]
    fn steady_input_comes_out_steady_and_a_rate_of_zero_is_refused() {
        let out = convert(44_100, 48_000, &vec![0.25; 4410]);
        for sample in &out[TAPS * 2..] {
            assert!((sample - 0.25).abs() < 1e-4, "{sample}");
        }
        assert!(Resampler::new(0, 48_000).is_none());
        assert!(Resampler::new(48_000, 0).is_none());
    }
}
