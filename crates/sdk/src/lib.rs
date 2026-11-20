//! The engine as a library.
//!
//! So far: [`engine::Engine`], which runs one node (its khef and its control logic) over
//! a transport. Joining by ticket, media devices and events for an application are to come.

pub mod engine;

pub use engine::{AudioIn, Engine, EngineConfig, Status};
