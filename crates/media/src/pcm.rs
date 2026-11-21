//! Between a sound device's samples and the pipeline's frames.
//!
//! A device has its own rate, its own number of channels, and hands over or asks for
//! however many samples it likes at a time. The pipeline works in mono frames of 20 ms at
//! 48 kHz. [`Capture`] turns what a microphone gives into frames; [`Playback`] turns
//! frames into what a speaker asks for. Neither touches a device: they are given samples
//! and give samples, so they are tested without one.

use crate::resample::Resampler;
use palaver_opus::{FRAME_SAMPLES, SAMPLE_RATE};
use std::collections::VecDeque;

/// A 16-bit sample as the pipeline's float, between -1 and 1.
pub fn from_i16(sample: i16) -> f32 {
    f32::from(sample) / 32768.0
}

/// The pipeline's float as a 16-bit sample. Anything outside -1 to 1 is held at the end
/// of the range rather than wrapped round.
pub fn to_i16(sample: f32) -> i16 {
    (sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// The shape of a device's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    /// Samples a second, per channel.
    pub rate: u32,
    /// How many channels are interleaved in the stream.
    pub channels: u16,
}

/// A microphone's stream turned into the pipeline's frames.
#[derive(Debug)]
pub struct Capture {
    channels: usize,
    /// `None` when the device already runs at the pipeline's rate.
    resampler: Option<Resampler>,
    mono: Vec<f32>,
    /// Samples at the pipeline's rate not yet handed out as a frame.
    ready: VecDeque<f32>,
}

impl Capture {
    /// `None` if the format has no channels or no rate.
    pub fn new(format: Format) -> Option<Self> {
        if format.channels == 0 {
            return None;
        }
        let resampler = if format.rate == SAMPLE_RATE {
            None
        } else {
            Some(Resampler::new(format.rate, SAMPLE_RATE)?)
        };
        Some(Self {
            channels: usize::from(format.channels),
            resampler,
            mono: Vec::new(),
            ready: VecDeque::new(),
        })
    }

    /// Take what the device captured: its channels interleaved. They are averaged into
    /// one. A trailing part of a sample group is dropped.
    pub fn push(&mut self, interleaved: &[f32]) {
        self.mono.clear();
        self.mono.extend(
            interleaved
                .chunks_exact(self.channels)
                .map(|group| group.iter().sum::<f32>() / group.len() as f32),
        );
        match &mut self.resampler {
            None => self.ready.extend(&self.mono),
            Some(resampler) => {
                let mut converted = Vec::new();
                resampler.process(&self.mono, &mut converted);
                self.ready.extend(converted);
            }
        }
    }

    /// The next whole frame, if there is one: `frame` must be [`FRAME_SAMPLES`] long.
    pub fn pop(&mut self, frame: &mut [f32]) -> bool {
        if frame.len() != FRAME_SAMPLES || self.ready.len() < FRAME_SAMPLES {
            return false;
        }
        for (to, from) in frame.iter_mut().zip(self.ready.drain(..FRAME_SAMPLES)) {
            *to = from;
        }
        true
    }

    /// How many samples wait for a frame's worth to gather.
    pub fn waiting(&self) -> usize {
        self.ready.len()
    }
}

/// The pipeline's frames turned into a speaker's stream.
#[derive(Debug)]
pub struct Playback {
    channels: usize,
    resampler: Option<Resampler>,
    /// Mono samples at the device's rate that it has not yet asked for.
    ready: VecDeque<f32>,
    /// The most to hold. A device that asks for less than it is given would otherwise
    /// fall ever further behind the present.
    limit: usize,
    /// Samples the device asked for and there were none to give.
    starved: u64,
    /// Samples thrown away because the device was not asking for them fast enough.
    dropped: u64,
}

impl Playback {
    /// `limit_ms` is how much sound may wait for the device at the most; a frame that
    /// would take it past that pushes the oldest out. `None` if the format has no
    /// channels or no rate.
    pub fn new(format: Format, limit_ms: u32) -> Option<Self> {
        if format.channels == 0 || format.rate == 0 {
            return None;
        }
        let resampler = if format.rate == SAMPLE_RATE {
            None
        } else {
            Some(Resampler::new(SAMPLE_RATE, format.rate)?)
        };
        let limit = u64::from(format.rate).saturating_mul(u64::from(limit_ms)) / 1000;
        Some(Self {
            channels: usize::from(format.channels),
            resampler,
            ready: VecDeque::new(),
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
            starved: 0,
            dropped: 0,
        })
    }

    /// Give one frame to be played.
    pub fn push(&mut self, frame: &[f32]) {
        match &mut self.resampler {
            None => self.ready.extend(frame),
            Some(resampler) => {
                let mut converted = Vec::new();
                resampler.process(frame, &mut converted);
                self.ready.extend(converted);
            }
        }
        let extra = self.ready.len().saturating_sub(self.limit);
        self.ready.drain(..extra);
        self.dropped = self.dropped.saturating_add(extra as u64);
    }

    /// Fill what the device asks for: every channel gets the same sound. Where there is
    /// nothing to give, silence.
    pub fn fill(&mut self, interleaved: &mut [f32]) {
        for group in interleaved.chunks_mut(self.channels) {
            let sample = self.ready.pop_front().unwrap_or_else(|| {
                self.starved = self.starved.saturating_add(1);
                0.0
            });
            group.fill(sample);
        }
    }

    /// How much sound waits for the device, in samples at its rate.
    pub fn waiting(&self) -> usize {
        self.ready.len()
    }

    /// Samples asked for that could not be given, and samples dropped for being late.
    pub fn starved_and_dropped(&self) -> (u64, u64) {
        (self.starved, self.dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const MONO_48: Format = Format {
        rate: 48_000,
        channels: 1,
    };

    #[test]
    fn sixteen_bit_samples_convert_both_ways_and_are_held_at_the_ends() {
        assert_eq!(from_i16(0), 0.0);
        assert_eq!(from_i16(i16::MIN), -1.0);
        assert!(from_i16(i16::MAX) < 1.0);
        for sample in [0i16, 1, -1, 12345, -12345, i16::MAX, i16::MIN] {
            assert_eq!(to_i16(from_i16(sample)), sample);
        }
        assert_eq!(to_i16(7.0), i16::MAX);
        assert_eq!(to_i16(-7.0), i16::MIN);
        assert_eq!(to_i16(f32::NAN), 0);
    }

    #[test]
    fn capture_gives_whole_frames_whatever_sizes_the_device_hands_over() {
        let mut capture = Capture::new(MONO_48).unwrap();
        let input: Vec<f32> = (0..3000).map(|n| n as f32).collect();
        let mut frame = [0.0; FRAME_SAMPLES];
        assert!(!capture.pop(&mut frame));
        let mut frames = Vec::new();
        for piece in input.chunks(441) {
            capture.push(piece);
            while capture.pop(&mut frame) {
                frames.extend_from_slice(&frame);
            }
        }
        // Three frames of 960, in order, and the rest waits.
        assert_eq!(frames, input[..2880]);
        assert_eq!(capture.waiting(), 120);
        // A buffer of the wrong length is not a frame.
        assert!(!capture.pop(&mut [0.0; 100]));
    }

    #[test]
    fn capture_averages_channels_and_brings_the_rate_to_the_pipelines() {
        let stereo = Format {
            rate: 44_100,
            channels: 2,
        };
        let mut capture = Capture::new(stereo).unwrap();
        // A second of a tone on the left and its mirror image plus a steady 0.4 on the
        // right: the tone cancels and half of 0.4 is left.
        let mut input = Vec::new();
        for n in 0..44_100 {
            let tone = (TAU * 440.0 * n as f32 / 44_100.0).sin();
            input.extend_from_slice(&[tone, 0.4 - tone]);
        }
        capture.push(&input);
        let mut frame = [0.0; FRAME_SAMPLES];
        let mut frames = 0;
        while capture.pop(&mut frame) {
            frames += 1;
            if frames > 2 {
                assert!(frame.iter().all(|s| (s - 0.2).abs() < 1e-3), "{frames}");
            }
        }
        // A second of sound.
        assert_eq!(frames, 50);
    }

    #[test]
    fn playback_fills_every_channel_and_gives_silence_when_it_has_nothing() {
        let stereo = Format {
            rate: 48_000,
            channels: 2,
        };
        let mut playback = Playback::new(stereo, 200).unwrap();
        let mut out = [9.0; 8];
        playback.fill(&mut out);
        assert_eq!(out, [0.0; 8]);
        assert_eq!(playback.starved_and_dropped(), (4, 0));

        let frame: Vec<f32> = (0..FRAME_SAMPLES).map(|n| n as f32).collect();
        playback.push(&frame);
        playback.fill(&mut out);
        assert_eq!(out, [0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
        assert_eq!(playback.waiting(), FRAME_SAMPLES - 4);
    }

    #[test]
    fn playback_drops_the_oldest_when_the_device_falls_behind() {
        // At most 40 ms may wait: two frames.
        let mut playback = Playback::new(MONO_48, 40).unwrap();
        for n in 0..5 {
            playback.push(&[n as f32; FRAME_SAMPLES]);
        }
        assert_eq!(playback.waiting(), 2 * FRAME_SAMPLES);
        assert_eq!(
            playback.starved_and_dropped(),
            (0, 3 * FRAME_SAMPLES as u64)
        );
        let mut out = [0.0; 1];
        playback.fill(&mut out);
        // The newest two were kept.
        assert_eq!(out, [3.0]);
    }

    #[test]
    fn playback_brings_the_rate_to_the_devices() {
        let device = Format {
            rate: 44_100,
            channels: 1,
        };
        let mut playback = Playback::new(device, 2000).unwrap();
        for _ in 0..50 {
            playback.push(&[0.3; FRAME_SAMPLES]);
        }
        // A second of sound at the device's rate, less what the rate change still holds.
        assert!(
            (44_000..=44_100).contains(&playback.waiting()),
            "{}",
            playback.waiting()
        );
        let mut out = vec![0.0; 44_000];
        playback.fill(&mut out);
        assert!(out[200..].iter().all(|s| (s - 0.3).abs() < 1e-3));
    }

    #[test]
    fn a_format_with_no_channels_or_no_rate_is_refused() {
        let none = Format {
            rate: 48_000,
            channels: 0,
        };
        let still = Format {
            rate: 0,
            channels: 1,
        };
        assert!(Capture::new(none).is_none());
        assert!(Capture::new(still).is_none());
        assert!(Playback::new(none, 100).is_none());
        assert!(Playback::new(still, 100).is_none());
    }
}
