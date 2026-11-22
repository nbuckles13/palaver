//! A participant with a sound card.
//!
//! [`voice::Voice`] joins a call to a microphone and a speaker, with the speaker keeping
//! time; [`live::Live`] runs one on a thread of its own against the system's devices.
//! The program in `main.rs` is a call from the command line; the window comes next.

pub mod check;
pub mod history;
pub mod live;
pub mod png;
pub mod session;
pub mod ui;
pub mod voice;
pub mod words;
