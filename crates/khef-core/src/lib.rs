//! The khef state machine: sessions, subscriptions, routing, top-N, priorities, dedupe.
//!
//! Everything here is on or next to the forwarding path, so beyond the workspace lints this
//! crate refuses arithmetic that can overflow and indexing that can panic.
#![cfg_attr(
    not(test),
    deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)
)]

pub mod dedupe;
pub mod flood;
pub mod session;
pub mod top_n;

pub use dedupe::Dedupe;
pub use flood::{Cached, Flood, Offer};
pub use session::{Action, CloseCode, Event, Role, Session, StreamId, StreamKind};
pub use top_n::{TopN, TopNConfig};
