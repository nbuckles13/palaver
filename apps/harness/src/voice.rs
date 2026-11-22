//! A call joined to a microphone and a speaker.
//!
//! The speaker keeps time. Each [`turn`](Voice::turn) tops up what waits for the speaker
//! to a little ahead of what it has taken, making frames only as the device makes room
//! for them, and sends on whatever the microphone has gathered. Nothing here waits: the
//! owner calls `turn` whenever the speaker has taken sound.
//!
//! The devices are behind two small traits, so that the same code runs against a sound
//! card and against made-up devices in a test.

use crate::check::{Ear, Finding};
use palaver_media::level::audio_level;
use palaver_media::process::{Processor, Settings};
use palaver_media::{Channels, CodecError};
use palaver_sdk::{Call, VoiceGauge};
use palaver_sound::{Microphone, Speaker};

/// The most frames made for the speaker in one turn. A speaker that never fills (a broken
/// device, or a test's) must not hold the turn for ever.
const MOST_FRAMES_A_TURN: usize = 25;

/// Where captured sound comes from.
pub trait Source {
    /// The next whole frame of one channel, if one has gathered.
    fn pop(&mut self, frame: &mut [f32]) -> bool;
}

/// Where sound to play goes.
pub trait Sink {
    /// How much waits to be played, in samples per channel at the device's rate.
    fn waiting(&self) -> usize;
    /// Give one frame to be played.
    fn push(&mut self, frame: &[f32]);
}

impl Source for Microphone {
    fn pop(&mut self, frame: &mut [f32]) -> bool {
        Self::pop(self, frame)
    }
}

impl Sink for Speaker {
    fn waiting(&self) -> usize {
        Self::waiting(self)
    }

    fn push(&mut self, frame: &[f32]) {
        Self::push(self, frame);
    }
}

/// What one turn did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Turn {
    /// Frames made for the speaker.
    pub played: usize,
    /// How many voices were in the last of them.
    pub voices: usize,
    /// Frames the microphone gave.
    pub captured: usize,
    /// How many of them were sent on: the rest were silence.
    pub sent: usize,
    /// The loudest of them after cleaning, on the scale of [`palaver_media::level`].
    pub loudest: u8,
    /// How much waited for the speaker once it was topped up, in its samples per channel.
    pub waiting: usize,
}

/// A call, a microphone and a speaker.
pub struct Voice<M, S> {
    call: Call,
    microphone: M,
    speaker: S,
    /// `None` when what the microphone gives is sent as it is.
    processor: Option<Processor>,
    /// How much is kept waiting for the speaker, in its samples per channel.
    ahead: usize,
    captured: Vec<f32>,
    played: Vec<f32>,
    /// While set, what the microphone gives is listened to (the echo canceller needs
    /// it) and not sent.
    muted: bool,
    /// Judges the microphone by what it gives, before any cleaning here.
    ear: Ear,
}

impl<M: Source, S: Sink> Voice<M, S> {
    /// `call` must send one channel and play `playout`. `ahead` is how much sound is
    /// kept waiting for the speaker, in samples per channel at its rate: enough that it
    /// does not run dry between turns, little enough not to be heard as delay.
    pub fn new(
        call: Call,
        microphone: M,
        speaker: S,
        cleaning: Option<Settings>,
        playout: Channels,
        ahead: usize,
    ) -> Self {
        Self {
            call,
            microphone,
            speaker,
            processor: cleaning.map(|settings| Processor::new(settings, Channels::Mono, playout)),
            ahead,
            captured: vec![0.0; Channels::Mono.frame_len()],
            played: vec![0.0; playout.frame_len()],
            muted: false,
            ear: Ear::default(),
        }
    }

    /// Top up the speaker and send on what the microphone has.
    ///
    /// # Errors
    ///
    /// When the encoder or a decoder fails.
    pub fn turn(&mut self) -> Result<Turn, CodecError> {
        let mut turn = Turn::default();
        while self.speaker.waiting() < self.ahead && turn.played < MOST_FRAMES_A_TURN {
            turn.voices = self.call.listen(&mut self.played)?;
            if let Some(processor) = &mut self.processor {
                processor
                    .played(&self.played)
                    .map_err(|_| CodecError::FrameLength)?;
            }
            self.speaker.push(&self.played);
            turn.played = turn.played.saturating_add(1);
        }
        turn.waiting = self.speaker.waiting();
        while self.microphone.pop(&mut self.captured) {
            self.ear.hear(&self.captured);
            if let Some(processor) = &mut self.processor {
                processor
                    .captured(&mut self.captured)
                    .map_err(|_| CodecError::FrameLength)?;
            }
            turn.captured = turn.captured.saturating_add(1);
            turn.loudest = turn.loudest.max(audio_level(&self.captured));
            if !self.muted && self.call.speak(&self.captured)? {
                turn.sent = turn.sent.saturating_add(1);
            }
        }
        Ok(turn)
    }

    /// How each voice being played is faring.
    #[must_use]
    pub fn gauges(&self) -> Vec<VoiceGauge> {
        self.call.gauges()
    }

    /// What the last few seconds say of the microphone, as the device gives it.
    #[must_use]
    pub fn microphone_finding(&self) -> Finding {
        self.ear.finding()
    }

    /// Stop sending what the microphone gives, or start again.
    pub const fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub const fn speaker(&mut self) -> &mut S {
        &mut self.speaker
    }

    pub const fn microphone(&mut self) -> &mut M {
        &mut self.microphone
    }

    /// Let go of the devices and give the call back, to be stopped.
    pub fn into_call(self) -> Call {
        self.call
    }
}
