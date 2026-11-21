//! One participant: a node on a network, with a voice.
//!
//! A [`Participant`] is what an application or a bot starts with: a key of its own, an
//! endpoint, a running [`Engine`] for the meeting a secret names, and a [`Call`] to put
//! sound on it. It makes invitations and takes them:
//!
//! - [`ticket`](Participant::ticket): an invitation with this participant as the way in;
//! - [`join`](Participant::join) with [`entry_points`] of a ticket someone gave;
//! - [`reply_to`](Participant::reply_to) and [`reach`](Participant::reach): the ticket
//!   coming back, for a joiner nobody can answer (design section 3).
//!
//! It does nothing until told to [`create_meeting`](Participant::create_meeting) or to
//! [`join`](Participant::join).

use crate::{Call, Engine, EngineConfig};
use palaver_khef_core::LinkWatchConfig;
use palaver_media::jitter::Config as JitterConfig;
use palaver_media::{Channels, CodecError, EncoderConfig};
use palaver_net::admission::{AdmissionKey, Admitted};
use palaver_net::identity::{Identity, meeting_tag};
use palaver_net::memory::MemoryNetwork;
use palaver_net::quic::{BindError, QuicEndpoint, QuicOptions};
use palaver_net::reach::{self, Network, StunConfig};
use palaver_net::transport::{Endpoint, Peer};
use palaver_proto::{
    EntryPoint, MAX_ENTRY_POINTS, MediaTrack, Redacted, SECRET_BYTES, Secret, Signer, Source,
    Ticket,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;

/// How a participant shows itself and handles sound.
#[derive(Debug, Clone)]
pub struct Options {
    /// The name the user chose.
    pub name: String,
    /// How its voice is encoded, and in how many channels.
    pub encoder: EncoderConfig,
    pub jitter: JitterConfig,
    /// The channels it plays in.
    pub playout: Channels,
}

impl Options {
    /// One channel each way and the usual settings, under `name`.
    #[must_use]
    pub fn named(name: &str) -> Self {
        Self {
            name: name.into(),
            encoder: EncoderConfig::default(),
            jitter: JitterConfig::default(),
            playout: Channels::Mono,
        }
    }
}

/// Why a participant could not be started.
#[derive(Debug)]
pub enum StartError {
    /// The operating system would not give random bytes for a key.
    NoRandomness,
    /// Another endpoint on the network already has this key.
    KeyInUse,
    /// The meeting secret could not be turned into keys.
    Derivation,
    Codec(CodecError),
    /// The QUIC endpoint could not be opened.
    Bind(BindError),
}

/// A node on a network, with a voice.
pub struct Participant {
    call: Call,
    /// Where others can find it.
    address: Peer,
    /// The QUIC endpoint it runs on, if it is on the network.
    quic: Option<Arc<QuicEndpoint>>,
}

impl Participant {
    /// A participant on an in-memory network, for a meeting with this secret. Must be
    /// called inside a tokio runtime.
    ///
    /// # Errors
    ///
    /// When the operating system gives no random bytes for a key, the network already
    /// has an endpoint with that key, the secret cannot be turned into keys, or the
    /// codec cannot be set up.
    pub fn on_memory(
        network: &MemoryNetwork,
        secret: &Secret<[u8; SECRET_BYTES]>,
        options: Options,
    ) -> Result<Self, StartError> {
        let identity = Identity::generate().map_err(|_| StartError::NoRandomness)?;
        let endpoint = network
            .endpoint(identity.public_key())
            .ok_or(StartError::KeyInUse)?;
        Self::start(endpoint, identity, secret, options, Vec::new(), None)
    }

    /// A participant on the network, over QUIC, for a meeting with this secret. It tells
    /// the meeting it can be reached at `advertise`; if that is empty, at the address it
    /// is bound to, with the machine's outward address in place of "every interface".
    /// Must be called inside a tokio runtime.
    ///
    /// # Errors
    ///
    /// When the operating system gives no random bytes for a key, the QUIC endpoint
    /// cannot be opened, the secret cannot be turned into keys, or the codec cannot be
    /// set up.
    pub fn on_quic(
        quic: QuicOptions,
        advertise: Vec<SocketAddr>,
        secret: &Secret<[u8; SECRET_BYTES]>,
        options: Options,
    ) -> Result<Self, StartError> {
        let identity = Identity::generate().map_err(|_| StartError::NoRandomness)?;
        let endpoint = Arc::new(QuicEndpoint::bind(&identity, quic).map_err(StartError::Bind)?);
        let advertise = if advertise.is_empty() {
            endpoint
                .local_address()
                .and_then(reachable_form)
                .into_iter()
                .collect()
        } else {
            advertise
        };
        let quic = Some(Arc::clone(&endpoint));
        Self::start(endpoint, identity, secret, options, advertise, quic)
    }

    fn start<E: Endpoint>(
        endpoint: E,
        identity: Identity,
        secret: &Secret<[u8; SECRET_BYTES]>,
        options: Options,
        addresses: Vec<SocketAddr>,
        quic: Option<Arc<QuicEndpoint>>,
    ) -> Result<Self, StartError> {
        let key = AdmissionKey::from_meeting_secret(secret).ok_or(StartError::Derivation)?;
        let address = Peer {
            key: identity.public_key(),
            addresses: addresses.iter().copied().map(Redacted::new).collect(),
        };
        let config = EngineConfig {
            meeting: meeting_tag(secret).ok_or(StartError::Derivation)?,
            name: options.name,
            tracks: vec![MediaTrack {
                audio: true,
                source: Source::Main,
                rendition: b"opus".to_vec(),
            }],
            addresses,
            link_watch: LinkWatchConfig::default(),
        };
        let engine = Engine::start(Admitted::new(endpoint, key), identity, config);
        let call = Call::new(engine, options.encoder, options.jitter, options.playout)
            .map_err(StartError::Codec)?;
        Ok(Self {
            call,
            address,
            quic,
        })
    }

    /// Find out what kind of network this is on, asking STUN servers if `config` says
    /// to. A public address that is learned is added to those given to the meeting and
    /// put in tickets. A participant not on the network learns nothing.
    pub async fn find_network(&mut self, config: &StunConfig) -> Network {
        let (Some(quic), Some(local)) = (&self.quic, self.address.addresses.first()) else {
            return Network::Unknown;
        };
        let network = reach::find(quic, *local.reveal(), config).await;
        if let Some(public) = network.public_address().map(Redacted::new)
            && !self.address.addresses.contains(&public)
        {
            self.address.addresses.push(public);
            let all = self.address.addresses.iter().map(|a| *a.reveal()).collect();
            self.call.engine().set_addresses(all).await;
        }
        network
    }

    /// Where others can find it.
    #[must_use]
    pub const fn address(&self) -> &Peer {
        &self.address
    }

    /// An invitation to the meeting with `secret`, with this participant as the way in.
    #[must_use]
    pub fn ticket(&self, secret: &Secret<[u8; SECRET_BYTES]>) -> Ticket {
        Ticket {
            secret: Secret::new(*secret.expose()),
            entry_points: vec![EntryPoint {
                key: self.address.key,
                addresses: self.address.addresses.clone(),
            }],
        }
    }

    /// The ticket to send back to whoever gave `invitation`, when this participant could
    /// not get in by it: the same meeting, with this participant first among the ways
    /// in. A member who is given it can [`reach`](Self::reach) for this one.
    #[must_use]
    pub fn reply_to(&self, invitation: &Ticket) -> Ticket {
        let mut reply = self.ticket(&invitation.secret);
        reply
            .entry_points
            .extend(invitation.entry_points.iter().cloned());
        reply.entry_points.truncate(MAX_ENTRY_POINTS);
        reply
    }

    /// Dial everyone `ticket` names: for a ticket sent back by someone who cannot get in.
    #[expect(
        clippy::future_not_send,
        reason = "it holds Opus coders, which are not `Sync`; it runs on a current-thread runtime"
    )]
    pub async fn reach(&self, ticket: &Ticket) {
        self.call.engine().reach(entry_points(ticket)).await;
    }

    #[expect(
        clippy::future_not_send,
        reason = "it holds Opus coders, which are not `Sync`; it runs on a current-thread runtime"
    )]
    pub async fn create_meeting(&self) {
        self.call.engine().create_meeting().await;
    }

    #[expect(
        clippy::future_not_send,
        reason = "it holds Opus coders, which are not `Sync`; it runs on a current-thread runtime"
    )]
    pub async fn join(&self, entry_points: Vec<Peer>) {
        self.call.engine().join(entry_points).await;
    }

    #[must_use]
    pub const fn engine(&self) -> &Engine {
        self.call.engine()
    }

    /// Its voice: samples in, samples out.
    pub const fn call(&mut self) -> &mut Call {
        &mut self.call
    }

    /// Stop the node.
    pub async fn stop(self) {
        self.call.stop().await;
    }
}

/// `bound` as others can dial it: an address bound to every interface becomes the
/// address this machine sends from towards the internet. `None` if it has no such route.
fn reachable_form(bound: SocketAddr) -> Option<SocketAddr> {
    if !bound.ip().is_unspecified() {
        return Some(bound);
    }
    // Connecting a UDP socket sends nothing; it only makes the system choose a route.
    let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip: IpAddr = probe.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then(|| SocketAddr::new(ip, bound.port()))
}

/// The members a ticket says to connect to first.
#[must_use]
pub fn entry_points(ticket: &Ticket) -> Vec<Peer> {
    ticket
        .entry_points
        .iter()
        .map(|entry| Peer {
            key: entry.key,
            addresses: entry.addresses.clone(),
        })
        .collect()
}
