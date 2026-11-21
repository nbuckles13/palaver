//! Mixing: several voices into the one signal that is played.

/// Adds `voice` into `out`, sample by sample. Call once per voice, then [`limit`].
pub fn add(out: &mut [f32], voice: &[f32]) {
    for (sum, sample) in out.iter_mut().zip(voice) {
        *sum += *sample;
    }
}

/// Keeps a mix within what a sound device takes.
///
/// Up to `KNEE` the signal is untouched, so one voice (or several at ordinary levels) is
/// not coloured; above it the excess is squeezed smoothly into the room that is left, so
/// loud voices together are compressed rather than clipped.
pub fn limit(out: &mut [f32]) {
    const KNEE: f32 = 0.8;
    const ROOM: f32 = 1.0 - KNEE;
    for sample in out {
        let size = sample.abs();
        if size > KNEE {
            let over = (size - KNEE) / ROOM;
            *sample = sample.signum() * (KNEE + ROOM * over / (1.0 + over));
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::float_cmp,
    reason = "adding to silence gives the samples back exactly"
)]
mod tests {
    use super::*;

    #[test]
    fn voices_add_and_ordinary_levels_pass_untouched() {
        let mut out = [0.0f32; 4];
        add(&mut out, &[0.1, -0.2, 0.3, 0.0]);
        add(&mut out, &[0.1, 0.1, -0.5, 0.0]);
        limit(&mut out);
        for (got, want) in out.iter().zip([0.2, -0.1, -0.2, 0.0]) {
            assert!((got - want).abs() < 1e-6, "{got} {want}");
        }
    }

    #[test]
    fn a_loud_mix_is_squeezed_under_full_scale_and_keeps_its_order() {
        let mut out = [0.8f32, 0.9, 1.5, 3.0, 100.0, -3.0];
        limit(&mut out);
        assert!((out[0] - 0.8).abs() < 1e-6);
        assert!(out.iter().all(|s| s.abs() < 1.0), "{out:?}");
        // Louder in is still louder out, so the shape of the sound survives.
        assert!(out[0] < out[1] && out[1] < out[2] && out[2] < out[3] && out[3] < out[4]);
        assert!((out[5] + out[3]).abs() < 1e-6);
    }

    #[test]
    fn a_shorter_voice_adds_what_it_has() {
        let mut out = [0.0f32; 3];
        add(&mut out, &[0.5]);
        assert_eq!(out, [0.5, 0.0, 0.0]);
    }
}
