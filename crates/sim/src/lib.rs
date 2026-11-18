//! Deterministic multi-node simulator over the sans-IO cores.
//!
//! A discrete-event loop: a virtual clock, seeded randomness, and an event queue with a total
//! order, so one seed always produces the same run. See `docs/spikes/05-simulator.md`.

mod rng;
mod world;

pub use rng::Rng;
pub use world::{Micros, NodeId, World};
