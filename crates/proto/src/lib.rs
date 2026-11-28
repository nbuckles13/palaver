//! Shared types: ids, names, reports, plan, and the secret and redaction wrappers.

mod fields;
mod names;
mod plan;
mod report;
mod sensitive;
pub mod sign;
mod ticket;
mod time;

pub use fields::{DecodeError, MAX_ENTRIES};
pub use names::{
    MediaTrack, MeetingTag, PLAN_ALIAS, ParticipantId, Shape, Source, Track, TrackName,
};
pub use plan::{AssignedTrack, Edge, Member, Plan, PlanStamp, WIDE_ALIAS_START};
pub use report::{Link, MAX_ADDRESSES, MAX_NAME_BYTES, MAX_TRACKS, Report, advertisable};
pub use sensitive::{Redacted, Secret};
pub use sign::{PublicKey, SIGNATURE_BYTES, Signature, Signer, Verifier};
pub use ticket::{EntryPoint, MAX_ENTRY_POINTS, SECRET_BYTES, Ticket, TicketError};
pub use time::Timestamp;
