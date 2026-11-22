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
    AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, NoiseSuppression,
    NoiseSuppressionLevel, Pipeline, TransparentModeType,
};
use sonora::{AudioProcessing, Config, StreamConfig};

/// The processor works on 10 ms at a time: half a frame.
const CHUNK: usize = FRAME_SAMPLES / 2;

/// How hard steady noise is turned down. The more, the more a voice is coloured by it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NoiseStrength {
    /// About 6 dB.
    Low,
    /// About 12 dB: enough for an ordinary microphone in an ordinary room.
    #[default]
    Moderate,
    /// About 18 dB: for a microphone with much noise of its own.
    High,
    /// About 21 dB.
    VeryHigh,
}

/// Which kinds of cleaning are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub echo: bool,
    pub noise: bool,
    /// How hard, when `noise` is on.
    pub noise_strength: NoiseStrength,
    pub level: bool,
}

impl Default for Settings {
    /// Everything on.
    fn default() -> Self {
        Self {
            echo: true,
            noise: true,
            noise_strength: NoiseStrength::Moderate,
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
    settings: Settings,
    captured: Planar,
    played: Planar,
}

impl Processor {
    /// What the processor is told to do, for these settings and channels.
    fn config(settings: Settings, captured: Channels, played: Channels) -> Config {
        Config {
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
            noise_suppression: settings.noise.then(|| NoiseSuppression {
                level: match settings.noise_strength {
                    NoiseStrength::Low => NoiseSuppressionLevel::Low,
                    NoiseStrength::Moderate => NoiseSuppressionLevel::Moderate,
                    NoiseStrength::High => NoiseSuppressionLevel::High,
                    NoiseStrength::VeryHigh => NoiseSuppressionLevel::VeryHigh,
                },
                ..NoiseSuppression::default()
            }),
            gain_controller2: settings.level.then(|| GainController2 {
                // The part that follows the voice's level; without it the controller
                // only limits.
                adaptive_digital: Some(AdaptiveDigital::default()),
                ..GainController2::default()
            }),
            ..Config::default()
        }
    }

    /// Turn steady noise down as hard as `strength` from here on, without starting
    /// again: what has been learnt of the room and of the voice's level is kept.
    pub fn set_noise_strength(&mut self, strength: NoiseStrength) {
        if self.settings.noise_strength == strength {
            return;
        }
        self.settings.noise_strength = strength;
        self.apm.apply_config(Self::config(
            self.settings,
            self.captured.channels,
            self.played.channels,
        ));
    }

    /// How hard steady noise is being turned down, if it is at all.
    #[must_use]
    pub fn noise_strength(&self) -> Option<NoiseStrength> {
        self.settings.noise.then_some(self.settings.noise_strength)
    }

    /// For a microphone whose frames have `captured` channels beside a speaker whose
    /// frames have `played`.
    #[must_use]
    pub fn new(settings: Settings, captured: Channels, played: Channels) -> Self {
        let config = Self::config(settings, captured, played);
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
            settings,
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
