//! Cleaning what the microphone hears: echo, noise and level.
//!
//! - **Echo.** The speaker plays the others' voices and the microphone hears them. Sent
//!   back, each of them would hear themselves a moment late. The processor is shown every
//!   frame on its way to the speaker and takes out of the microphone's frames what it
//!   recognises of them.
//! - **Noise.** Steady sound that is not a voice (a fan, a hum) is turned down.
//! - **Level.** A quiet voice is brought up and a loud one held down, so that people come
//!   out about as loud as each other.
//!
//! The work is done by `sonora`, a Rust port of WebRTC's audio processing. It is for a
//! voice at a microphone: a track of music or shared computer sound is sent without it.
//!
//! The owner calls [`Processor::played`] with each frame given to the speaker and
//! [`Processor::captured`] with each frame from the microphone, in the order they happen.

use palaver_opus::{Channels, FRAME_SAMPLES, SAMPLE_RATE};
use sonora::config::{
    AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, NoiseSuppression, Pipeline,
    TransparentModeType,
};
use sonora::{AudioProcessing, Config, StreamConfig};

/// The processor works on 10 ms at a time: half a frame.
const CHUNK: usize = FRAME_SAMPLES / 2;

/// Which kinds of cleaning are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub echo: bool,
    pub noise: bool,
    pub level: bool,
}

impl Default for Settings {
    /// Everything on.
    fn default() -> Self {
        Self {
            echo: true,
            noise: true,
            level: true,
        }
    }
}

/// A frame was not the length its channels make it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrongLength;

/// One 10 ms piece with its channels apart, which is how the processor wants them.
#[derive(Debug)]
struct Planar {
    channels: Channels,
    /// One channel after the other, [`CHUNK`] samples each.
    samples: Vec<f32>,
    /// Where the processor writes.
    out: Vec<f32>,
}

impl Planar {
    fn new(channels: Channels) -> Self {
        let len = CHUNK.saturating_mul(channels.count());
        Self {
            channels,
            samples: vec![0.0; len],
            out: vec![0.0; len],
        }
    }

    /// Take half a frame of interleaved samples apart.
    fn load(&mut self, interleaved: &[f32]) {
        let count = self.channels.count();
        for (channel, lane) in self.samples.chunks_exact_mut(CHUNK).enumerate() {
            let source = interleaved.iter().skip(channel).step_by(count);
            for (to, from) in lane.iter_mut().zip(source) {
                *to = *from;
            }
        }
    }

    /// Put what the processor wrote back together.
    fn store(&self, interleaved: &mut [f32]) {
        let count = self.channels.count();
        for (channel, lane) in self.out.chunks_exact(CHUNK).enumerate() {
            let target = interleaved.iter_mut().skip(channel).step_by(count);
            for (to, from) in target.zip(lane) {
                *to = *from;
            }
        }
    }

    /// Run `process` over the piece: it is given each channel to read and to write.
    fn run(
        &mut self,
        process: impl FnOnce(&[&[f32]], &mut [&mut [f32]]) -> Result<(), sonora::Error>,
    ) -> Result<(), WrongLength> {
        let done = match self.channels {
            Channels::Mono => process(&[&self.samples], &mut [&mut self.out]),
            Channels::Stereo => {
                let from: [&[f32]; 2] = self.samples.split_at(CHUNK).into();
                let mut to: [&mut [f32]; 2] = self.out.split_at_mut(CHUNK).into();
                process(&from, &mut to)
            }
        };
        // The pieces are made to the lengths the processor was told of.
        done.map_err(|_| WrongLength)
    }
}

/// Cleans one microphone's frames, knowing what the speaker plays.
pub struct Processor {
    apm: AudioProcessing,
    captured: Planar,
    played: Planar,
}

impl Processor {
    /// For a microphone whose frames have `captured` channels beside a speaker whose
    /// frames have `played`.
    pub fn new(settings: Settings, captured: Channels, played: Channels) -> Self {
        let config = Config {
            pipeline: Pipeline {
                multi_channel_capture: captured == Channels::Stereo,
                multi_channel_render: played == Channels::Stereo,
                ..Pipeline::default()
            },
            // Takes out rumble below the voice, which the echo canceller wants gone.
            high_pass_filter: Some(HighPassFilter::default()),
            echo_canceller: settings.echo.then(|| EchoCanceller {
                // How it tells that there is no echo to cancel (headphones). The other
                // way, the default, took eight seconds of the others talking to notice,
                // and until then took whoever spoke at the microphone for an echo and
                // all but silenced them.
                transparent_mode: TransparentModeType::Hmm,
                ..EchoCanceller::default()
            }),
            noise_suppression: settings.noise.then(NoiseSuppression::default),
            gain_controller2: settings.level.then(|| GainController2 {
                // The part that follows the voice's level; without it the controller
                // only limits.
                adaptive_digital: Some(AdaptiveDigital::default()),
                ..GainController2::default()
            }),
            ..Config::default()
        };
        let stream = |channels: Channels| {
            StreamConfig::new(
                SAMPLE_RATE,
                u16::try_from(channels.count()).unwrap_or(u16::MAX),
            )
        };
        Self {
            apm: AudioProcessing::builder()
                .config(config)
                .capture_config(stream(captured))
                .render_config(stream(played))
                .build(),
            captured: Planar::new(captured),
            played: Planar::new(played),
        }
    }

    /// A frame is on its way to the speaker: remember it, to know its echo.
    ///
    /// # Errors
    ///
    /// [`WrongLength`] if `frame` is not a frame of the speaker's channels.
    pub fn played(&mut self, frame: &[f32]) -> Result<(), WrongLength> {
        if frame.len() != self.played.channels.frame_len() {
            return Err(WrongLength);
        }
        let half = frame.len() / 2;
        for piece in frame.chunks_exact(half) {
            self.played.load(piece);
            let apm = &mut self.apm;
            self.played
                .run(|from, to| apm.process_render_f32(from, to))?;
        }
        Ok(())
    }

    /// Clean a frame from the microphone, in place.
    ///
    /// # Errors
    ///
    /// [`WrongLength`] if `frame` is not a frame of the microphone's channels.
    pub fn captured(&mut self, frame: &mut [f32]) -> Result<(), WrongLength> {
        if frame.len() != self.captured.channels.frame_len() {
            return Err(WrongLength);
        }
        let half = frame.len() / 2;
        for piece in frame.chunks_exact_mut(half) {
            self.captured.load(piece);
            let apm = &mut self.apm;
            self.captured
                .run(|from, to| apm.process_capture_f32(from, to))?;
            self.captured.store(piece);
        }
        Ok(())
    }

    /// How much of the echo is being taken out, in decibels, once the processor has
    /// heard enough to say.
    pub fn echo_removed_db(&self) -> Option<f64> {
        self.apm.statistics().echo_return_loss_enhancement
    }
}
