//! A voice on a running node: samples in, samples out.
//!
//! A [`Call`] sits between whatever makes and plays sound (a sound card, a bot's made-up
//! voice) and the [`Engine`]. Every 20 ms its owner gives it the frame just captured and
//! asks for the frame to play:
//!
//! - [`speak`](Call::speak) measures the frame's level, encodes it, and hands it to the
//!   engine, which sends it wherever the plan says.
//! - [`listen`](Call::listen) takes the packets the engine has for this node's client
//!   (the loudest few voices, already chosen by the khefs), runs each voice through its
//!   own jitter buffer and decoder, and mixes them.
//!
//! What is sent has the channels of the encoder's settings; what is played has the
//! channels the call was made with, whatever each voice was sent in.
//!
//! The owner keeps the 20 ms beat. Nothing here waits or spawns.

use crate::engine::Engine;
use bytes::Bytes;
use palaver_media::jitter::Config as JitterConfig;
use palaver_media::voice::{Played, VoiceReceiver, VoiceSender};
use palaver_media::{Channels, CodecError, EncoderConfig, mix};
use palaver_proto::Timestamp;
use std::collections::BTreeMap;
use tokio::time::Instant;

/// A voice that has given nothing but silence for this many frames (five seconds) is
/// forgotten; it starts afresh if it speaks again.
const FORGET_AFTER_FRAMES: u32 = 250;

struct Voice {
    receiver: VoiceReceiver<Bytes>,
    /// Frames in a row for which it had nothing to play.
    silent_for: u32,
}

/// One participant's audio, both ways.
pub struct Call {
    engine: Engine,
    sender: VoiceSender,
    jitter: JitterConfig,
    /// What [`listen`](Call::listen) gives.
    playout: Channels,
    /// Voices being played, by alias.
    voices: BTreeMap<u64, Voice>,
    started: Instant,
    frame: Vec<f32>,
}

impl Call {
    pub fn new(
        engine: Engine,
        encoder: EncoderConfig,
        jitter: JitterConfig,
        playout: Channels,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            engine,
            sender: VoiceSender::new(encoder)?,
            jitter,
            playout,
            voices: BTreeMap::new(),
            started: Instant::now(),
            frame: vec![0.0; playout.frame_len()],
        })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Stop the node.
    pub async fn stop(self) {
        self.engine.stop().await;
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_micros(
            u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX),
        )
    }

    /// Send the 20 ms just captured, in the channels of the encoder's settings. `Ok(false)` if nothing was sent: the encoder found
    /// silence, or the engine is behind.
    pub fn speak(&mut self, pcm: &[f32]) -> Result<bool, CodecError> {
        let Some(packet) = self.sender.encode(pcm)? else {
            return Ok(false);
        };
        let payload = Bytes::copy_from_slice(packet.payload);
        Ok(self.engine.send_audio(packet.seq, packet.level, payload))
    }

    /// Write the next 20 ms to play into `out`, which must hold a frame of the channels
    /// the call plays in ([`Channels::frame_len`]). Returns how many voices are in it.
    pub fn listen(&mut self, out: &mut [f32]) -> Result<usize, CodecError> {
        if out.len() != self.playout.frame_len() {
            return Err(CodecError::FrameLength);
        }
        let now = self.now();
        while let Some(audio) = self.engine.try_next_audio() {
            let voice = match self.voices.get_mut(&audio.alias) {
                Some(voice) => voice,
                None => {
                    let receiver = VoiceReceiver::new(self.jitter, self.playout)?;
                    self.voices.entry(audio.alias).or_insert(Voice {
                        receiver,
                        silent_for: 0,
                    })
                }
            };
            voice
                .receiver
                .insert(audio.group, Some(audio.level), audio.payload, now);
        }

        out.fill(0.0);
        let mut heard = 0usize;
        for voice in self.voices.values_mut() {
            match voice.receiver.pull(now, &mut self.frame)? {
                Played::Silent => voice.silent_for = voice.silent_for.saturating_add(1),
                Played::Decoded | Played::Recovered | Played::Concealed => {
                    voice.silent_for = 0;
                    mix::add(out, &self.frame);
                    heard = heard.saturating_add(1);
                }
            }
        }
        self.voices
            .retain(|_, voice| voice.silent_for < FORGET_AFTER_FRAMES);
        mix::limit(out);
        Ok(heard)
    }
}
