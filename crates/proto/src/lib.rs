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
pub use names::{MeetingTag, PLAN_ALIAS, ParticipantId, Source, Track, TrackName};
pub use plan::{Edge, Member, Plan, PlanStamp};
pub use report::{
    Link, MAX_ADDRESSES, MAX_NAME_BYTES, MAX_RELAY_URL_BYTES, PathKind, Report, advertisable,
};
pub use sensitive::{Redacted, Secret};
pub use sign::{PublicKey, SIGNATURE_BYTES, Signature, Signer, Verifier};
pub use ticket::{EntryPoint, MAX_ENTRY_POINTS, SECRET_BYTES, Ticket, TicketError};
pub use time::Timestamp;
