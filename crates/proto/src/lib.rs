//! Shared types: ids, names, reports, plan, and the secret and redaction wrappers.

/// Time in microseconds on the caller's clock. Sans-IO code never reads a clock; it is told
/// the time. Only differences matter.
pub type Micros = u64;

mod fields;
mod names;
mod plan;
mod report;
mod sensitive;
pub mod sign;

pub use fields::{DecodeError, MAX_ENTRIES};
pub use names::{MeetingTag, ParticipantId, Source, Track, TrackName};
pub use plan::{Edge, Member, Plan, PlanStamp};
pub use report::{
    Link, MAX_ADDRESSES, MAX_NAME_BYTES, MAX_RELAY_URL_BYTES, PathKind, Report, advertisable,
};
pub use sensitive::{Redacted, Secret};
pub use sign::{PublicKey, SIGNATURE_BYTES, Signature, Signer, Verifier};
