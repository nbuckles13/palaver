//! The engine as a library.
//!
//! - [`engine::Engine`] runs one node (its khef and its control logic) over a transport.
//! - [`call::Call`] puts a voice on it: samples in, samples out.
//! - [`video::VideoSender`] and [`video::VideoReceiver`] put pictures on it, through the
//!   engine's [`engine::VideoPort`].
//! - [`pictures::Pictures`] runs both for one node: its camera out, what it watches in.
//! - [`participant::Participant`] is a node on a network with a key of its own, started
//!   together with its call: what an application or a bot begins with. It makes
//!   invitations and joins by them, through an [`engine::Controller`] that can be kept
//!   apart from the call.
//!
//! Media devices and events for an application are to come.

pub mod call;
pub mod engine;
pub mod participant;
pub mod pictures;
pub mod video;

pub use call::{Call, VoiceGauge};
pub use engine::{
    AudioIn, Controller, Engine, EngineConfig, LinkGauge, Status, VideoIn, VideoPort,
};
pub use palaver_node::VideoTrack;
pub use palaver_proto::Shape;
pub use participant::{Offer, Options, Participant, StartError, entry_points};
pub use pictures::Pictures;
pub use video::{Piece, VideoReceiver, VideoSender};
