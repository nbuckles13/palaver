//! Plan computation: peering graph, routing tables, audio overlay.

mod wiring;

pub use wiring::{Input, MAX_AUDIO_DEGREE, MIN_PEERINGS, ReportedLink, Wiring, wire};
