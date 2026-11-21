//! The microphone and the speaker.
//!
//! This is the one crate that touches sound devices. A [`Microphone`] gives the pipeline's
//! 20 ms frames; a [`Speaker`] takes them. Whatever rate, channels and sample type the
//! device has is dealt with here and in [`palaver_media::pcm`], which does the converting
//! and is tested without a device.
//!
//! The device calls back on a thread of its own whenever it has samples or wants some.
//! Those calls and the owner's meet at a lock around the converter, held only for as long
//! as it takes to copy samples in or out.
//!
//! Both are the system's default devices, in the format the system suggests for them.
//! The device is asked to work in pieces of [`DEVICE_BUFFER_MS`]: left to themselves some
//! systems buffer seconds of sound, which is seconds of delay. A device that refuses is
//! opened with whatever it prefers.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use palaver_media::Channels;
use palaver_media::pcm::{Capture, Format, Playback};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The most sound that may wait for the speaker. More than this and what is heard has
/// fallen behind what is said; the oldest is dropped.
pub const SPEAKER_LIMIT_MS: u32 = 200;

/// How much sound the device is asked to take or give at a time.
pub const DEVICE_BUFFER_MS: u32 = 10;

/// The stream settings to try, in order: pieces of [`DEVICE_BUFFER_MS`] if the device
/// says it can, then whatever it prefers.
fn settings(config: &cpal::SupportedStreamConfig) -> Vec<cpal::StreamConfig> {
    let preferred = config.config();
    let wanted = config.sample_rate().saturating_mul(DEVICE_BUFFER_MS) / 1000;
    let small = match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } if min <= max => Some(cpal::StreamConfig {
            buffer_size: cpal::BufferSize::Fixed(wanted.clamp(*min, *max)),
            ..preferred
        }),
        _ => None,
    };
    small.into_iter().chain([preferred]).collect()
}

/// The first of `settings` that `build` can make a stream with, started.
fn start(
    settings: Vec<cpal::StreamConfig>,
    build: impl Fn(cpal::StreamConfig) -> Result<cpal::Stream, cpal::Error>,
) -> Result<cpal::Stream, SoundError> {
    let mut last = SoundError::Stream(String::from("nothing to try"));
    for config in settings {
        match build(config).and_then(|stream| stream.play().map(|()| stream)) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = SoundError::Stream(error.to_string()),
        }
    }
    Err(last)
}

/// Why a device could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundError {
    /// The system has no default device of this kind.
    NoDevice,
    /// The device would not say what format it has.
    NoFormat(String),
    /// The device's samples are of a type not handled here.
    SampleType(String),
    /// The stream could not be made or started.
    Stream(String),
}

impl core::fmt::Display for SoundError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDevice => write!(f, "no sound device"),
            Self::NoFormat(why) => write!(f, "the device gave no format: {why}"),
            Self::SampleType(which) => write!(f, "samples of type {which} are not handled"),
            Self::Stream(why) => write!(f, "the stream could not start: {why}"),
        }
    }
}

impl std::error::Error for SoundError {}

/// What a device is and how it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The system's name for it; may be empty.
    pub name: String,
    pub format: Format,
    /// The type of its samples, as the system names it.
    pub sample_type: String,
}

/// What the device gave, turned into the pipeline's samples and handed to `capture`.
/// `scratch` is reused from call to call.
pub fn take<T>(data: &[T], scratch: &mut Vec<f32>, capture: &Mutex<Capture>)
where
    T: SizedSample,
    f32: FromSample<T>,
{
    scratch.clear();
    scratch.extend(data.iter().map(|sample| f32::from_sample_(*sample)));
    // A lock is poisoned only if its other holder panicked, and then there is nobody
    // left to give the sound to.
    if let Ok(mut capture) = capture.lock() {
        capture.push(scratch);
    }
}

/// What the device asks for, filled from `playback` in the device's sample type.
/// `scratch` is reused from call to call.
pub fn give<T>(data: &mut [T], scratch: &mut Vec<f32>, playback: &Mutex<Playback>)
where
    T: SizedSample + FromSample<f32>,
{
    scratch.clear();
    scratch.resize(data.len(), 0.0);
    if let Ok(mut playback) = playback.lock() {
        playback.fill(scratch);
    }
    for (to, from) in data.iter_mut().zip(scratch.iter()) {
        *to = T::from_sample_(*from);
    }
}

fn describe(device: &cpal::Device, config: &cpal::SupportedStreamConfig) -> Device {
    Device {
        name: device
            .description()
            .map(|description| description.to_string())
            .unwrap_or_default(),
        format: Format {
            rate: config.sample_rate(),
            channels: config.channels(),
        },
        sample_type: config.sample_format().to_string(),
    }
}

/// Counts the errors a stream reports while it runs.
fn counter(errors: &Arc<AtomicU64>) -> impl FnMut(cpal::Error) + Send + 'static {
    let errors = Arc::clone(errors);
    move |_| {
        errors.fetch_add(1, Ordering::Relaxed);
    }
}

/// The default microphone, giving frames.
pub struct Microphone {
    // Dropping the stream stops it.
    _stream: cpal::Stream,
    capture: Arc<Mutex<Capture>>,
    device: Device,
    errors: Arc<AtomicU64>,
}

impl Microphone {
    /// Open the system's default microphone and start it; its frames have `channels`.
    pub fn open(channels: Channels) -> Result<Self, SoundError> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or(SoundError::NoDevice)?;
        let config = device
            .default_input_config()
            .map_err(|error| SoundError::NoFormat(error.to_string()))?;
        let described = describe(&device, &config);
        let capture = Capture::new(described.format, channels)
            .ok_or_else(|| SoundError::NoFormat(String::from("no channels or no rate")))?;
        let capture = Arc::new(Mutex::new(capture));
        let errors = Arc::new(AtomicU64::new(0));

        fn build<T>(
            device: &cpal::Device,
            config: cpal::StreamConfig,
            capture: Arc<Mutex<Capture>>,
            errors: &Arc<AtomicU64>,
        ) -> Result<cpal::Stream, cpal::Error>
        where
            T: SizedSample,
            f32: FromSample<T>,
        {
            let mut scratch = Vec::new();
            device.build_input_stream(
                config,
                move |data: &[T], _: &cpal::InputCallbackInfo| take(data, &mut scratch, &capture),
                counter(errors),
                None,
            )
        }
        let format = config.sample_format();
        if !matches!(
            format,
            SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16 | SampleFormat::I32
        ) {
            return Err(SoundError::SampleType(format.to_string()));
        }
        let stream = start(settings(&config), |settings| {
            let shared = Arc::clone(&capture);
            match format {
                SampleFormat::I16 => build::<i16>(&device, settings, shared, &errors),
                SampleFormat::U16 => build::<u16>(&device, settings, shared, &errors),
                SampleFormat::I32 => build::<i32>(&device, settings, shared, &errors),
                _ => build::<f32>(&device, settings, shared, &errors),
            }
        })?;
        Ok(Self {
            _stream: stream,
            capture,
            device: described,
            errors,
        })
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    /// The next whole frame, if one has gathered: `frame` must be as long as a frame of
    /// the channels the microphone was opened with.
    pub fn pop(&self, frame: &mut [f32]) -> bool {
        self.capture
            .lock()
            .is_ok_and(|mut capture| capture.pop(frame))
    }

    /// How many errors the device has reported since it was opened.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }
}

/// The default speaker, taking frames.
pub struct Speaker {
    _stream: cpal::Stream,
    playback: Arc<Mutex<Playback>>,
    device: Device,
    errors: Arc<AtomicU64>,
}

impl Speaker {
    /// Open the system's default speaker and start it; the frames it is given have
    /// `channels`.
    pub fn open(channels: Channels) -> Result<Self, SoundError> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or(SoundError::NoDevice)?;
        let config = device
            .default_output_config()
            .map_err(|error| SoundError::NoFormat(error.to_string()))?;
        let described = describe(&device, &config);
        let playback = Playback::new(described.format, channels, SPEAKER_LIMIT_MS)
            .ok_or_else(|| SoundError::NoFormat(String::from("no channels or no rate")))?;
        let playback = Arc::new(Mutex::new(playback));
        let errors = Arc::new(AtomicU64::new(0));

        fn build<T>(
            device: &cpal::Device,
            config: cpal::StreamConfig,
            playback: Arc<Mutex<Playback>>,
            errors: &Arc<AtomicU64>,
        ) -> Result<cpal::Stream, cpal::Error>
        where
            T: SizedSample + FromSample<f32>,
        {
            let mut scratch = Vec::new();
            device.build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    give(data, &mut scratch, &playback)
                },
                counter(errors),
                None,
            )
        }
        let format = config.sample_format();
        if !matches!(
            format,
            SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16 | SampleFormat::I32
        ) {
            return Err(SoundError::SampleType(format.to_string()));
        }
        let stream = start(settings(&config), |settings| {
            let shared = Arc::clone(&playback);
            match format {
                SampleFormat::I16 => build::<i16>(&device, settings, shared, &errors),
                SampleFormat::U16 => build::<u16>(&device, settings, shared, &errors),
                SampleFormat::I32 => build::<i32>(&device, settings, shared, &errors),
                _ => build::<f32>(&device, settings, shared, &errors),
            }
        })?;
        Ok(Self {
            _stream: stream,
            playback,
            device: described,
            errors,
        })
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Give one frame to be played.
    pub fn push(&self, frame: &[f32]) {
        if let Ok(mut playback) = self.playback.lock() {
            playback.push(frame);
        }
    }

    /// Samples (per channel) the device asked for when there were none, and samples
    /// dropped because it was not asking fast enough.
    pub fn starved_and_dropped(&self) -> (u64, u64) {
        self.playback
            .lock()
            .map(|playback| playback.starved_and_dropped())
            .unwrap_or_default()
    }

    /// How many errors the device has reported since it was opened.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use palaver_media::FRAME_SAMPLES;

    const DEVICE: Format = Format {
        rate: 48_000,
        channels: 2,
    };

    #[test]
    fn sixteen_bit_samples_from_a_device_become_a_frame() {
        let capture = Mutex::new(Capture::new(DEVICE, Channels::Stereo).unwrap());
        let mut scratch = Vec::new();
        // Half scale on the left, minus a quarter on the right, in pieces as a device
        // gives them.
        let data: Vec<i16> = (0..FRAME_SAMPLES).flat_map(|_| [16_384, -8192]).collect();
        for piece in data.chunks(256) {
            take(piece, &mut scratch, &capture);
        }
        let mut frame = [0.0; 2 * FRAME_SAMPLES];
        assert!(capture.lock().unwrap().pop(&mut frame));
        assert!(frame.chunks_exact(2).all(|pair| pair == [0.5, -0.25]));
    }

    #[test]
    fn a_frame_fills_a_device_in_its_own_sample_type() {
        let playback = Mutex::new(Playback::new(DEVICE, Channels::Stereo, 200).unwrap());
        let frame: Vec<f32> = (0..FRAME_SAMPLES).flat_map(|_| [0.5, -0.25]).collect();
        playback.lock().unwrap().push(&frame);
        let mut scratch = Vec::new();

        let mut signed = [7i16; 4];
        give(&mut signed, &mut scratch, &playback);
        assert_eq!(signed, [16_384, -8192, 16_384, -8192]);
        // Unsigned samples have silence in the middle of their range.
        let mut unsigned = [7u16; 2];
        give(&mut unsigned, &mut scratch, &playback);
        assert_eq!(unsigned, [49_152, 24_576]);
        let mut float = [7.0f32; 2];
        give(&mut float, &mut scratch, &playback);
        assert_eq!(float, [0.5, -0.25]);

        // With nothing left to give, the device gets silence in its own terms.
        let playback = Mutex::new(Playback::new(DEVICE, Channels::Stereo, 200).unwrap());
        let mut unsigned = [7u16; 2];
        give(&mut unsigned, &mut scratch, &playback);
        assert_eq!(unsigned, [32_768, 32_768]);
    }
}
