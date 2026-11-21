//! The boundary every transport is written to.
//!
//! An [`Endpoint`] is a node's presence on a network: it dials others by key and accepts
//! those who dial it. A [`Connection`] offers what MoQ asks of QUIC: one-way streams,
//! two-way streams, and datagrams. The shapes follow QUIC's so that the QUIC transport is a
//! thin layer; the in-memory transport ([`crate::memory`]) keeps the same promises without
//! a socket.
//!
//! What every transport promises:
//!
//! - **A connection is to a key.** [`Connection::peer`] is the key the other end proved it
//!   holds. Addresses are only hints for finding it.
//! - **A stream delivers its bytes in order, then says how it ended**: finished, or reset
//!   with a code. Bytes sent before a reset may be lost.
//! - **A datagram arrives whole or not at all**, in no particular order, and is dropped
//!   rather than queued when the path cannot take it.
//! - **Closing is final and seen by both ends**: every operation still waiting on the
//!   connection or on one of its streams returns [`Closed`]. What had arrived and was not
//!   yet read is gone with it.
//! - **Writing waits when the reader is slow.** Nothing is buffered without limit.

use bytes::Bytes;
use palaver_proto::{PublicKey, Redacted};
use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

/// Who to connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub key: PublicKey,
    /// Where it may be found. Hints only; all are tried.
    pub addresses: Vec<Redacted<SocketAddr>>,
}

impl Peer {
    /// A peer known by key alone.
    pub fn by_key(key: PublicKey) -> Self {
        Self {
            key,
            addresses: Vec::new(),
        }
    }
}

/// Which end of a connection this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// This end dialled.
    Dialled,
    /// This end accepted.
    Accepted,
}

/// How a connection reaches its peer at the moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Path {
    /// The round-trip time the transport measures.
    pub rtt: Duration,
    /// The address the peer's packets come from, where the transport has addresses.
    pub remote: Option<SocketAddr>,
}

/// Why a dial did not give a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialError {
    /// Nothing answered for that key at any address tried.
    Unreachable,
    /// Something answered and turned the connection away.
    Refused,
    /// This endpoint has been closed.
    EndpointClosed,
}

/// Who ended a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedBy {
    Us,
    Peer,
    /// Neither end closed it: the transport gave up on the path.
    Transport,
}

/// A connection has ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    pub by: ClosedBy,
    /// The code given by whoever closed it; zero if the transport did.
    pub code: u64,
    pub reason: String,
}

/// Why an operation on a stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// The sender abandoned the stream.
    Reset(u64),
    /// The receiver asked the sender to stop.
    Stopped(u64),
    /// This end already finished or reset the stream.
    Finished,
    Closed(Closed),
}

/// Why a datagram was not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DatagramError {
    /// Larger than [`Connection::max_datagram_size`].
    TooLarge,
    Closed(Closed),
}

/// A node's presence on a network.
pub trait Endpoint: Send + Sync + 'static {
    type Connection: Connection;

    /// The key this endpoint proves it holds to everyone it connects to.
    fn key(&self) -> PublicKey;

    /// Whether a dial needs an address: without one it cannot succeed, so the caller does
    /// better to wait until it has learned one.
    fn dials_by_address(&self) -> bool;

    /// Connect to `peer`.
    fn dial(&self, peer: &Peer)
    -> impl Future<Output = Result<Self::Connection, DialError>> + Send;

    /// The next connection someone else dialled. `None` once the endpoint is closed.
    fn accept(&self) -> impl Future<Output = Option<Self::Connection>> + Send;

    /// Close every connection and stop accepting.
    fn close(&self);
}

/// A shared endpoint is an endpoint: whoever starts a node on it can keep a handle, to ask
/// the transport what the node cannot (its public address, say).
impl<E: Endpoint> Endpoint for std::sync::Arc<E> {
    type Connection = E::Connection;

    fn key(&self) -> PublicKey {
        (**self).key()
    }

    fn dials_by_address(&self) -> bool {
        (**self).dials_by_address()
    }

    fn dial(
        &self,
        peer: &Peer,
    ) -> impl Future<Output = Result<Self::Connection, DialError>> + Send {
        (**self).dial(peer)
    }

    fn accept(&self) -> impl Future<Output = Option<Self::Connection>> + Send {
        (**self).accept()
    }

    fn close(&self) {
        (**self).close();
    }
}

/// One connection. Cloning gives another handle to the same connection.
pub trait Connection: Clone + Send + Sync + 'static {
    type Send: SendStream;
    type Recv: RecvStream;

    fn peer(&self) -> PublicKey;
    fn side(&self) -> Side;
    fn path(&self) -> Path;

    /// Open a stream only this end sends on.
    fn open_uni(&self) -> impl Future<Output = Result<Self::Send, Closed>> + Send;

    /// Open a stream both ends send on. The peer learns of it when the first bytes arrive.
    fn open_bidi(&self) -> impl Future<Output = Result<(Self::Send, Self::Recv), Closed>> + Send;

    /// The next stream the peer opened to send on.
    fn accept_uni(&self) -> impl Future<Output = Result<Self::Recv, Closed>> + Send;

    /// The next two-way stream the peer opened.
    fn accept_bidi(&self) -> impl Future<Output = Result<(Self::Send, Self::Recv), Closed>> + Send;

    /// Send a datagram. `Ok` means it was handed to the path, not that it will arrive.
    fn send_datagram(&self, datagram: Bytes) -> Result<(), DatagramError>;

    /// The next datagram to arrive.
    fn read_datagram(&self) -> impl Future<Output = Result<Bytes, Closed>> + Send;

    /// The largest datagram the path takes now.
    fn max_datagram_size(&self) -> usize;

    /// Close the connection. Does nothing if it is already closed.
    fn close(&self, code: u64, reason: &str);

    /// Resolves when the connection has ended, however it ended.
    fn closed(&self) -> impl Future<Output = Closed> + Send;
}

/// The sending half of a stream. Dropping it without finishing resets the stream.
pub trait SendStream: Send + 'static {
    /// Send `bytes`, waiting while the reader is behind.
    fn write(&mut self, bytes: Bytes) -> impl Future<Output = Result<(), StreamError>> + Send;

    /// End the stream cleanly: the reader gets everything written, then the end.
    fn finish(&mut self);

    /// Abandon the stream: the reader gets `code`, and may not get what was written.
    fn reset(&mut self, code: u64);
}

/// The receiving half of a stream.
pub trait RecvStream: Send + 'static {
    /// The next bytes, or `None` at the clean end of the stream.
    fn read(&mut self) -> impl Future<Output = Result<Option<Bytes>, StreamError>> + Send;

    /// Ask the sender to stop. Nothing more is read.
    fn stop(&mut self, code: u64);
}
