//! The engine as a library.
//!
//! So far: [`engine::Engine`], which runs one node (its khef and its control logic) over
//! a transport, and [`call::Call`], which puts a voice on it: samples in, samples out.
//! Joining by ticket, media devices and events for an application are to come.

pub mod call;
pub mod engine;

pub use call::Call;
pub use engine::{AudioIn, Engine, EngineConfig, Status};
