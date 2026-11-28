//! A transport that never leaves the process.
//!
//! A [`MemoryNetwork`] is a set of endpoints that can dial each other by key. It keeps the
//! promises in [`crate::transport`] with channels in place of sockets: streams deliver in
//! order and say how they ended, a slow reader makes the writer wait, datagrams are dropped
//! when their queue is full, and a close is seen by both ends.
//!
//! An endpoint can be put behind a NAT ([`MemoryNetwork::put_behind_nat`]): it is then
//! reached only by those it has itself dialled, which is what a home router does and
//! what two nodes dialling each other at once get past.
//!
//! What it does not do: delay, loss, or a path that goes quiet without closing. Those are
//! the simulator's business; this is for running real nodes against each other in one
//! process, in tests and for bots that share one.

use crate::transport::{
    Closed, ClosedBy, Connection, DatagramError, DialError, Endpoint, Path, Peer, RecvStream,
    SendStream, Side, StreamError,
};
use bytes::Bytes;
use palaver_proto::PublicKey;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, mpsc, watch};
use tokio::time::timeout;

/// How long a dial to an endpoint behind a NAT waits for that endpoint to dial back.
pub const NAT_PATIENCE: Duration = Duration::from_secs(10);
/// Connections dialled to an endpoint and not yet accepted by it.
const ACCEPT_QUEUE: usize = 16;
/// Streams opened by one end and not yet accepted by the other.
const OPEN_QUEUE: usize = 64;
/// Writes a stream holds for a reader that is behind. The next write waits.
pub const STREAM_QUEUE: usize = 16;
/// Datagrams held for a reader that is behind. The next is dropped.
pub const DATAGRAM_QUEUE: usize = 256;
/// The largest datagram a connection takes; what a QUIC path typically allows.
pub const MAX_DATAGRAM: usize = 1200;

/// How a connection ended: which end closed it, the code and the reason.
type End = Option<(Side, u64, String)>;

/// A lock that a panic elsewhere cannot leave unusable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn closed_as(side: Side, end: &(Side, u64, String)) -> Closed {
    Closed {
        by: if end.0 == side {
            ClosedBy::Us
        } else {
            ClosedBy::Peer
        },
        code: end.1,
        reason: end.2.clone(),
    }
}

/// What to report if the record of how a connection ended has itself gone.
fn vanished() -> Closed {
    Closed {
        by: ClosedBy::Transport,
        code: 0,
        reason: "connection dropped".into(),
    }
}

async fn wait_closed(end: &mut watch::Receiver<End>, side: Side) -> Closed {
    match end.wait_for(Option::is_some).await {
        Ok(end) => end.as_ref().map_or_else(vanished, |e| closed_as(side, e)),
        Err(_) => vanished(),
    }
}

fn closed_now(end: &watch::Receiver<End>, side: Side) -> Option<Closed> {
    end.borrow().as_ref().map(|e| closed_as(side, e))
}

/// What an endpoint shares with those who dial it.
struct Listener {
    incoming: mpsc::Sender<MemoryConnection>,
    connections: Mutex<Vec<MemoryConnection>>,
    closed: watch::Sender<bool>,
}

impl Listener {
    fn keep(&self, connection: MemoryConnection) {
        let mut connections = lock(&self.connections);
        connections.retain(|c| c.0.end.borrow().is_none());
        connections.push(connection);
    }
}

/// Endpoints that can reach each other.
#[derive(Clone, Default)]
pub struct MemoryNetwork {
    listeners: Arc<Mutex<BTreeMap<PublicKey, Arc<Listener>>>>,
    nat: Arc<Nat>,
}

/// Who is behind a NAT, and who has dialled whom: (from, to), kept for good.
#[derive(Default)]
struct Nat {
    behind: Mutex<BTreeSet<PublicKey>>,
    dialled: watch::Sender<BTreeSet<(PublicKey, PublicKey)>>,
}

impl MemoryNetwork {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Put the endpoint known by `key` behind a NAT, now or when it comes: a dial reaches
    /// it only once it has dialled the dialler, and waits [`NAT_PATIENCE`] for that.
    pub fn put_behind_nat(&self, key: PublicKey) {
        lock(&self.nat.behind).insert(key);
    }

    /// An endpoint on this network known by `key`. `None` if an open endpoint already
    /// has that key.
    #[must_use]
    pub fn endpoint(&self, key: PublicKey) -> Option<MemoryEndpoint> {
        let (incoming, accepted) = mpsc::channel(ACCEPT_QUEUE);
        let listener = Arc::new(Listener {
            incoming,
            connections: Mutex::new(Vec::new()),
            closed: watch::Sender::new(false),
        });
        let mut listeners = lock(&self.listeners);
        if listeners.contains_key(&key) {
            return None;
        }
        listeners.insert(key, Arc::clone(&listener));
        drop(listeners);
        Some(MemoryEndpoint {
            key,
            network: self.clone(),
            listener,
            accepted: AsyncMutex::new(accepted),
        })
    }
}

/// One node's presence on a [`MemoryNetwork`]. Dropping it closes it.
pub struct MemoryEndpoint {
    key: PublicKey,
    network: MemoryNetwork,
    listener: Arc<Listener>,
    accepted: AsyncMutex<mpsc::Receiver<MemoryConnection>>,
}

impl Drop for MemoryEndpoint {
    fn drop(&mut self) {
        self.close();
    }
}

impl Endpoint for MemoryEndpoint {
    type Connection = MemoryConnection;

    fn key(&self) -> PublicKey {
        self.key
    }

    fn dials_by_address(&self) -> bool {
        false
    }

    async fn dial(&self, peer: &Peer) -> Result<MemoryConnection, DialError> {
        if *self.listener.closed.borrow() {
            return Err(DialError::EndpointClosed);
        }
        if peer.key == self.key {
            return Err(DialError::Unreachable);
        }
        // The dial opens the way back through this end's NAT, if it has one.
        let nat = &self.network.nat;
        nat.dialled
            .send_if_modified(|dialled| dialled.insert((self.key, peer.key)));
        if lock(&nat.behind).contains(&peer.key) {
            let mut dialled = nat.dialled.subscribe();
            let opened = dialled.wait_for(|dialled| dialled.contains(&(peer.key, self.key)));
            if !matches!(timeout(NAT_PATIENCE, opened).await, Ok(Ok(_))) {
                return Err(DialError::Unreachable);
            }
        }
        let target = lock(&self.network.listeners).get(&peer.key).cloned();
        let Some(target) = target else {
            return Err(DialError::Unreachable);
        };
        let (ours, theirs) = pair(self.key, peer.key);
        // Both endpoints know of the connection before either can use it, so closing
        // either endpoint closes it, accepted or not.
        target.keep(theirs.clone());
        self.listener.keep(ours.clone());
        if target.incoming.send(theirs).await.is_err() || *target.closed.borrow() {
            ours.close(0, "endpoint closed");
            return Err(DialError::Unreachable);
        }
        Ok(ours)
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the queue stays locked for the whole wait, so that one caller at a time takes from it"
    )]
    async fn accept(&self) -> Option<MemoryConnection> {
        let mut closed = self.listener.closed.subscribe();
        let mut accepted = self.accepted.lock().await;
        tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => None,
            connection = accepted.recv() => connection,
        }
    }

    fn close(&self) {
        if self.listener.closed.send_replace(true) {
            return;
        }
        lock(&self.network.listeners).remove(&self.key);
        let connections = std::mem::take(&mut *lock(&self.listener.connections));
        for connection in connections {
            connection.close(0, "endpoint closed");
        }
    }
}

struct Half {
    peer: PublicKey,
    side: Side,
    /// Shared by both halves.
    end: Arc<watch::Sender<End>>,
    uni_out: mpsc::Sender<MemoryRecv>,
    uni_in: AsyncMutex<mpsc::Receiver<MemoryRecv>>,
    bidi_out: mpsc::Sender<(MemorySend, MemoryRecv)>,
    bidi_in: AsyncMutex<mpsc::Receiver<(MemorySend, MemoryRecv)>>,
    datagrams_out: mpsc::Sender<Bytes>,
    datagrams_in: AsyncMutex<mpsc::Receiver<Bytes>>,
}

/// One end of a connection on a [`MemoryNetwork`].
#[derive(Clone)]
pub struct MemoryConnection(Arc<Half>);

/// The two ends of a new connection: the dialler's, then the accepter's.
fn pair(dialler: PublicKey, accepter: PublicKey) -> (MemoryConnection, MemoryConnection) {
    let end = Arc::new(watch::Sender::new(None));
    let (uni_a, uni_b) = (mpsc::channel(OPEN_QUEUE), mpsc::channel(OPEN_QUEUE));
    let (bidi_a, bidi_b) = (mpsc::channel(OPEN_QUEUE), mpsc::channel(OPEN_QUEUE));
    let (dg_a, dg_b) = (mpsc::channel(DATAGRAM_QUEUE), mpsc::channel(DATAGRAM_QUEUE));
    let ours = Half {
        peer: accepter,
        side: Side::Dialled,
        end: Arc::clone(&end),
        uni_out: uni_a.0,
        uni_in: AsyncMutex::new(uni_b.1),
        bidi_out: bidi_a.0,
        bidi_in: AsyncMutex::new(bidi_b.1),
        datagrams_out: dg_a.0,
        datagrams_in: AsyncMutex::new(dg_b.1),
    };
    let theirs = Half {
        peer: dialler,
        side: Side::Accepted,
        end,
        uni_out: uni_b.0,
        uni_in: AsyncMutex::new(uni_a.1),
        bidi_out: bidi_b.0,
        bidi_in: AsyncMutex::new(bidi_a.1),
        datagrams_out: dg_b.0,
        datagrams_in: AsyncMutex::new(dg_a.1),
    };
    (
        MemoryConnection(Arc::new(ours)),
        MemoryConnection(Arc::new(theirs)),
    )
}

impl MemoryConnection {
    fn check(&self) -> Result<(), Closed> {
        match closed_now(&self.0.end.subscribe(), self.0.side) {
            Some(closed) => Err(closed),
            None => Ok(()),
        }
    }

    /// One direction of a stream: what this end sends on and what the peer reads from.
    fn pipe(&self) -> (MemorySend, MemoryRecv) {
        let (bytes_out, bytes_in) = mpsc::channel(STREAM_QUEUE);
        let how = watch::Sender::new(Ending::Open);
        let stop = watch::Sender::new(None);
        let peer_side = match self.0.side {
            Side::Dialled => Side::Accepted,
            Side::Accepted => Side::Dialled,
        };
        let recv = MemoryRecv {
            bytes: bytes_in,
            how: how.subscribe(),
            stop,
            connection: self.0.end.subscribe(),
            side: peer_side,
            ended: false,
            stopped: false,
        };
        let send = MemorySend {
            bytes: Some(bytes_out),
            how,
            stop: recv.stop.subscribe(),
            connection: self.0.end.subscribe(),
            side: self.0.side,
        };
        (send, recv)
    }

    /// The same, the other way: what the peer sends on and what this end reads from.
    fn pipe_back(&self) -> (MemorySend, MemoryRecv) {
        let (mut send, mut recv) = self.pipe();
        std::mem::swap(&mut send.side, &mut recv.side);
        (send, recv)
    }
}

impl Connection for MemoryConnection {
    type Send = MemorySend;
    type Recv = MemoryRecv;

    fn peer(&self) -> PublicKey {
        self.0.peer
    }

    fn side(&self) -> Side {
        self.0.side
    }

    fn path(&self) -> Path {
        Path {
            rtt: Duration::ZERO,
            sent: 0,
            lost: 0,
            bytes_sent: 0,
            bytes_received: 0,
            remote: None,
        }
    }

    async fn open_uni(&self) -> Result<MemorySend, Closed> {
        self.check()?;
        let (send, recv) = self.pipe();
        let mut end = self.0.end.subscribe();
        tokio::select! {
            biased;
            closed = wait_closed(&mut end, self.0.side) => Err(closed),
            outcome = self.0.uni_out.send(recv) => match outcome {
                Ok(()) => Ok(send),
                Err(_) => Err(self.closed().await),
            },
        }
    }

    async fn open_bidi(&self) -> Result<(MemorySend, MemoryRecv), Closed> {
        self.check()?;
        let (send, their_recv) = self.pipe();
        let (their_send, recv) = self.pipe_back();
        let mut end = self.0.end.subscribe();
        tokio::select! {
            biased;
            closed = wait_closed(&mut end, self.0.side) => Err(closed),
            outcome = self.0.bidi_out.send((their_send, their_recv)) => match outcome {
                Ok(()) => Ok((send, recv)),
                Err(_) => Err(self.closed().await),
            },
        }
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the queue stays locked for the whole wait, so that one caller at a time takes from it"
    )]
    async fn accept_uni(&self) -> Result<MemoryRecv, Closed> {
        let mut end = self.0.end.subscribe();
        let mut incoming = self.0.uni_in.lock().await;
        tokio::select! {
            biased;
            closed = wait_closed(&mut end, self.0.side) => Err(closed),
            stream = incoming.recv() => match stream {
                Some(stream) => Ok(stream),
                None => Err(self.closed().await),
            },
        }
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the queue stays locked for the whole wait, so that one caller at a time takes from it"
    )]
    async fn accept_bidi(&self) -> Result<(MemorySend, MemoryRecv), Closed> {
        let mut end = self.0.end.subscribe();
        let mut incoming = self.0.bidi_in.lock().await;
        tokio::select! {
            biased;
            closed = wait_closed(&mut end, self.0.side) => Err(closed),
            stream = incoming.recv() => match stream {
                Some(stream) => Ok(stream),
                None => Err(self.closed().await),
            },
        }
    }

    fn send_datagram(&self, datagram: Bytes) -> Result<(), DatagramError> {
        self.check().map_err(DatagramError::Closed)?;
        if datagram.len() > MAX_DATAGRAM {
            return Err(DatagramError::TooLarge);
        }
        // A full queue drops the datagram, as a congested path would.
        let _ = self.0.datagrams_out.try_send(datagram);
        Ok(())
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the queue stays locked for the whole wait, so that one caller at a time takes from it"
    )]
    async fn read_datagram(&self) -> Result<Bytes, Closed> {
        let mut end = self.0.end.subscribe();
        let mut incoming = self.0.datagrams_in.lock().await;
        tokio::select! {
            biased;
            closed = wait_closed(&mut end, self.0.side) => Err(closed),
            datagram = incoming.recv() => match datagram {
                Some(datagram) => Ok(datagram),
                None => Err(self.closed().await),
            },
        }
    }

    fn max_datagram_size(&self) -> usize {
        MAX_DATAGRAM
    }

    fn close(&self, code: u64, reason: &str) {
        self.0.end.send_if_modified(|end| {
            if end.is_some() {
                return false;
            }
            *end = Some((self.0.side, code, reason.to_owned()));
            true
        });
    }

    async fn closed(&self) -> Closed {
        wait_closed(&mut self.0.end.subscribe(), self.0.side).await
    }
}

impl Drop for Half {
    /// A connection nobody holds any more is closed, so the other end is not left waiting.
    fn drop(&mut self) {
        let side = self.side;
        self.end.send_if_modified(|end| {
            if end.is_some() {
                return false;
            }
            *end = Some((side, 0, "dropped".to_owned()));
            true
        });
    }
}

/// How the sender ended a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    Open,
    Finished,
    Reset(u64),
}

/// The sending half of a stream on a [`MemoryConnection`].
pub struct MemorySend {
    /// `None` once finished or reset.
    bytes: Option<mpsc::Sender<Bytes>>,
    how: watch::Sender<Ending>,
    stop: watch::Receiver<Option<u64>>,
    connection: watch::Receiver<End>,
    side: Side,
}

impl MemorySend {
    fn end(&mut self, how: Ending) {
        if self.bytes.take().is_some() {
            self.how.send_replace(how);
        }
    }
}

impl SendStream for MemorySend {
    async fn write(&mut self, bytes: Bytes) -> Result<(), StreamError> {
        let Some(out) = &self.bytes else {
            return Err(StreamError::Finished);
        };
        let side = self.side;
        tokio::select! {
            biased;
            closed = wait_closed(&mut self.connection, side) => Err(StreamError::Closed(closed)),
            stop = self.stop.wait_for(Option::is_some) => {
                // If the reader has gone without a word, that is a stop too.
                Err(StreamError::Stopped(stop.ok().and_then(|code| *code).unwrap_or(0)))
            }
            permit = out.reserve() => match permit {
                Ok(permit) => {
                    permit.send(bytes);
                    Ok(())
                }
                Err(_) => Err(StreamError::Stopped(0)),
            },
        }
    }

    fn finish(&mut self) {
        self.end(Ending::Finished);
    }

    fn reset(&mut self, code: u64) {
        self.end(Ending::Reset(code));
    }
}

impl Drop for MemorySend {
    fn drop(&mut self) {
        self.end(Ending::Reset(0));
    }
}

/// The receiving half of a stream on a [`MemoryConnection`].
pub struct MemoryRecv {
    bytes: mpsc::Receiver<Bytes>,
    how: watch::Receiver<Ending>,
    stop: watch::Sender<Option<u64>>,
    connection: watch::Receiver<End>,
    side: Side,
    ended: bool,
    stopped: bool,
}

impl MemoryRecv {
    /// How the sender has ended the stream, so far.
    fn how(&self) -> Ending {
        *self.how.borrow()
    }
}

impl RecvStream for MemoryRecv {
    async fn read(&mut self) -> Result<Option<Bytes>, StreamError> {
        if self.stopped {
            return Err(StreamError::Finished);
        }
        if self.ended {
            return Ok(None);
        }
        if let Some(closed) = closed_now(&self.connection, self.side) {
            return Err(StreamError::Closed(closed));
        }
        // A reset overtakes what was written before it.
        if let Ending::Reset(code) = self.how() {
            return Err(StreamError::Reset(code));
        }
        let side = self.side;
        tokio::select! {
            biased;
            closed = wait_closed(&mut self.connection, side) => Err(StreamError::Closed(closed)),
            chunk = self.bytes.recv() => match chunk {
                Some(chunk) => Ok(Some(chunk)),
                None => match self.how() {
                    Ending::Finished => {
                        self.ended = true;
                        Ok(None)
                    }
                    Ending::Reset(code) => Err(StreamError::Reset(code)),
                    Ending::Open => Err(StreamError::Reset(0)),
                },
            },
        }
    }

    fn stop(&mut self, code: u64) {
        if !self.stopped && !self.ended {
            self.stopped = true;
            self.stop.send_replace(Some(code));
            self.bytes.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    const SOON: Duration = Duration::from_millis(50);

    fn key(n: u8) -> PublicKey {
        PublicKey([n; 32])
    }

    /// Two endpoints and a connection between them: the dialler's end, then the accepter's.
    async fn connected() -> (
        MemoryEndpoint,
        MemoryEndpoint,
        MemoryConnection,
        MemoryConnection,
    ) {
        let network = MemoryNetwork::new();
        let a = network.endpoint(key(1)).unwrap();
        let b = network.endpoint(key(2)).unwrap();
        let ours = a.dial(&Peer::by_key(key(2))).await.unwrap();
        let theirs = b.accept().await.unwrap();
        (a, b, ours, theirs)
    }

    fn bytes(text: &'static str) -> Bytes {
        Bytes::from_static(text.as_bytes())
    }

    /// Whether `future` is still waiting after a while. Time is virtual, so this is exact.
    async fn waits<T>(future: impl Future<Output = T>) -> bool {
        timeout(SOON, future).await.is_err()
    }

    #[tokio::test]
    async fn a_dial_gives_each_end_the_others_key_and_its_own_side() {
        let (_a, _b, ours, theirs) = connected().await;
        assert_eq!(ours.peer(), key(2));
        assert_eq!(ours.side(), Side::Dialled);
        assert_eq!(theirs.peer(), key(1));
        assert_eq!(theirs.side(), Side::Accepted);
    }

    #[tokio::test]
    async fn a_key_nobody_holds_cannot_be_dialled_and_nor_can_ones_own() {
        let network = MemoryNetwork::new();
        let a = network.endpoint(key(1)).unwrap();
        assert_eq!(
            a.dial(&Peer::by_key(key(9))).await.err(),
            Some(DialError::Unreachable)
        );
        assert_eq!(
            a.dial(&Peer::by_key(key(1))).await.err(),
            Some(DialError::Unreachable)
        );
    }

    #[tokio::test]
    async fn a_key_is_held_by_one_open_endpoint_at_a_time() {
        let network = MemoryNetwork::new();
        let first = network.endpoint(key(1)).unwrap();
        assert!(network.endpoint(key(1)).is_none());
        first.close();
        assert!(network.endpoint(key(1)).is_some());
    }

    #[tokio::test]
    async fn a_one_way_stream_delivers_in_order_and_then_ends() {
        let (_a, _b, ours, theirs) = connected().await;
        let mut send = ours.open_uni().await.unwrap();
        let mut recv = theirs.accept_uni().await.unwrap();
        send.write(bytes("one")).await.unwrap();
        send.write(bytes("two")).await.unwrap();
        send.finish();
        assert_eq!(recv.read().await.unwrap(), Some(bytes("one")));
        assert_eq!(recv.read().await.unwrap(), Some(bytes("two")));
        assert_eq!(recv.read().await.unwrap(), None);
        assert_eq!(recv.read().await.unwrap(), None);
        // Nothing more can be written.
        assert_eq!(send.write(bytes("x")).await, Err(StreamError::Finished));
    }

    #[tokio::test]
    async fn a_two_way_stream_carries_a_request_and_its_answer() {
        let (_a, _b, ours, theirs) = connected().await;
        let (mut ask, mut answer) = theirs.open_bidi().await.unwrap();
        let (mut reply, mut question) = ours.accept_bidi().await.unwrap();
        ask.write(bytes("tracks?")).await.unwrap();
        assert_eq!(question.read().await.unwrap(), Some(bytes("tracks?")));
        reply.write(bytes("ok")).await.unwrap();
        reply.finish();
        assert_eq!(answer.read().await.unwrap(), Some(bytes("ok")));
        assert_eq!(answer.read().await.unwrap(), None);
        // The asker's direction is still open.
        ask.write(bytes("more")).await.unwrap();
        assert_eq!(question.read().await.unwrap(), Some(bytes("more")));
    }

    #[tokio::test]
    async fn streams_are_accepted_in_the_order_they_were_opened() {
        let (_a, _b, ours, theirs) = connected().await;
        for n in 0..5u8 {
            let mut send = ours.open_uni().await.unwrap();
            send.write(Bytes::from(vec![n])).await.unwrap();
            send.finish();
        }
        for n in 0..5u8 {
            let mut recv = theirs.accept_uni().await.unwrap();
            assert_eq!(recv.read().await.unwrap(), Some(Bytes::from(vec![n])));
        }
    }

    #[tokio::test]
    async fn a_reset_reaches_the_reader_with_its_code() {
        let (_a, _b, ours, theirs) = connected().await;
        let mut send = ours.open_uni().await.unwrap();
        let mut recv = theirs.accept_uni().await.unwrap();
        send.write(bytes("lost")).await.unwrap();
        send.reset(7);
        assert_eq!(recv.read().await, Err(StreamError::Reset(7)));
    }

    #[tokio::test]
    async fn dropping_a_sender_that_has_not_finished_is_a_reset() {
        let (_a, _b, ours, theirs) = connected().await;
        let send = ours.open_uni().await.unwrap();
        let mut recv = theirs.accept_uni().await.unwrap();
        drop(send);
        assert_eq!(recv.read().await, Err(StreamError::Reset(0)));
    }

    #[tokio::test]
    async fn a_stop_reaches_the_writer_with_its_code() {
        let (_a, _b, ours, theirs) = connected().await;
        let mut send = ours.open_uni().await.unwrap();
        let mut recv = theirs.accept_uni().await.unwrap();
        recv.stop(9);
        assert_eq!(send.write(bytes("x")).await, Err(StreamError::Stopped(9)));
        assert_eq!(recv.read().await, Err(StreamError::Finished));
    }

    #[tokio::test(start_paused = true)]
    async fn a_writer_waits_for_a_reader_that_is_behind() {
        let (_a, _b, ours, theirs) = connected().await;
        let mut send = ours.open_uni().await.unwrap();
        let mut recv = theirs.accept_uni().await.unwrap();
        for _ in 0..STREAM_QUEUE {
            send.write(bytes("chunk")).await.unwrap();
        }
        assert!(waits(send.write(bytes("one too many"))).await);
        // The reader takes one; now there is room for one.
        recv.read().await.unwrap();
        send.write(bytes("fits")).await.unwrap();
        assert!(waits(send.write(bytes("one too many"))).await);
    }

    #[tokio::test]
    async fn a_datagram_arrives_whole() {
        let (_a, _b, ours, theirs) = connected().await;
        ours.send_datagram(bytes("frame 1")).unwrap();
        ours.send_datagram(bytes("frame 2")).unwrap();
        theirs.send_datagram(bytes("back")).unwrap();
        assert_eq!(theirs.read_datagram().await.unwrap(), bytes("frame 1"));
        assert_eq!(theirs.read_datagram().await.unwrap(), bytes("frame 2"));
        assert_eq!(ours.read_datagram().await.unwrap(), bytes("back"));
    }

    #[tokio::test]
    async fn a_datagram_larger_than_the_path_takes_is_refused() {
        let (_a, _b, ours, _theirs) = connected().await;
        let size = ours.max_datagram_size();
        assert!(ours.send_datagram(Bytes::from(vec![0; size])).is_ok());
        assert_eq!(
            ours.send_datagram(Bytes::from(vec![0; size + 1])),
            Err(DatagramError::TooLarge)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn datagrams_nobody_reads_are_dropped_not_queued_without_limit() {
        let (_a, _b, ours, theirs) = connected().await;
        for n in 0..DATAGRAM_QUEUE + 50 {
            ours.send_datagram(Bytes::from(n.to_be_bytes().to_vec()))
                .unwrap();
        }
        // The first ones are kept, the overflow is gone.
        for n in 0..DATAGRAM_QUEUE {
            assert_eq!(
                theirs.read_datagram().await.unwrap(),
                Bytes::from(n.to_be_bytes().to_vec())
            );
        }
        assert!(waits(theirs.read_datagram()).await);
    }

    #[tokio::test]
    async fn a_close_is_seen_by_both_ends_with_its_code_and_reason() {
        let (_a, _b, ours, theirs) = connected().await;
        ours.close(3, "not in the plan");
        let seen = theirs.closed().await;
        assert_eq!(
            seen,
            Closed {
                by: ClosedBy::Peer,
                code: 3,
                reason: "not in the plan".into()
            }
        );
        assert_eq!(ours.closed().await.by, ClosedBy::Us);
        // The first close stands.
        theirs.close(4, "too late");
        assert_eq!(ours.closed().await.code, 3);
    }

    #[tokio::test]
    async fn a_close_ends_everything_waiting_on_the_connection() {
        let (_a, _b, ours, theirs) = connected().await;
        let mut send = ours.open_uni().await.unwrap();
        let mut recv = theirs.accept_uni().await.unwrap();
        for _ in 0..STREAM_QUEUE {
            send.write(bytes("chunk")).await.unwrap();
        }
        let (mut idle_send, mut idle_recv) = ours.open_bidi().await.unwrap();
        let waiting = {
            let (ours, theirs) = (ours.clone(), theirs.clone());
            tokio::spawn(async move {
                tokio::join!(
                    theirs.accept_bidi_twice(),
                    theirs.read_datagram(),
                    ours.accept_uni(),
                    send.write(bytes("blocked")),
                    idle_recv.read(),
                )
            })
        };
        tokio::task::yield_now().await;
        theirs.close(1, "bye");
        let (bidi, datagram, uni, write, read) = waiting.await.unwrap();
        assert_eq!(bidi.err().map(|c| c.by), Some(ClosedBy::Us));
        assert_eq!(datagram.err().map(|c| c.by), Some(ClosedBy::Us));
        assert_eq!(uni.err().map(|c| c.by), Some(ClosedBy::Peer));
        assert!(matches!(write, Err(StreamError::Closed(_))));
        assert!(matches!(read, Err(StreamError::Closed(_))));
        // And everything asked for afterwards.
        assert!(ours.open_uni().await.is_err());
        assert!(matches!(
            ours.send_datagram(bytes("x")),
            Err(DatagramError::Closed(_))
        ));
        assert!(matches!(
            idle_send.write(bytes("x")).await,
            Err(StreamError::Closed(_))
        ));
        // Even what had arrived and was not yet read: a closed connection gives nothing.
        assert!(matches!(recv.read().await, Err(StreamError::Closed(_))));
    }

    impl MemoryConnection {
        /// Accept the stream that was opened, then wait for one that never comes.
        async fn accept_bidi_twice(&self) -> Result<(MemorySend, MemoryRecv), Closed> {
            let _first = self.accept_bidi().await?;
            self.accept_bidi().await
        }
    }

    #[tokio::test]
    async fn closing_an_endpoint_closes_its_connections_and_ends_accept() {
        let (a, b, ours, theirs) = connected().await;
        b.close();
        assert_eq!(ours.closed().await.by, ClosedBy::Peer);
        assert_eq!(theirs.closed().await.by, ClosedBy::Us);
        assert!(b.accept().await.is_none());
        // It can no longer be reached, nor dial.
        assert_eq!(
            a.dial(&Peer::by_key(key(2))).await.err(),
            Some(DialError::Unreachable)
        );
        assert_eq!(
            b.dial(&Peer::by_key(key(1))).await.err(),
            Some(DialError::EndpointClosed)
        );
    }

    #[tokio::test]
    async fn a_connection_dialled_but_not_yet_accepted_closes_with_the_endpoint() {
        let network = MemoryNetwork::new();
        let a = network.endpoint(key(1)).unwrap();
        let b = network.endpoint(key(2)).unwrap();
        let ours = a.dial(&Peer::by_key(key(2))).await.unwrap();
        drop(b);
        assert_eq!(ours.closed().await.by, ClosedBy::Peer);
    }

    #[tokio::test]
    async fn a_connection_nobody_holds_is_closed_for_the_other_end() {
        let (_a, b, ours, theirs) = connected().await;
        drop(theirs);
        // The endpoint still holds it, so it is open until the endpoint lets go.
        drop(b);
        assert_eq!(ours.closed().await.by, ClosedBy::Peer);
    }

    #[tokio::test(start_paused = true)]
    async fn an_endpoint_behind_a_nat_is_reached_only_by_those_it_has_dialled() {
        let network = MemoryNetwork::new();
        let open = network.endpoint(key(1)).unwrap();
        let hidden = network.endpoint(key(2)).unwrap();
        network.put_behind_nat(key(2));
        assert_eq!(
            open.dial(&Peer::by_key(key(2))).await.err(),
            Some(DialError::Unreachable)
        );
        // It can dial out, and that opens the way back in.
        assert!(hidden.dial(&Peer::by_key(key(1))).await.is_ok());
        assert!(open.dial(&Peer::by_key(key(2))).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn two_endpoints_behind_nats_connect_when_both_dial() {
        let network = MemoryNetwork::new();
        let a = Arc::new(network.endpoint(key(1)).unwrap());
        let b = network.endpoint(key(2)).unwrap();
        network.put_behind_nat(key(1));
        network.put_behind_nat(key(2));
        let first = {
            let a = Arc::clone(&a);
            tokio::spawn(async move { a.dial(&Peer::by_key(key(2))).await.is_ok() })
        };
        // The first dial is still waiting when the second comes the other way.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(b.dial(&Peer::by_key(key(1))).await.is_ok());
        assert!(first.await.unwrap());
    }
}
