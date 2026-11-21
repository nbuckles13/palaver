//! Admission: only a node that holds the meeting secret gets a connection.
//!
//! The transport proves which key is at the other end. Admission proves that the holder of
//! that key also holds the meeting secret, in both directions, before a single MoQ message
//! is sent. It runs on the first two-way stream of a connection:
//!
//! ```text
//! dialler  → version (1 byte), dialler's nonce (32)
//! accepter → accepter's nonce (32), accepter's proof (32)
//! dialler  → dialler's proof (32)
//! accepter → admitted (1 byte)
//! ```
//!
//! A proof is an HMAC-SHA-256, under a key derived from the meeting secret, of: a label,
//! which end is proving, both ends' transport keys, and both nonces. So a proof is good
//! for one connection between one pair of keys, in one direction: it cannot be replayed on
//! another connection (the nonces differ), passed on by a node in the middle (the keys
//! differ), or sent back to its maker as the maker's own proof (the ends differ).
//!
//! Either end that sees a wrong proof, a wrong version, or nothing for
//! [`ADMISSION_TIMEOUT`] closes the connection.
//!
//! This is the phase 1 handshake. It is sound because the meeting secret is 32 random
//! bytes from a ticket. A password a person can remember would be open to guessing by
//! anyone who can dial, which is why the password handshake of a later phase is a PAKE.

use crate::transport::{Connection, DialError, Endpoint, Peer, RecvStream, SendStream, Side};
use bytes::Bytes;
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use palaver_proto::{PublicKey, SECRET_BYTES, Secret};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;

/// How long the whole handshake may take.
pub const ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);
/// The close code of a connection that was not admitted.
pub const CLOSE_NOT_ADMITTED: u64 = 0x110;

const VERSION: u8 = 1;
const ADMITTED: u8 = 1;
const NONCE_BYTES: usize = 32;
const PROOF_BYTES: usize = 32;
/// Says what the HKDF output is for. Changing it locks every older node out.
const KEY_LABEL: &[u8] = b"palaver admission key v1";
const PROOF_LABEL: &[u8] = b"palaver admission proof v1";
/// Connections admitted and not yet taken by the caller.
const ADMITTED_QUEUE: usize = 16;

/// The key proofs are made with: derived from the meeting secret, and no use for anything
/// else.
#[derive(Clone)]
pub struct AdmissionKey(Secret<[u8; 32]>);

impl AdmissionKey {
    pub fn from_meeting_secret(secret: &Secret<[u8; SECRET_BYTES]>) -> Option<Self> {
        let mut key = [0u8; 32];
        Hkdf::<Sha256>::new(None, secret.expose())
            .expand(KEY_LABEL, &mut key)
            .ok()?;
        Some(Self(Secret::new(key)))
    }

    /// The proof that `prover` (the end that dialled, or the one that accepted) holds
    /// this key, for the connection these keys and nonces belong to.
    fn mac(
        &self,
        prover: Side,
        dialler: &PublicKey,
        accepter: &PublicKey,
        dialler_nonce: &[u8],
        accepter_nonce: &[u8],
    ) -> Option<Hmac<Sha256>> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(self.0.expose()).ok()?;
        mac.update(PROOF_LABEL);
        mac.update(&[
            0,
            match prover {
                Side::Dialled => 1,
                Side::Accepted => 2,
            },
        ]);
        mac.update(&dialler.0);
        mac.update(&accepter.0);
        mac.update(dialler_nonce);
        mac.update(accepter_nonce);
        Some(mac)
    }
}

/// Why a connection was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionError {
    /// The other end did not prove it holds the meeting secret, or said we did not.
    Refused,
    /// The other end does not speak this handshake, or not this version of it.
    NotUnderstood,
    /// Nothing, or not enough, came in time.
    TimedOut,
    /// The connection ended first.
    Closed,
    /// The operating system would not give random bytes.
    NoRandomness,
}

/// Run the handshake on a new connection, as the end `connection` says we are. On any
/// failure the connection is closed.
pub async fn admit<C: Connection>(
    connection: &C,
    us: PublicKey,
    key: &AdmissionKey,
) -> Result<(), AdmissionError> {
    let handshake = async {
        match connection.side() {
            Side::Dialled => as_dialler(connection, us, key).await,
            Side::Accepted => as_accepter(connection, us, key).await,
        }
    };
    let result = match tokio::time::timeout(ADMISSION_TIMEOUT, handshake).await {
        Ok(result) => result,
        Err(_) => Err(AdmissionError::TimedOut),
    };
    if result.is_err() {
        connection.close(CLOSE_NOT_ADMITTED, "not admitted");
    }
    result
}

async fn as_dialler<C: Connection>(
    connection: &C,
    us: PublicKey,
    key: &AdmissionKey,
) -> Result<(), AdmissionError> {
    let them = connection.peer();
    let (mut send, recv) = connection
        .open_bidi()
        .await
        .map_err(|_| AdmissionError::Closed)?;
    let mut recv = Buffered::new(recv);

    let our_nonce = nonce()?;
    let mut hello = vec![VERSION];
    hello.extend_from_slice(&our_nonce);
    write(&mut send, hello).await?;

    let reply = recv.take(NONCE_BYTES + PROOF_BYTES).await?;
    let (their_nonce, their_proof) = reply
        .split_at_checked(NONCE_BYTES)
        .ok_or(AdmissionError::NotUnderstood)?;
    key.mac(Side::Accepted, &us, &them, &our_nonce, their_nonce)
        .ok_or(AdmissionError::Refused)?
        .verify_slice(their_proof)
        .map_err(|_| AdmissionError::Refused)?;

    let proof = key
        .mac(Side::Dialled, &us, &them, &our_nonce, their_nonce)
        .ok_or(AdmissionError::Refused)?
        .finalize()
        .into_bytes();
    write(&mut send, proof.to_vec()).await?;
    send.finish();

    // The accepter closes the connection if it does not like the proof.
    match recv.take(1).await {
        Ok(verdict) if verdict == [ADMITTED] => Ok(()),
        Ok(_) | Err(AdmissionError::Closed) => Err(AdmissionError::Refused),
        Err(other) => Err(other),
    }
}

async fn as_accepter<C: Connection>(
    connection: &C,
    us: PublicKey,
    key: &AdmissionKey,
) -> Result<(), AdmissionError> {
    let them = connection.peer();
    let (mut send, recv) = connection
        .accept_bidi()
        .await
        .map_err(|_| AdmissionError::Closed)?;
    let mut recv = Buffered::new(recv);

    let hello = recv.take(1 + NONCE_BYTES).await?;
    let (version, their_nonce) = hello.split_first().ok_or(AdmissionError::NotUnderstood)?;
    if *version != VERSION {
        return Err(AdmissionError::NotUnderstood);
    }

    let our_nonce = nonce()?;
    let proof = key
        .mac(Side::Accepted, &them, &us, their_nonce, &our_nonce)
        .ok_or(AdmissionError::Refused)?
        .finalize()
        .into_bytes();
    let mut reply = our_nonce.to_vec();
    reply.extend_from_slice(&proof);
    write(&mut send, reply).await?;

    let their_proof = recv.take(PROOF_BYTES).await?;
    key.mac(Side::Dialled, &them, &us, their_nonce, &our_nonce)
        .ok_or(AdmissionError::Refused)?
        .verify_slice(&their_proof)
        .map_err(|_| AdmissionError::Refused)?;

    write(&mut send, vec![ADMITTED]).await?;
    send.finish();
    Ok(())
}

fn nonce() -> Result<[u8; NONCE_BYTES], AdmissionError> {
    let mut nonce = [0u8; NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|_| AdmissionError::NoRandomness)?;
    Ok(nonce)
}

async fn write<S: SendStream>(send: &mut S, bytes: Vec<u8>) -> Result<(), AdmissionError> {
    send.write(Bytes::from(bytes))
        .await
        .map_err(|_| AdmissionError::Closed)
}

/// A stream read a fixed number of bytes at a time, however they were cut up on the way.
struct Buffered<R> {
    recv: R,
    held: Vec<u8>,
}

impl<R: RecvStream> Buffered<R> {
    fn new(recv: R) -> Self {
        Self {
            recv,
            held: Vec::new(),
        }
    }

    async fn take(&mut self, count: usize) -> Result<Vec<u8>, AdmissionError> {
        while self.held.len() < count {
            match self.recv.read().await {
                Ok(Some(bytes)) => self.held.extend_from_slice(&bytes),
                // The stream ended, or was reset, short of what the handshake needs.
                Ok(None) => return Err(AdmissionError::NotUnderstood),
                Err(_) => return Err(AdmissionError::Closed),
            }
        }
        let rest = self.held.split_off(count);
        Ok(std::mem::replace(&mut self.held, rest))
    }
}

/// An endpoint that gives out only admitted connections: every connection it dials or
/// accepts has passed the handshake before the caller sees it.
///
/// Handshakes on incoming connections run side by side, so one that stalls holds up
/// nobody else. Must be made inside a tokio runtime.
pub struct Admitted<E: Endpoint> {
    inner: Arc<E>,
    key: AdmissionKey,
    admitted: AsyncMutex<mpsc::Receiver<E::Connection>>,
    accepting: JoinHandle<()>,
}

impl<E: Endpoint> Admitted<E> {
    pub fn new(inner: E, key: AdmissionKey) -> Self {
        let inner = Arc::new(inner);
        let (pass, admitted) = mpsc::channel(ADMITTED_QUEUE);
        let accepting = {
            let (inner, key) = (Arc::clone(&inner), key.clone());
            tokio::spawn(async move {
                let us = inner.key();
                while let Some(connection) = inner.accept().await {
                    let (key, pass) = (key.clone(), pass.clone());
                    tokio::spawn(async move {
                        if admit(&connection, us, &key).await.is_ok() {
                            let _ = pass.send(connection).await;
                        }
                    });
                }
            })
        };
        Self {
            inner,
            key,
            admitted: AsyncMutex::new(admitted),
            accepting,
        }
    }
}

impl<E: Endpoint> Drop for Admitted<E> {
    fn drop(&mut self) {
        self.accepting.abort();
        self.inner.close();
    }
}

impl<E: Endpoint> Endpoint for Admitted<E> {
    type Connection = E::Connection;

    fn key(&self) -> PublicKey {
        self.inner.key()
    }

    fn dials_by_address(&self) -> bool {
        self.inner.dials_by_address()
    }

    /// [`DialError::Refused`] if the peer was reached and the handshake failed.
    async fn dial(&self, peer: &Peer) -> Result<E::Connection, DialError> {
        let connection = self.inner.dial(peer).await?;
        match admit(&connection, self.inner.key(), &self.key).await {
            Ok(()) => Ok(connection),
            Err(_) => Err(DialError::Refused),
        }
    }

    async fn accept(&self) -> Option<E::Connection> {
        self.admitted.lock().await.recv().await
    }

    fn close(&self) {
        self.inner.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryConnection, MemoryEndpoint, MemoryNetwork};
    use crate::transport::ClosedBy;
    use tokio::time::{Instant, timeout};

    fn key(n: u8) -> PublicKey {
        PublicKey([n; 32])
    }

    fn admission(secret: u8) -> AdmissionKey {
        AdmissionKey::from_meeting_secret(&Secret::new([secret; SECRET_BYTES])).unwrap()
    }

    fn guarded(network: &MemoryNetwork, n: u8, secret: u8) -> Admitted<MemoryEndpoint> {
        Admitted::new(network.endpoint(key(n)).unwrap(), admission(secret))
    }

    /// Whether `future` is still waiting after a second.
    async fn waits<T>(future: impl Future<Output = T>) -> bool {
        timeout(Duration::from_secs(1), future).await.is_err()
    }

    #[tokio::test(start_paused = true)]
    async fn two_nodes_with_the_same_secret_are_connected_and_can_then_talk() {
        let network = MemoryNetwork::new();
        let (a, b) = (guarded(&network, 1, 7), guarded(&network, 2, 7));
        let ours = a.dial(&Peer::by_key(key(2))).await.unwrap();
        let theirs = b.accept().await.unwrap();
        assert_eq!((ours.peer(), theirs.peer()), (key(2), key(1)));
        // The handshake's stream is out of the way: the next stream is the caller's.
        let mut send = ours.open_bidi().await.unwrap().0;
        send.write(Bytes::from_static(b"setup")).await.unwrap();
        let mut recv = theirs.accept_bidi().await.unwrap().1;
        assert_eq!(
            recv.read().await.unwrap(),
            Some(Bytes::from_static(b"setup"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_node_with_another_secret_is_refused_and_never_seen_by_the_caller() {
        let network = MemoryNetwork::new();
        let (a, b) = (guarded(&network, 1, 7), guarded(&network, 2, 8));
        assert_eq!(
            a.dial(&Peer::by_key(key(2))).await.err(),
            Some(DialError::Refused)
        );
        assert!(waits(b.accept()).await);
        // And the other way round.
        assert_eq!(
            b.dial(&Peer::by_key(key(1))).await.err(),
            Some(DialError::Refused)
        );
        assert!(waits(a.accept()).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dialler_that_says_nothing_is_closed_after_the_timeout_and_holds_up_nobody() {
        let network = MemoryNetwork::new();
        let b = guarded(&network, 2, 7);
        let silent = network.endpoint(key(3)).unwrap();
        let started = Instant::now();
        let stalled = silent.dial(&Peer::by_key(key(2))).await.unwrap();
        // A node with the secret is admitted while the silent one is still being waited for.
        let a = guarded(&network, 1, 7);
        a.dial(&Peer::by_key(key(2))).await.unwrap();
        assert_eq!(b.accept().await.unwrap().peer(), key(1));
        assert!(started.elapsed() < ADMISSION_TIMEOUT);
        // Then the silent one is closed.
        let closed = stalled.closed().await;
        assert_eq!(
            (closed.by, closed.code),
            (ClosedBy::Peer, CLOSE_NOT_ADMITTED)
        );
        assert_eq!(started.elapsed(), ADMISSION_TIMEOUT);
        assert!(waits(b.accept()).await);
    }

    #[tokio::test(start_paused = true)]
    async fn an_accepter_that_does_not_answer_is_given_up_on() {
        let network = MemoryNetwork::new();
        let _mute = network.endpoint(key(2)).unwrap();
        let a = guarded(&network, 1, 7);
        let started = Instant::now();
        assert_eq!(
            a.dial(&Peer::by_key(key(2))).await.err(),
            Some(DialError::Refused)
        );
        assert_eq!(started.elapsed(), ADMISSION_TIMEOUT);
    }

    /// Dial `b` without the handshake and send `bytes` on the first stream. Returns the
    /// connection and the stream's halves.
    async fn dial_and_say(
        from: &MemoryEndpoint,
        bytes: Vec<u8>,
    ) -> (MemoryConnection, impl SendStream, impl RecvStream) {
        let connection = from.dial(&Peer::by_key(key(2))).await.unwrap();
        let (mut send, recv) = connection.open_bidi().await.unwrap();
        send.write(Bytes::from(bytes)).await.unwrap();
        (connection, send, recv)
    }

    #[tokio::test(start_paused = true)]
    async fn a_version_it_does_not_know_and_a_stream_cut_short_are_both_closed_at_once() {
        let network = MemoryNetwork::new();
        let _b = guarded(&network, 2, 7);
        let raw = network.endpoint(key(3)).unwrap();
        let started = Instant::now();

        let mut hello = vec![9];
        hello.extend_from_slice(&[0; NONCE_BYTES]);
        let (connection, _send, _recv) = dial_and_say(&raw, hello).await;
        assert_eq!(connection.closed().await.code, CLOSE_NOT_ADMITTED);

        let (connection, mut send, _recv) = dial_and_say(&raw, vec![VERSION, 1, 2, 3]).await;
        send.finish();
        assert_eq!(connection.closed().await.code, CLOSE_NOT_ADMITTED);
        // Neither waited for the timeout.
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_proof_made_without_the_secret_is_refused() {
        let network = MemoryNetwork::new();
        let b = guarded(&network, 2, 7);
        let raw = network.endpoint(key(3)).unwrap();
        let mut hello = vec![VERSION];
        hello.extend_from_slice(&[5; NONCE_BYTES]);
        let (connection, mut send, recv) = dial_and_say(&raw, hello).await;
        let mut recv = Buffered::new(recv);
        // The accepter proves itself first; an outsider learns nothing it can use from that.
        let reply = recv.take(NONCE_BYTES + PROOF_BYTES).await.unwrap();
        assert_eq!(reply.len(), 64);
        // Sending the accepter's own proof back does not pass for the dialler's.
        send.write(Bytes::from(reply[NONCE_BYTES..].to_vec()))
            .await
            .unwrap();
        assert_eq!(connection.closed().await.code, CLOSE_NOT_ADMITTED);
        assert!(waits(b.accept()).await);
    }

    #[tokio::test(start_paused = true)]
    async fn the_handshake_survives_being_cut_into_single_bytes() {
        let network = MemoryNetwork::new();
        let b = guarded(&network, 2, 7);
        let raw = network.endpoint(key(1)).unwrap();
        let connection = raw.dial(&Peer::by_key(key(2))).await.unwrap();
        let (mut send, recv) = connection.open_bidi().await.unwrap();
        let mut recv = Buffered::new(recv);
        let key_7 = admission(7);

        let our_nonce = [3u8; NONCE_BYTES];
        let mut hello = vec![VERSION];
        hello.extend_from_slice(&our_nonce);
        for byte in hello {
            send.write(Bytes::from(vec![byte])).await.unwrap();
        }
        let reply = recv.take(NONCE_BYTES + PROOF_BYTES).await.unwrap();
        let their_nonce = &reply[..NONCE_BYTES];
        let proof = key_7
            .mac(Side::Dialled, &key(1), &key(2), &our_nonce, their_nonce)
            .unwrap()
            .finalize()
            .into_bytes();
        for byte in proof {
            send.write(Bytes::from(vec![byte])).await.unwrap();
        }
        assert_eq!(recv.take(1).await.unwrap(), [ADMITTED]);
        assert_eq!(b.accept().await.unwrap().peer(), key(1));
    }

    #[test]
    fn a_proof_is_for_one_end_of_one_connection_between_one_pair_of_keys() {
        let k = admission(7);
        let proof = |prover, d: u8, a: u8, dn: u8, an: u8, with: &AdmissionKey| {
            with.mac(prover, &key(d), &key(a), &[dn; 32], &[an; 32])
                .unwrap()
                .finalize()
                .into_bytes()
                .to_vec()
        };
        let base = proof(Side::Dialled, 1, 2, 3, 4, &k);
        assert_eq!(base, proof(Side::Dialled, 1, 2, 3, 4, &k));
        // The other end, other keys, the keys swapped, other nonces, another secret.
        assert_ne!(base, proof(Side::Accepted, 1, 2, 3, 4, &k));
        assert_ne!(base, proof(Side::Dialled, 9, 2, 3, 4, &k));
        assert_ne!(base, proof(Side::Dialled, 1, 9, 3, 4, &k));
        assert_ne!(base, proof(Side::Dialled, 2, 1, 3, 4, &k));
        assert_ne!(base, proof(Side::Dialled, 1, 2, 9, 4, &k));
        assert_ne!(base, proof(Side::Dialled, 1, 2, 3, 9, &k));
        assert_ne!(base, proof(Side::Dialled, 1, 2, 3, 4, &admission(8)));
    }

    #[test]
    fn the_admission_key_is_not_the_meeting_tag_nor_the_secret() {
        let secret = Secret::new([7; SECRET_BYTES]);
        let key = AdmissionKey::from_meeting_secret(&secret).unwrap();
        let tag = crate::identity::meeting_tag(&secret).unwrap();
        assert_ne!(key.0.expose(), secret.expose());
        assert_ne!(&key.0.expose()[..16], tag.as_bytes());
    }
}
