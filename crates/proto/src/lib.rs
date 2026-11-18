//! Shared types: ids, names, reports, plan, and the secret and redaction wrappers.

mod names;
mod sensitive;

pub use names::{MeetingTag, ParticipantId, Source, Track, TrackName};
pub use sensitive::{Redacted, Secret};
