//! One voice, from samples to packets and back.
//!
//! [`VoiceSender`] turns 20 ms of captured samples into at most one packet: its level, and
//! the frame as Opus. [`VoiceReceiver`] takes the packets of one remote voice as they
//! arrive, and every 20 ms gives the samples to play: decoded, rebuilt from the next
//! packet when one was lost, or guessed by the decoder when there is nothing to go on.
//!
//! A voice is sent in one channel or two ([`EncoderConfig::channels`]) and a receiver
//! gives whichever it is asked for, whatever was sent. Samples of two channels are
//! interleaved.
//!
//! The frame counter advances through silence, whether or not a packet is sent, so a gap
//! in the numbers is a gap in time (design section 8).

use crate::jitter::{self, Inserted, JitterBuffer, Packet, Playout};
use crate::level::audio_level;
use palaver_opus::{
    Channels, DTX_PACKET_BYTES, Decoder, Encoder, EncoderConfig, Error, MAX_PACKET_BYTES,
};
use palaver_proto::Timestamp;

/// One encoded frame, ready to send.
#[derive(Debug, PartialEq, Eq)]
pub struct Encoded<'a> {
    /// The frame counter: the group id of the object.
    pub seq: u64,
    /// The `audio_level` property.
    pub level: u8,
    pub payload: &'a [u8],
}

/// Encodes one voice.
pub struct VoiceSender {
    encoder: Encoder,
    next_seq: u64,
    packet: Vec<u8>,
}

impl VoiceSender {
    /// A sender that encodes as `config` says.
    ///
    /// # Errors
    ///
    /// As for [`Encoder::new`]: libopus could not make the encoder.
    pub fn new(config: EncoderConfig) -> Result<Self, Error> {
        Ok(Self {
            encoder: Encoder::new(config)?,
            next_seq: 0,
            packet: vec![0; MAX_PACKET_BYTES],
        })
    }

    /// The counter the next frame will carry.
    #[must_use]
    pub const fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Encode the next 20 ms: a frame of the channels the sender was made for. `None` if the encoder found nothing worth sending (silence,
    /// with discontinuous transmission on); the counter advances all the same.
    ///
    /// # Errors
    ///
    /// [`Error::FrameLength`] if `pcm` is not a frame of the sender's channels, and
    /// [`Error::Opus`] if libopus refuses to encode it. The counter does not advance.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Option<Encoded<'_>>, Error> {
        let level = audio_level(pcm);
        let length = self.encoder.encode(pcm, &mut self.packet)?;
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        if length <= DTX_PACKET_BYTES {
            return Ok(None);
        }
        Ok(self.packet.get(..length).map(|payload| Encoded {
            seq,
            level,
            payload,
        }))
    }
}

/// How one frame of output was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Played {
    /// Nothing to play: silence.
    Silent,
    /// The frame itself, decoded.
    Decoded,
    /// A lost frame, rebuilt from data in a later packet.
    Recovered,
    /// The decoder's guess.
    Concealed,
}

/// Receives one voice. `P` is whatever holds a packet's bytes.
pub struct VoiceReceiver<P> {
    jitter: JitterBuffer<P>,
    decoder: Decoder,
}

impl<P: AsRef<[u8]> + Clone> VoiceReceiver<P> {
    /// A receiver that gives `channels`: a voice sent in one is given on both sides, a
    /// voice sent in two is averaged into one.
    ///
    /// # Errors
    ///
    /// As for [`Decoder::new`]: libopus could not make the decoder.
    pub fn new(config: jitter::Config, channels: Channels) -> Result<Self, Error> {
        Ok(Self {
            jitter: JitterBuffer::new(config),
            decoder: Decoder::new(channels)?,
        })
    }

    /// A packet has arrived.
    pub fn insert(&mut self, seq: u64, level: Option<u8>, payload: P, now: Timestamp) -> Inserted {
        self.jitter.insert(
            Packet {
                seq,
                payload,
                level,
            },
            now,
        )
    }

    /// The next 20 ms to play, written to `pcm` (which must hold a frame of the receiver's
    /// channels: [`Channels::frame_len`]). Call once per frame interval.
    ///
    /// # Errors
    ///
    /// [`Error::FrameLength`] if `pcm` is not that long, before anything is taken from
    /// the buffer. [`Error::Opus`] if the decoder refuses the packet it was given.
    pub fn pull(&mut self, now: Timestamp, pcm: &mut [f32]) -> Result<Played, Error> {
        if pcm.len() != self.decoder.channels().frame_len() {
            return Err(Error::FrameLength);
        }
        match self.jitter.pull(now) {
            Playout::Waiting | Playout::Silence { .. } => {
                pcm.fill(0.0);
                Ok(Played::Silent)
            }
            Playout::Frame { payload, .. } => {
                self.decoder.decode(payload.as_ref(), pcm)?;
                Ok(Played::Decoded)
            }
            Playout::Fec { next, .. } => {
                self.decoder.decode_fec(next.as_ref(), pcm)?;
                Ok(Played::Recovered)
            }
            Playout::Dred {
                later, frames_back, ..
            } => {
                if self.decoder.decode_dred(later.as_ref(), frames_back, pcm)? {
                    Ok(Played::Recovered)
                } else {
                    // The packet's history did not reach that far back.
                    self.decoder.conceal(pcm)?;
                    Ok(Played::Concealed)
                }
            }
            Playout::Conceal { .. } | Playout::Hold => {
                self.decoder.conceal(pcm)?;
                Ok(Played::Concealed)
            }
        }
    }

    #[must_use]
    pub const fn stats(&self) -> jitter::Stats {
        self.jitter.stats()
    }

    /// The playout delay the buffer is aiming for.
    #[must_use]
    pub const fn target_delay(&self) -> std::time::Duration {
        self.jitter.target_delay()
    }
}
