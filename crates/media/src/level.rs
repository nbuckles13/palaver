//! The loudness of one frame, as the `audio_level` carried on every audio packet.
//!
//! The scale is 0 to 127, louder is higher: 127 is a full-scale signal and each step down is
//! one decibel, so a level of `L` means an RMS of `L - 127` dBFS. (It is the scale of RFC
//! 6464 turned over, so that a larger number is a louder voice.) Ordinary speech into a
//! well-set microphone is around 95 to 110; a quiet room is around 60 to 70.
//!
//! It is computed on the frame as captured, with no smoothing, so it lags by nothing.

/// The level of a full-scale signal, and the top of the scale.
pub const MAX_LEVEL: u8 = 127;

/// The level of `pcm`, whose samples are in the range -1 to 1.
pub fn audio_level(pcm: &[f32]) -> u8 {
    if pcm.is_empty() {
        return 0;
    }
    let energy: f64 = pcm.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    let mean = energy / pcm.len() as f64;
    if mean <= 0.0 {
        return 0;
    }
    let dbfs = 10.0 * mean.log10();
    let level = (f64::from(MAX_LEVEL) + dbfs).round();
    // The cast saturates and the clamp has already made it fit.
    level.clamp(0.0, f64::from(MAX_LEVEL)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant(value: f32) -> Vec<f32> {
        vec![value; 960]
    }

    #[test]
    fn full_scale_is_the_top_and_each_decibel_is_one_step() {
        assert_eq!(audio_level(&constant(1.0)), 127);
        // Half the amplitude is 6 dB down; a tenth is 20 dB down.
        assert_eq!(audio_level(&constant(0.5)), 121);
        assert_eq!(audio_level(&constant(0.1)), 107);
        assert_eq!(audio_level(&constant(0.001)), 67);
    }

    #[test]
    fn silence_and_nothing_are_zero() {
        assert_eq!(audio_level(&constant(0.0)), 0);
        assert_eq!(audio_level(&[]), 0);
        // Far below anything a microphone gives.
        assert_eq!(audio_level(&constant(1e-9)), 0);
    }

    #[test]
    fn it_is_the_rms_that_counts_not_the_peak_or_the_sign() {
        assert_eq!(audio_level(&constant(-0.5)), 121);
        // A full-scale sine has an RMS 3 dB below its peak.
        let sine: Vec<f32> = (0..960)
            .map(|i| (i as f32 * std::f32::consts::TAU * 1000.0 / 48_000.0).sin())
            .collect();
        assert_eq!(audio_level(&sine), 124);
        // Out-of-range samples do not overflow the scale.
        assert_eq!(audio_level(&constant(4.0)), 127);
    }
}
