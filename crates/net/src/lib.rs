//! What a node needs from the outside world: a key of its own, and connections to others.
//!
//! - [`identity`]: the node's per-meeting Ed25519 key, the participant id that follows from
//!   it, and the meeting tag that follows from the meeting secret. The sans-IO crates take
//!   signing and verifying through traits and do no cryptography themselves; this is where
//!   those traits are filled in.
//! - [`transport`]: the boundary every transport is written to. A connection offers
//!   one-way streams, two-way streams and datagrams, which is what MoQ asks of QUIC.
//! - [`memory`]: a transport that never leaves the process, for tests and for bots that
//!   share one.
//!
//! - [`admission`]: the handshake that lets only holders of the meeting secret connect.
//!
//! The QUIC transport (iroh) is to come.

pub mod admission;
pub mod identity;
pub mod memory;
pub mod transport;
