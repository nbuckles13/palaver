//! The real transport: QUIC, by way of quinn.
//!
//! A [`QuicEndpoint`] is a node's presence on the network: one UDP socket, used both to
//! dial and to accept.
//!
//! **A connection is to a key.** Each end presents its Ed25519 public key in the TLS
//! handshake as a raw public key (RFC 7250, no certificates), and proves it holds the
//! matching secret key by signing the handshake. Then:
//!
//! - the end that dials accepts only the key it meant to reach, so an address that is
//!   answered by someone else gives no connection;
//! - the end that accepts takes any Ed25519 key, and learns which. Whether that key may
//!   join is not decided here: that is admission's job ([`crate::admission`]).
//!
//! TLS 1.3 only. Every connection uses the protocol name [`ALPN`]; a peer that does not
//! speak it is refused in the handshake.
//!
//! A peer is dialled at the addresses given for it, all at once, and the first to answer
//! is used. The endpoint can ask a STUN server what its own public address is
//! ([`QuicEndpoint::public_address`]); the question leaves from the same socket, so the
//! answer is the address QUIC traffic comes from ([`crate::stun`]).

use crate::identity::Identity;
use crate::stun::StunSocket;
use crate::transport::{
    Closed, ClosedBy, Connection, DatagramError, DialError, Endpoint, Path, Peer, RecvStream,
    SendStream, Side, StreamError,
};
use bytes::Bytes;
use palaver_proto::{PublicKey, Signer};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ConnectionError, ReadError, SendDatagramError, VarInt, WriteError};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature_with_raw_key,
};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer,
    UnixTime,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::{JoinHandle, JoinSet};

/// The name of the khef-to-khef protocol, with its version. Distinct from the standard
/// MoQ name, which is kept for a future standard client edge.
pub const ALPN: &[u8] = b"palaver/1";
/// How long a dial may take, by default, before the peer counts as unreachable.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections whose handshake is done and that the caller has not yet taken.
const ACCEPT_QUEUE: usize = 16;
/// Read at most this much from a stream at a time.
const READ_CHUNK: usize = 64 * 1024;
/// The name asked for in the handshake. Nothing checks it: the key is what is checked.
const SERVER_NAME: &str = "palaver";

/// An Ed25519 public key as TLS carries it: these bytes, then the 32 bytes of the key.
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];
/// An Ed25519 secret key as TLS libraries take it: these bytes, then the 32 bytes of the
/// seed.
const PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

fn spki_of(key: &PublicKey) -> Vec<u8> {
    let mut spki = SPKI_PREFIX.to_vec();
    spki.extend_from_slice(&key.0);
    spki
}

/// The key in what a peer presented, if it is an Ed25519 public key and nothing else.
fn key_of(presented: &[u8]) -> Option<PublicKey> {
    let key = presented.strip_prefix(&SPKI_PREFIX)?;
    Some(PublicKey(key.try_into().ok()?))
}

fn not_the_key() -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

fn tls13_signature(
    message: &[u8],
    presented: &CertificateDer<'_>,
    signature: &DigitallySignedStruct,
    algorithms: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    let key = SubjectPublicKeyInfoDer::from(presented.as_ref());
    verify_tls13_signature_with_raw_key(message, &key, signature, algorithms)
}

/// For the end that dials: the peer must present exactly this key.
#[derive(Debug)]
struct ExpectKey {
    expected: PublicKey,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ExpectKey {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if key_of(end_entity) == Some(self.expected) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(not_the_key())
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// For the end that accepts: the peer must present an Ed25519 key, any one, and prove it
/// holds it.
#[derive(Debug)]
struct AnyKey {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyKey {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        match key_of(end_entity) {
            Some(_) => Ok(ClientCertVerified::assertion()),
            None => Err(not_the_key()),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// How to set up a [`QuicEndpoint`].
#[derive(Debug, Clone)]
pub struct QuicOptions {
    /// The local address to listen on. The default is every IPv4 interface, any port.
    pub bind: SocketAddr,
    /// How long a dial may take before the peer counts as unreachable.
    pub dial_timeout: Duration,
    /// The protocol name to speak. Only tests have a reason to change it.
    pub alpn: Vec<u8>,
}

impl Default for QuicOptions {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            dial_timeout: DIAL_TIMEOUT,
            alpn: ALPN.to_vec(),
        }
    }
}

/// Why an endpoint could not be set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    /// The TLS library would not take the key or the settings.
    Tls(String),
    /// The socket could not be opened.
    Socket(String),
}

fn tls(error: impl ToString) -> BindError {
    BindError::Tls(error.to_string())
}

/// What STUN servers said of this node's address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reflexive {
    /// Nobody answered.
    Unknown,
    /// Every server that answered saw this address: others can be told to send here.
    /// With one answer only, that is a hope rather than a finding.
    Stable(SocketAddr),
    /// Servers saw different addresses: the router gives one per destination (a
    /// symmetric NAT), so no address can be handed out and a hole punch will not work.
    PerDestination,
}

impl Reflexive {
    fn from_answers(seen: Vec<SocketAddr>) -> Self {
        match seen.split_first() {
            None => Self::Unknown,
            Some((first, rest)) if rest.iter().all(|other| other == first) => Self::Stable(*first),
            Some(_) => Self::PerDestination,
        }
    }
}

/// A node's presence on the network.
pub struct QuicEndpoint {
    endpoint: quinn::Endpoint,
    socket: Arc<StunSocket>,
    key: PublicKey,
    /// This node's key as TLS presents it, with the means to sign for it.
    certified: Arc<CertifiedKey>,
    provider: Arc<CryptoProvider>,
    options: QuicOptions,
    accepted: AsyncMutex<mpsc::Receiver<QuicConnection>>,
    accepting: JoinHandle<()>,
}

impl QuicEndpoint {
    /// Open an endpoint known by `identity`'s key. Must be called inside a tokio runtime.
    pub fn bind(identity: &Identity, options: QuicOptions) -> Result<Self, BindError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let key = identity.public_key();

        let mut pkcs8 = PKCS8_PREFIX.to_vec();
        pkcs8.extend_from_slice(identity.secret().expose());
        let signing = provider
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8)))
            .map_err(tls)?;
        let certified = Arc::new(CertifiedKey::new(
            vec![CertificateDer::from(spki_of(&key))],
            signing,
        ));

        let mut server = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(tls)?
            .with_client_cert_verifier(Arc::new(AnyKey {
                algorithms: provider.signature_verification_algorithms,
            }))
            .with_cert_resolver(Arc::new(
                rustls::server::AlwaysResolvesServerRawPublicKeys::new(Arc::clone(&certified)),
            ));
        server.alpn_protocols = vec![options.alpn.clone()];
        let server = QuicServerConfig::try_from(server).map_err(tls)?;
        // One socket for everything: QUIC both ways, and the questions to STUN servers,
        // whose answers are taken out before QUIC sees them.
        let socket_error = |e: std::io::Error| BindError::Socket(e.to_string());
        let runtime = Arc::new(quinn::TokioRuntime);
        let socket = std::net::UdpSocket::bind(options.bind).map_err(socket_error)?;
        let socket = Arc::new(StunSocket::new(
            quinn::Runtime::wrap_udp_socket(&*runtime, socket).map_err(socket_error)?,
        ));
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(quinn::ServerConfig::with_crypto(Arc::new(server))),
            Arc::clone(&socket) as Arc<dyn quinn::AsyncUdpSocket>,
            runtime,
        )
        .map_err(socket_error)?;

        // Handshakes of incoming connections run side by side, so that one that stalls
        // holds up nobody else.
        let (pass, accepted) = mpsc::channel(ACCEPT_QUEUE);
        let accepting = {
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                while let Some(incoming) = endpoint.accept().await {
                    let pass = pass.clone();
                    tokio::spawn(async move {
                        let Ok(connection) = incoming.await else {
                            return;
                        };
                        if let Some(connection) = QuicConnection::new(connection, Side::Accepted) {
                            let _ = pass.send(connection).await;
                        }
                    });
                }
            })
        };
        Ok(Self {
            endpoint,
            socket,
            key,
            certified,
            provider,
            options,
            accepted: AsyncMutex::new(accepted),
            accepting,
        })
    }

    /// The local address the endpoint is listening on.
    pub fn local_address(&self) -> Option<SocketAddr> {
        self.endpoint.local_addr().ok()
    }

    /// The address `server`, a STUN server, sees this endpoint's packets come from: its
    /// public address, if it is behind a router that rewrites addresses. `None` if no
    /// answer comes within `patience`.
    ///
    /// The server learns this node's address and that it asked; nothing else is sent.
    pub async fn public_address(
        &self,
        server: SocketAddr,
        patience: Duration,
    ) -> Option<SocketAddr> {
        self.socket.mapped_address(server, patience).await
    }

    /// Ask several STUN servers at once. What they agree on tells what kind of router
    /// this node is behind.
    pub async fn public_addresses(&self, servers: &[SocketAddr], patience: Duration) -> Reflexive {
        let mut asking = JoinSet::new();
        for server in servers {
            let (socket, server) = (Arc::clone(&self.socket), *server);
            asking.spawn(async move { socket.mapped_address(server, patience).await });
        }
        let mut seen = Vec::new();
        while let Some(answer) = asking.join_next().await {
            if let Ok(Some(address)) = answer {
                seen.push(address);
            }
        }
        Reflexive::from_answers(seen)
    }

    /// The settings for dialling `peer`: present our key, accept only theirs.
    fn dialling(&self, peer: PublicKey) -> Option<quinn::ClientConfig> {
        let mut client = rustls::ClientConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .ok()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(ExpectKey {
                expected: peer,
                algorithms: self.provider.signature_verification_algorithms,
            }))
            .with_client_cert_resolver(Arc::new(
                rustls::client::AlwaysResolvesClientRawPublicKeys::new(Arc::clone(&self.certified)),
            ));
        client.alpn_protocols = vec![self.options.alpn.clone()];
        let client = QuicClientConfig::try_from(client).ok()?;
        Some(quinn::ClientConfig::new(Arc::new(client)))
    }
}

impl Drop for QuicEndpoint {
    fn drop(&mut self) {
        self.close();
    }
}

impl Endpoint for QuicEndpoint {
    type Connection = QuicConnection;

    fn key(&self) -> PublicKey {
        self.key
    }

    fn dials_by_address(&self) -> bool {
        true
    }

    async fn dial(&self, peer: &Peer) -> Result<QuicConnection, DialError> {
        if peer.key == self.key {
            return Err(DialError::Unreachable);
        }
        let Some(config) = self.dialling(peer.key) else {
            return Err(DialError::Unreachable);
        };
        // Every address at once; the first to complete the handshake is the connection.
        let mut attempts = JoinSet::new();
        for address in &peer.addresses {
            match self
                .endpoint
                .connect_with(config.clone(), *address.reveal(), SERVER_NAME)
            {
                Ok(connecting) => {
                    attempts.spawn(connecting);
                }
                Err(quinn::ConnectError::EndpointStopping) => {
                    return Err(DialError::EndpointClosed);
                }
                Err(_) => {}
            }
        }
        let mut refused = false;
        let first = async {
            while let Some(attempt) = attempts.join_next().await {
                match attempt {
                    Ok(Ok(connection)) => return Some(connection),
                    // Something answered, and the handshake failed: not the key we
                    // asked for, or not our protocol.
                    Ok(Err(ConnectionError::TransportError(_)))
                    | Ok(Err(ConnectionError::ConnectionClosed(_)))
                    | Ok(Err(ConnectionError::ApplicationClosed(_))) => refused = true,
                    _ => {}
                }
            }
            None
        };
        let connection = tokio::time::timeout(self.options.dial_timeout, first)
            .await
            .ok()
            .flatten();
        match connection.and_then(|c| QuicConnection::new(c, Side::Dialled)) {
            Some(connection) => Ok(connection),
            None if refused => Err(DialError::Refused),
            None => Err(DialError::Unreachable),
        }
    }

    async fn accept(&self) -> Option<QuicConnection> {
        self.accepted.lock().await.recv().await
    }

    fn close(&self) {
        self.accepting.abort();
        self.endpoint.close(VarInt::from_u32(0), b"endpoint closed");
    }
}

/// One QUIC connection.
#[derive(Clone)]
pub struct QuicConnection {
    connection: quinn::Connection,
    peer: PublicKey,
    side: Side,
    /// What this end closed the connection with, if it did: QUIC tells the peer, and this
    /// is how this end's own waiters are told the same.
    ours: Arc<OnceLock<(u64, String)>>,
}

impl QuicConnection {
    /// `None`, closing the connection, if the peer's key cannot be read from the
    /// handshake; the handshake does not complete without one, so this is not expected.
    fn new(connection: quinn::Connection, side: Side) -> Option<Self> {
        let peer = connection
            .peer_identity()
            .and_then(|identity| identity.downcast::<Vec<CertificateDer<'static>>>().ok())
            .and_then(|presented| presented.first().and_then(|first| key_of(first)));
        let Some(peer) = peer else {
            connection.close(VarInt::from_u32(0), b"no key");
            return None;
        };
        Some(Self {
            connection,
            peer,
            side,
            ours: Arc::new(OnceLock::new()),
        })
    }

    fn closed_as(&self, error: &ConnectionError) -> Closed {
        match error {
            ConnectionError::ApplicationClosed(close) => Closed {
                by: ClosedBy::Peer,
                code: close.error_code.into_inner(),
                reason: String::from_utf8_lossy(&close.reason).into_owned(),
            },
            ConnectionError::LocallyClosed => {
                let (code, reason) = self.ours.get().cloned().unwrap_or_default();
                Closed {
                    by: ClosedBy::Us,
                    code,
                    reason,
                }
            }
            other => Closed {
                by: ClosedBy::Transport,
                code: 0,
                reason: other.to_string(),
            },
        }
    }

    fn check(&self) -> Result<(), Closed> {
        match self.connection.close_reason() {
            Some(error) => Err(self.closed_as(&error)),
            None => Ok(()),
        }
    }

    fn send_half(&self, send: quinn::SendStream) -> QuicSend {
        QuicSend {
            send,
            connection: self.clone(),
            done: false,
        }
    }

    fn recv_half(&self, recv: quinn::RecvStream) -> QuicRecv {
        QuicRecv {
            recv,
            connection: self.clone(),
            stopped: false,
        }
    }
}

fn code_of(code: u64) -> VarInt {
    VarInt::from_u64(code).unwrap_or(VarInt::MAX)
}

impl Connection for QuicConnection {
    type Send = QuicSend;
    type Recv = QuicRecv;

    fn peer(&self) -> PublicKey {
        self.peer
    }

    fn side(&self) -> Side {
        self.side
    }

    fn path(&self) -> Path {
        Path {
            rtt: self.connection.rtt(),
            remote: Some(self.connection.remote_address()),
        }
    }

    async fn open_uni(&self) -> Result<QuicSend, Closed> {
        match self.connection.open_uni().await {
            Ok(send) => Ok(self.send_half(send)),
            Err(error) => Err(self.closed_as(&error)),
        }
    }

    async fn open_bidi(&self) -> Result<(QuicSend, QuicRecv), Closed> {
        match self.connection.open_bi().await {
            Ok((send, recv)) => Ok((self.send_half(send), self.recv_half(recv))),
            Err(error) => Err(self.closed_as(&error)),
        }
    }

    async fn accept_uni(&self) -> Result<QuicRecv, Closed> {
        match self.connection.accept_uni().await {
            Ok(recv) => Ok(self.recv_half(recv)),
            Err(error) => Err(self.closed_as(&error)),
        }
    }

    async fn accept_bidi(&self) -> Result<(QuicSend, QuicRecv), Closed> {
        match self.connection.accept_bi().await {
            Ok((send, recv)) => Ok((self.send_half(send), self.recv_half(recv))),
            Err(error) => Err(self.closed_as(&error)),
        }
    }

    fn send_datagram(&self, datagram: Bytes) -> Result<(), DatagramError> {
        self.check().map_err(DatagramError::Closed)?;
        match self.connection.send_datagram(datagram) {
            Ok(()) => Ok(()),
            Err(SendDatagramError::ConnectionLost(error)) => {
                Err(DatagramError::Closed(self.closed_as(&error)))
            }
            // Too large for the path, or the peer takes no datagrams at all, in which
            // case nothing will ever fit.
            Err(_) => Err(DatagramError::TooLarge),
        }
    }

    async fn read_datagram(&self) -> Result<Bytes, Closed> {
        self.connection
            .read_datagram()
            .await
            .map_err(|error| self.closed_as(&error))
    }

    fn max_datagram_size(&self) -> usize {
        self.connection.max_datagram_size().unwrap_or(0)
    }

    fn close(&self, code: u64, reason: &str) {
        if self.connection.close_reason().is_some() {
            return;
        }
        // The first close stands.
        let _ = self.ours.set((code, reason.to_owned()));
        self.connection.close(code_of(code), reason.as_bytes());
    }

    async fn closed(&self) -> Closed {
        let error = self.connection.closed().await;
        self.closed_as(&error)
    }
}

/// The sending half of a QUIC stream.
pub struct QuicSend {
    send: quinn::SendStream,
    connection: QuicConnection,
    /// Finished or reset.
    done: bool,
}

impl SendStream for QuicSend {
    async fn write(&mut self, bytes: Bytes) -> Result<(), StreamError> {
        if self.done {
            return Err(StreamError::Finished);
        }
        match self.send.write_chunk(bytes).await {
            Ok(()) => Ok(()),
            Err(WriteError::Stopped(code)) => Err(StreamError::Stopped(code.into_inner())),
            Err(WriteError::ConnectionLost(error)) => {
                Err(StreamError::Closed(self.connection.closed_as(&error)))
            }
            Err(_) => Err(StreamError::Finished),
        }
    }

    fn finish(&mut self) {
        if !self.done {
            self.done = true;
            let _ = self.send.finish();
        }
    }

    fn reset(&mut self, code: u64) {
        if !self.done {
            self.done = true;
            let _ = self.send.reset(code_of(code));
        }
    }
}

impl Drop for QuicSend {
    /// A sender dropped before it finished abandons the stream. Left to itself, QUIC
    /// would finish it, and pass a stream that was cut off as one that was whole.
    fn drop(&mut self) {
        self.reset(0);
    }
}

/// The receiving half of a QUIC stream.
pub struct QuicRecv {
    recv: quinn::RecvStream,
    connection: QuicConnection,
    stopped: bool,
}

impl RecvStream for QuicRecv {
    async fn read(&mut self) -> Result<Option<Bytes>, StreamError> {
        if self.stopped {
            return Err(StreamError::Finished);
        }
        match self.recv.read_chunk(READ_CHUNK, true).await {
            Ok(Some(chunk)) => Ok(Some(chunk.bytes)),
            Ok(None) => Ok(None),
            Err(ReadError::Reset(code)) => Err(StreamError::Reset(code.into_inner())),
            Err(ReadError::ConnectionLost(error)) => {
                Err(StreamError::Closed(self.connection.closed_as(&error)))
            }
            Err(_) => Err(StreamError::Finished),
        }
    }

    fn stop(&mut self, code: u64) {
        if !self.stopped {
            self.stopped = true;
            let _ = self.recv.stop(code_of(code));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_survives_the_trip_through_its_tls_form_and_nothing_else_passes_for_one() {
        let key = PublicKey([7; 32]);
        let spki = spki_of(&key);
        assert_eq!(spki.len(), 44);
        assert_eq!(key_of(&spki), Some(key));
        // Too short, too long, and another kind of key (the algorithm bytes differ).
        assert_eq!(key_of(&spki[..43]), None);
        let mut long = spki.clone();
        long.push(0);
        assert_eq!(key_of(&long), None);
        let mut other = spki.clone();
        other[8] = 0x71;
        assert_eq!(key_of(&other), None);
        assert_eq!(key_of(&[]), None);
    }
}
