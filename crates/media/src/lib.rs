//! Capture, audio processing, codecs, jitter buffer, render; synthetic sources.
//!
//! What is here so far is the audio path between a microphone's samples and a speaker's,
//! without the devices at either end:
//!
//! ```text
//! 20 ms of samples → level + Opus (voice::VoiceSender) → … khefs … →
//!     jitter buffer + Opus (voice::VoiceReceiver) → mix → 20 ms of samples
//! ```
//!
//! [`synthetic`] makes voices for bots and tests, and can tell whose voice is in a mix.

pub mod jitter;
pub mod level;
pub mod mix;
pub mod synthetic;
pub mod voice;

pub use jitter::JitterBuffer;
pub use palaver_opus::{FRAME_SAMPLES, SAMPLE_RATE};
