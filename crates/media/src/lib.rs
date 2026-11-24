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
//! [`pcm`] stands between that path and a sound device, which has its own rate, channels
//! and block sizes; [`resample`] changes rates. Neither touches a device.
//!
//! [`synthetic`] makes voices for bots and tests, and can tell whose voice is in a mix.
//!
//! [`picture`] is the beginning of the same for video: one frame as it passes between a
//! camera, a codec and a screen, and moving pictures for bots and tests. [`video`] turns
//! pictures into H.264 and back.

pub mod jitter;
pub mod level;
pub mod mix;
pub mod pcm;
pub mod picture;
pub mod process;
pub mod resample;
pub mod synthetic;
pub mod video;
pub mod voice;

pub use jitter::JitterBuffer;
pub use palaver_opus::{Channels, EncoderConfig, Error as CodecError, FRAME_SAMPLES, SAMPLE_RATE};
