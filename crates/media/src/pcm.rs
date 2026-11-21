//! Between a sound device's samples and the pipeline's frames.
//!
//! A device has its own rate, its own number of channels, and hands over or asks for
//! however many samples it likes at a time. The pipeline works in frames of 20 ms at
//! 48 kHz, in one channel or two. [`Capture`] turns what a microphone gives into frames;
//! [`Playback`] turns frames into what a speaker asks for. Neither touches a device: they
//! are given samples and give samples, so they are tested without one.
//!
//! Where the device and the pipeline differ in channels:
//!
//! | Device | Pipeline | What happens |
//! |---|---|---|
//! | several | one | captured: all averaged; played: the one goes to the first two |
//! | one | two | captured: the one on both sides; played: the two averaged |
//! | more than two | two | the device's first two are left and right; the rest are silent |

use crate::resample::Resampler;
use palaver_opus::{Channels, SAMPLE_RATE};
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

/// One channel of a stream on its way from one rate to another.
#[derive(Debug)]
struct Lane {
    /// `None` when both ends run at the same rate.
    resampler: Option<Resampler>,
    /// This channel's part of what was last given.
    given: Vec<f32>,
    converted: Vec<f32>,
}

impl Lane {
    fn new(from: u32, to: u32) -> Option<Self> {
        Some(Self {
            resampler: if from == to {
                None
            } else {
                Some(Resampler::new(from, to)?)
            },
            given: Vec::new(),
            converted: Vec::new(),
        })
    }

    /// Bring `given` to the other rate, and return it.
    fn convert(&mut self) -> &[f32] {
        match &mut self.resampler {
            None => &self.given,
            Some(resampler) => {
                self.converted.clear();
                resampler.process(&self.given, &mut self.converted);
                &self.converted
            }
        }
    }
}

/// `given` lanes brought to the other rate and interleaved onto the end of `ready`.
/// Lanes fed the same number of samples give the same number back.
fn convert_onto(lanes: &mut [Lane], ready: &mut VecDeque<f32>) {
    match lanes {
        [one] => ready.extend(one.convert()),
        [left, right] => {
            let (left, right) = (left.convert(), right.convert());
            ready.extend(left.iter().zip(right).flat_map(|(l, r)| [*l, *r]));
        }
        _ => {}
    }
}

fn lanes(channels: Channels, from: u32, to: u32) -> Option<Vec<Lane>> {
    (0..channels.count()).map(|_| Lane::new(from, to)).collect()
}

/// A microphone's stream turned into the pipeline's frames.
#[derive(Debug)]
pub struct Capture {
    device_channels: usize,
    channels: Channels,
    lanes: Vec<Lane>,
    /// Samples at the pipeline's rate not yet handed out as a frame, interleaved.
    ready: VecDeque<f32>,
}

impl Capture {
    /// Frames of `channels` from a device of `format`. `None` if the format has no
    /// channels or no rate.
    pub fn new(format: Format, channels: Channels) -> Option<Self> {
        if format.channels == 0 {
            return None;
        }
        Some(Self {
            device_channels: usize::from(format.channels),
            channels,
            lanes: lanes(channels, format.rate, SAMPLE_RATE)?,
            ready: VecDeque::new(),
        })
    }

    /// Take what the device captured: its channels interleaved. A trailing part of a
    /// sample group is dropped.
    pub fn push(&mut self, interleaved: &[f32]) {
        let groups = interleaved.chunks_exact(self.device_channels);
        match self.lanes.as_mut_slice() {
            [one] => {
                one.given.clear();
                one.given
                    .extend(groups.map(|group| group.iter().sum::<f32>() / group.len() as f32));
            }
            [left, right] => {
                left.given.clear();
                right.given.clear();
                for group in groups {
                    let first = group.first().copied().unwrap_or(0.0);
                    left.given.push(first);
                    right.given.push(group.get(1).copied().unwrap_or(first));
                }
            }
            _ => {}
        }
        convert_onto(&mut self.lanes, &mut self.ready);
    }

    /// The next whole frame, if there is one: `frame` must be as long as a frame of the
    /// pipeline's channels ([`Channels::frame_len`]).
    pub fn pop(&mut self, frame: &mut [f32]) -> bool {
        let len = self.channels.frame_len();
        if frame.len() != len || self.ready.len() < len {
            return false;
        }
        for (to, from) in frame.iter_mut().zip(self.ready.drain(..len)) {
            *to = from;
        }
        true
    }

    /// How much sound waits for a frame's worth to gather, in samples per channel.
    pub fn waiting(&self) -> usize {
        self.ready.len() / self.channels.count()
    }
}

/// The pipeline's frames turned into a speaker's stream.
#[derive(Debug)]
pub struct Playback {
    device_channels: usize,
    channels: Channels,
    lanes: Vec<Lane>,
    /// Samples at the device's rate that it has not yet asked for, in the pipeline's
    /// channels, interleaved.
    ready: VecDeque<f32>,
    /// The most to hold, in samples per channel. A device that asks for less than it is
    /// given would otherwise fall ever further behind the present.
    limit: usize,
    /// Samples (per channel) the device asked for and there were none to give.
    starved: u64,
    /// Samples (per channel) thrown away because the device was not asking for them
    /// fast enough.
    dropped: u64,
}

impl Playback {
    /// Frames of `channels` to a device of `format`. `limit_ms` is how much sound may
    /// wait for the device at the most; a frame that would take it past that pushes the
    /// oldest out. `None` if the format has no channels or no rate.
    pub fn new(format: Format, channels: Channels, limit_ms: u32) -> Option<Self> {
        if format.channels == 0 || format.rate == 0 {
            return None;
        }
        let limit = u64::from(format.rate).saturating_mul(u64::from(limit_ms)) / 1000;
        Some(Self {
            device_channels: usize::from(format.channels),
            channels,
            lanes: lanes(channels, SAMPLE_RATE, format.rate)?,
            ready: VecDeque::new(),
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
            starved: 0,
            dropped: 0,
        })
    }

    /// Give one frame to be played, in the pipeline's channels.
    pub fn push(&mut self, frame: &[f32]) {
        match self.lanes.as_mut_slice() {
            [one] => {
                one.given.clear();
                one.given.extend_from_slice(frame);
            }
            [left, right] => {
                left.given.clear();
                right.given.clear();
                for pair in frame.chunks_exact(2) {
                    left.given.extend(pair.first());
                    right.given.extend(pair.get(1));
                }
            }
            _ => {}
        }
        convert_onto(&mut self.lanes, &mut self.ready);
        let extra = self.waiting().saturating_sub(self.limit);
        self.ready
            .drain(..extra.saturating_mul(self.channels.count()));
        self.dropped = self.dropped.saturating_add(extra as u64);
    }

    /// Fill what the device asks for. Where there is nothing to give, silence.
    pub fn fill(&mut self, interleaved: &mut [f32]) {
        for group in interleaved.chunks_mut(self.device_channels) {
            let first = self.ready.pop_front();
            let second = match self.channels {
                Channels::Mono => first,
                Channels::Stereo => self.ready.pop_front(),
            };
            let (Some(left), Some(right)) = (first, second) else {
                self.starved = self.starved.saturating_add(1);
                group.fill(0.0);
                continue;
            };
            match group {
                [only] => *only = (left + right) / 2.0,
                [first, second, rest @ ..] => {
                    *first = left;
                    *second = right;
                    rest.fill(0.0);
                }
                [] => {}
            }
        }
    }

    /// How much sound waits for the device, in samples per channel at its rate.
    pub fn waiting(&self) -> usize {
        self.ready.len() / self.channels.count()
    }

    /// Samples asked for that could not be given, and samples dropped for being late;
    /// both per channel.
    pub fn starved_and_dropped(&self) -> (u64, u64) {
        (self.starved, self.dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use palaver_opus::FRAME_SAMPLES;
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
        let mut capture = Capture::new(MONO_48, Channels::Mono).unwrap();
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
        let mut capture = Capture::new(stereo, Channels::Mono).unwrap();
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
    fn playback_fills_both_channels_and_gives_silence_when_it_has_nothing() {
        let stereo = Format {
            rate: 48_000,
            channels: 2,
        };
        let mut playback = Playback::new(stereo, Channels::Mono, 200).unwrap();
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
        let mut playback = Playback::new(MONO_48, Channels::Mono, 40).unwrap();
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
        let mut playback = Playback::new(device, Channels::Mono, 2000).unwrap();
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

    const STEREO_48: Format = Format {
        rate: 48_000,
        channels: 2,
    };

    /// `count` pairs of two steady levels, interleaved.
    fn two_levels(left: f32, right: f32, count: usize) -> Vec<f32> {
        (0..count).flat_map(|_| [left, right]).collect()
    }

    #[test]
    fn two_channels_are_kept_apart_from_the_device_to_the_frame_and_back() {
        let device = Format {
            rate: 44_100,
            channels: 2,
        };
        let mut capture = Capture::new(device, Channels::Stereo).unwrap();
        let mut playback = Playback::new(device, Channels::Stereo, 2000).unwrap();
        capture.push(&two_levels(0.3, -0.6, 44_100));
        let mut frame = [0.0; 2 * FRAME_SAMPLES];
        // A frame of one channel's length is not a frame of two.
        assert!(!capture.pop(&mut [0.0; FRAME_SAMPLES]));
        let mut frames = 0;
        while capture.pop(&mut frame) {
            frames += 1;
            if frames > 2 {
                assert!(
                    frame
                        .chunks_exact(2)
                        .all(|pair| (pair[0] - 0.3).abs() < 1e-3 && (pair[1] + 0.6).abs() < 1e-3),
                    "{frames}"
                );
            }
            playback.push(&frame);
        }
        assert_eq!(frames, 50);
        assert!(capture.waiting() < FRAME_SAMPLES);
        // Back at the device's rate, a second of it less what the rate changes hold.
        assert!(
            (43_900..=44_100).contains(&playback.waiting()),
            "{}",
            playback.waiting()
        );
        let mut out = vec![9.0; 2 * 43_900];
        playback.fill(&mut out);
        assert!(
            out.chunks_exact(2)
                .skip(400)
                .all(|pair| (pair[0] - 0.3).abs() < 1e-3 && (pair[1] + 0.6).abs() < 1e-3)
        );
        assert_eq!(playback.starved_and_dropped(), (0, 0));
    }

    #[test]
    fn one_channel_from_the_device_is_put_on_both_sides_of_a_frame_of_two() {
        let mut capture = Capture::new(MONO_48, Channels::Stereo).unwrap();
        let input: Vec<f32> = (0..FRAME_SAMPLES).map(|n| n as f32).collect();
        capture.push(&input);
        let mut frame = [0.0; 2 * FRAME_SAMPLES];
        assert!(capture.pop(&mut frame));
        assert!(
            frame
                .chunks_exact(2)
                .zip(&input)
                .all(|(pair, sample)| pair == [*sample, *sample])
        );
    }

    #[test]
    fn a_device_with_more_channels_than_two_captures_and_plays_on_its_first_two() {
        let surround = Format {
            rate: 48_000,
            channels: 6,
        };
        let mut capture = Capture::new(surround, Channels::Stereo).unwrap();
        let input: Vec<f32> = (0..FRAME_SAMPLES)
            .flat_map(|_| [0.1, 0.2, 0.3, 0.4, 0.5, 0.6])
            .collect();
        capture.push(&input);
        let mut frame = [0.0; 2 * FRAME_SAMPLES];
        assert!(capture.pop(&mut frame));
        assert!(frame.chunks_exact(2).all(|pair| pair == [0.1, 0.2]));

        let mut playback = Playback::new(surround, Channels::Stereo, 200).unwrap();
        playback.push(&frame);
        let mut out = [9.0; 12];
        playback.fill(&mut out);
        assert_eq!(out[..6], [0.1, 0.2, 0.0, 0.0, 0.0, 0.0]);

        // One channel in the pipeline goes to the same two.
        let mut playback = Playback::new(surround, Channels::Mono, 200).unwrap();
        playback.push(&[0.7; FRAME_SAMPLES]);
        playback.fill(&mut out);
        assert_eq!(out[6..], [0.7, 0.7, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn two_channels_played_on_a_device_with_one_are_averaged() {
        let mut playback = Playback::new(MONO_48, Channels::Stereo, 40).unwrap();
        playback.push(&two_levels(0.2, 0.6, FRAME_SAMPLES));
        let mut out = [9.0; 3];
        playback.fill(&mut out);
        assert!(
            out.iter().all(|sample| (sample - 0.4).abs() < 1e-6),
            "{out:?}"
        );
        assert_eq!(playback.waiting(), FRAME_SAMPLES - 3);
        // The limit is in time, whatever the channels: 40 ms is two frames.
        for _ in 0..4 {
            playback.push(&two_levels(0.2, 0.6, FRAME_SAMPLES));
        }
        assert_eq!(playback.waiting(), 2 * FRAME_SAMPLES);
        assert_eq!(
            playback.starved_and_dropped(),
            (0, 3 * FRAME_SAMPLES as u64 - 3)
        );
        // Asked for more than there is: the rest is silence, counted.
        let mut playback = Playback::new(STEREO_48, Channels::Stereo, 40).unwrap();
        playback.push(&two_levels(0.2, 0.6, FRAME_SAMPLES));
        let mut out = vec![9.0; 2 * FRAME_SAMPLES + 4];
        playback.fill(&mut out);
        assert_eq!(out[2 * FRAME_SAMPLES - 2..], [0.2, 0.6, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(playback.starved_and_dropped(), (2, 0));
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
        assert!(Capture::new(none, Channels::Mono).is_none());
        assert!(Capture::new(still, Channels::Stereo).is_none());
        assert!(Playback::new(none, Channels::Stereo, 100).is_none());
        assert!(Playback::new(still, Channels::Mono, 100).is_none());
    }
}
