//! A headless participant with a made-up voice.
//!
//! A [`Bot`] is a whole [`Participant`]: a real node, a real encoder and decoders, the same
//! call an application with a sound card uses. The only things made up are its voice
//! (a [`Voice`] at the pitch of its speaker number, talking to a [`Rhythm`]) and its ears
//! (it keeps what it was given to play, and can say whose pitch is in it).
//!
//! So a room of bots checks the product end to end by what a listener would notice: who
//! is heard, by whom, and who is not.

use palaver_media::synthetic::{Rhythm, Voice, hears, pitch_of, strength};
use palaver_media::{CodecError, FRAME_SAMPLES, SAMPLE_RATE};
use palaver_net::memory::MemoryNetwork;
use palaver_net::quic::QuicOptions;
use palaver_proto::{SECRET_BYTES, Secret};
use palaver_sdk::{Options, Participant};
use std::net::SocketAddr;
use std::time::Duration;

pub use palaver_sdk::{StartError as BotError, entry_points};

/// The length of one frame of audio.
pub const FRAME: Duration = Duration::from_millis(20);

/// How a bot sounds and when it talks.
#[derive(Debug, Clone, Copy)]
pub struct Script {
    /// Which of the made-up voices it has; see [`pitch_of`].
    pub speaker: u32,
    /// How loud its loudest moments are, 0 to 1.
    pub loudness: f32,
    pub rhythm: Rhythm,
}

impl Script {
    /// A bot that only listens.
    #[must_use]
    pub const fn silent(speaker: u32) -> Self {
        Self {
            speaker,
            loudness: 0.0,
            rhythm: Rhythm {
                talk: Duration::ZERO,
                pause: Duration::from_secs(1),
                offset: Duration::ZERO,
            },
        }
    }

    /// A bot that talks all the time.
    #[must_use]
    pub const fn talking(speaker: u32, loudness: f32) -> Self {
        Self {
            speaker,
            loudness,
            rhythm: Rhythm::ALWAYS,
        }
    }
}

/// One headless participant.
pub struct Bot {
    participant: Participant,
    script: Script,
    voice: Voice,
    /// Frames made so far.
    frames: u64,
    /// What it has been given to play, most recent last, up to [`Bot::EARS`].
    played: Vec<f32>,
    frame: Vec<f32>,
}

impl Bot {
    /// How much of what it played a bot keeps: two seconds.
    pub const EARS: usize = 2 * SAMPLE_RATE as usize;

    /// A bot on an in-memory network, for a meeting with this secret. It does nothing
    /// until its participant is told to create a meeting or to join one. Must be called
    /// inside a tokio runtime.
    ///
    /// # Errors
    ///
    /// As [`Participant::on_memory`].
    pub fn on_memory(
        network: &MemoryNetwork,
        secret: &Secret<[u8; SECRET_BYTES]>,
        name: &str,
        script: Script,
    ) -> Result<Self, BotError> {
        Participant::on_memory(network, secret, Options::named(name))
            .map(|participant| Self::with(participant, script))
    }

    /// A bot on the network, over QUIC, for a meeting with this secret; `advertise` as
    /// for [`Participant::on_quic`]. Must be called inside a tokio runtime.
    ///
    /// # Errors
    ///
    /// As [`Participant::on_quic`].
    pub fn on_quic(
        options: QuicOptions,
        advertise: Vec<SocketAddr>,
        secret: &Secret<[u8; SECRET_BYTES]>,
        name: &str,
        script: Script,
    ) -> Result<Self, BotError> {
        Participant::on_quic(options, advertise, secret, Options::named(name))
            .map(|participant| Self::with(participant, script))
    }

    fn with(participant: Participant, script: Script) -> Self {
        Self {
            participant,
            script,
            voice: Voice::new(pitch_of(script.speaker), script.loudness),
            frames: 0,
            played: Vec::new(),
            frame: vec![0.0; FRAME_SAMPLES],
        }
    }

    /// The node and the call behind the bot: creating, joining, tickets, status.
    #[must_use]
    pub const fn participant(&self) -> &Participant {
        &self.participant
    }

    pub const fn participant_mut(&mut self) -> &mut Participant {
        &mut self.participant
    }

    #[must_use]
    pub const fn script(&self) -> Script {
        self.script
    }

    /// One 20 ms step: say the next frame of its script, and play what it is given.
    ///
    /// # Errors
    ///
    /// When the encoder or a decoder fails.
    pub fn tick(&mut self) -> Result<(), CodecError> {
        let elapsed = FRAME.saturating_mul(u32::try_from(self.frames).unwrap_or(u32::MAX));
        self.frames = self.frames.saturating_add(1);
        let talking = self.script.rhythm.talking(elapsed);
        self.voice.frame(talking, &mut self.frame);
        let call = self.participant.call();
        call.speak(&self.frame)?;
        call.listen(&mut self.frame)?;
        self.played.extend_from_slice(&self.frame);
        let extra = self.played.len().saturating_sub(Self::EARS);
        self.played.drain(..extra);
        Ok(())
    }

    /// What this bot played over the last second; empty until it has played that much.
    fn last_second(&self) -> &[f32] {
        let second = SAMPLE_RATE as usize;
        self.played
            .len()
            .checked_sub(second)
            .and_then(|from| self.played.get(from..))
            .unwrap_or_default()
    }

    /// Whether `speaker`'s voice is in what this bot played over the last second.
    #[must_use]
    pub fn hears(&self, speaker: u32) -> bool {
        hears(self.last_second(), pitch_of(speaker))
    }

    /// How strongly `speaker`'s pitch is in what this bot played over the last second.
    #[must_use]
    pub fn strength_of(&self, speaker: u32) -> f32 {
        strength(self.last_second(), pitch_of(speaker))
    }

    /// Every speaker among `speakers` this bot hears, in order.
    pub fn hears_among(&self, speakers: impl IntoIterator<Item = u32>) -> Vec<u32> {
        speakers.into_iter().filter(|s| self.hears(*s)).collect()
    }

    pub async fn stop(self) {
        self.participant.stop().await;
    }
}
