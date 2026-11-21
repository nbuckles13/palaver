//! The engine as a library.
//!
//! - [`engine::Engine`] runs one node (its khef and its control logic) over a transport.
//! - [`call::Call`] puts a voice on it: samples in, samples out.
//! - [`participant::Participant`] is the two together on a network, with a key of its own:
//!   what an application or a bot starts with. It makes invitations and joins by them.
//!
//! Media devices and events for an application are to come.

pub mod call;
pub mod engine;
pub mod participant;

pub use call::Call;
pub use engine::{AudioIn, Engine, EngineConfig, Status};
pub use participant::{Options, Participant, StartError, entry_points};
