//! A headless participant with a made-up voice.
//!
//! A [`Bot`] is a whole participant: a real node, a real encoder and decoders, the same
//! [`Call`] an application with a sound card uses. The only things made up are its voice
//! (a [`Voice`] at the pitch of its speaker number, talking to a [`Rhythm`]) and its ears
//! (it keeps what it was given to play, and can say whose pitch is in it).
//!
//! So a room of bots checks the product end to end by what a listener would notice: who
//! is heard, by whom, and who is not.

use palaver_khef_core::LinkWatchConfig;
use palaver_media::jitter::Config as JitterConfig;
use palaver_media::synthetic::{Rhythm, Voice, hears, pitch_of, strength};
use palaver_media::{CodecError, EncoderConfig, FRAME_SAMPLES, SAMPLE_RATE};
use palaver_net::admission::{AdmissionKey, Admitted};
use palaver_net::identity::{Identity, meeting_tag};
use palaver_net::memory::MemoryNetwork;
use palaver_net::quic::{BindError, QuicEndpoint, QuicOptions};
use palaver_net::transport::{Endpoint, Peer};
use palaver_proto::{
    EntryPoint, MediaTrack, Redacted, SECRET_BYTES, Secret, Signer, Source, Ticket,
};
use palaver_sdk::{Call, Engine, EngineConfig};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

/// The length of one frame of audio.
pub const FRAME: Duration = Duration::from_millis(20);

/// How a bot sounds and when it talks.
#[derive(Debug, Clone, Copy)]
pub struct Script {
    /// Which of the made-up voices it has; see [`pitch_of`].
    pub speaker: u32,
    /// How loud its loudest moments are, 0 to 1.
    pub loudness: f32,
    pub rhythm: Rhythm,
}

impl Script {
    /// A bot that only listens.
    pub fn silent(speaker: u32) -> Self {
        Self {
            speaker,
            loudness: 0.0,
            rhythm: Rhythm {
                talk: Duration::ZERO,
                pause: Duration::from_secs(1),
                offset: Duration::ZERO,
            },
        }
    }

    /// A bot that talks all the time.
    pub fn talking(speaker: u32, loudness: f32) -> Self {
        Self {
            speaker,
            loudness,
            rhythm: Rhythm::ALWAYS,
        }
    }
}

/// Why a bot could not be made.
#[derive(Debug)]
pub enum BotError {
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

/// One headless participant.
pub struct Bot {
    call: Call,
    script: Script,
    voice: Voice,
    /// Where others can find this bot.
    address: Peer,
    /// Frames made so far.
    frames: u64,
    /// What it has been given to play, most recent last, up to [`Bot::EARS`].
    played: Vec<f32>,
    frame: Vec<f32>,
}

impl Bot {
    /// How much of what it played a bot keeps: two seconds.
    pub const EARS: usize = 2 * SAMPLE_RATE as usize;

    /// A bot on an in-memory network, for a meeting with this secret. It does nothing
    /// until told to [`create_meeting`](Self::create_meeting) or [`join`](Self::join).
    /// Must be called inside a tokio runtime.
    pub fn on_memory(
        network: &MemoryNetwork,
        secret: &Secret<[u8; SECRET_BYTES]>,
        name: &str,
        script: Script,
    ) -> Result<Self, BotError> {
        let identity = Identity::generate().map_err(|_| BotError::NoRandomness)?;
        let endpoint = network
            .endpoint(identity.public_key())
            .ok_or(BotError::KeyInUse)?;
        Self::start(endpoint, identity, secret, name, script, Vec::new())
    }

    /// A bot on the network, over QUIC, for a meeting with this secret. It tells the
    /// meeting it can be reached at `advertise`; if that is empty, at the address it is
    /// bound to, with the machine's outward address in place of "every interface".
    /// Must be called inside a tokio runtime.
    pub fn on_quic(
        options: QuicOptions,
        advertise: Vec<SocketAddr>,
        secret: &Secret<[u8; SECRET_BYTES]>,
        name: &str,
        script: Script,
    ) -> Result<Self, BotError> {
        let identity = Identity::generate().map_err(|_| BotError::NoRandomness)?;
        let endpoint = QuicEndpoint::bind(&identity, options).map_err(BotError::Bind)?;
        let advertise = if advertise.is_empty() {
            endpoint
                .local_address()
                .and_then(reachable_form)
                .into_iter()
                .collect()
        } else {
            advertise
        };
        Self::start(endpoint, identity, secret, name, script, advertise)
    }

    fn start<E: Endpoint>(
        endpoint: E,
        identity: Identity,
        secret: &Secret<[u8; SECRET_BYTES]>,
        name: &str,
        script: Script,
        addresses: Vec<SocketAddr>,
    ) -> Result<Self, BotError> {
        let key = AdmissionKey::from_meeting_secret(secret).ok_or(BotError::Derivation)?;
        let address = Peer {
            key: identity.public_key(),
            addresses: addresses.iter().copied().map(Redacted::new).collect(),
            relay_url: None,
        };
        let config = EngineConfig {
            meeting: meeting_tag(secret).ok_or(BotError::Derivation)?,
            name: name.into(),
            tracks: vec![MediaTrack {
                audio: true,
                source: Source::Main,
                rendition: b"opus".to_vec(),
            }],
            addresses,
            relay_url: None,
            link_watch: LinkWatchConfig::default(),
        };
        let engine = Engine::start(Admitted::new(endpoint, key), identity, config);
        let call = Call::new(engine, EncoderConfig::default(), JitterConfig::default())
            .map_err(BotError::Codec)?;
        Ok(Self {
            call,
            script,
            voice: Voice::new(pitch_of(script.speaker), script.loudness),
            address,
            frames: 0,
            played: Vec::new(),
            frame: vec![0.0; FRAME_SAMPLES],
        })
    }

    pub fn address(&self) -> &Peer {
        &self.address
    }

    /// An invitation to the meeting with `secret`, with this bot as the way in.
    pub fn ticket(&self, secret: &Secret<[u8; SECRET_BYTES]>) -> Ticket {
        Ticket {
            secret: Secret::new(*secret.expose()),
            relay_url: None,
            entry_points: vec![EntryPoint {
                key: self.address.key,
                addresses: self.address.addresses.clone(),
            }],
        }
    }

    pub fn engine(&self) -> &Engine {
        self.call.engine()
    }

    pub fn script(&self) -> Script {
        self.script
    }

    pub async fn create_meeting(&self) {
        self.call.engine().create_meeting().await;
    }

    pub async fn join(&self, entry_points: Vec<Peer>) {
        self.call.engine().join(entry_points).await;
    }

    /// One 20 ms step: say the next frame of its script, and play what it is given.
    pub fn tick(&mut self) -> Result<(), CodecError> {
        let elapsed = FRAME.saturating_mul(u32::try_from(self.frames).unwrap_or(u32::MAX));
        self.frames = self.frames.saturating_add(1);
        let talking = self.script.rhythm.talking(elapsed);
        self.voice.frame(talking, &mut self.frame);
        self.call.speak(&self.frame)?;
        self.call.listen(&mut self.frame)?;
        self.played.extend_from_slice(&self.frame);
        let extra = self.played.len().saturating_sub(Self::EARS);
        self.played.drain(..extra);
        Ok(())
    }

    /// What this bot played over the last second; empty until it has played that much.
    fn last_second(&self) -> &[f32] {
        let second = SAMPLE_RATE as usize;
        self.played
            .len()
            .checked_sub(second)
            .and_then(|from| self.played.get(from..))
            .unwrap_or_default()
    }

    /// Whether `speaker`'s voice is in what this bot played over the last second.
    pub fn hears(&self, speaker: u32) -> bool {
        hears(self.last_second(), pitch_of(speaker))
    }

    /// How strongly `speaker`'s pitch is in what this bot played over the last second.
    pub fn strength_of(&self, speaker: u32) -> f32 {
        strength(self.last_second(), pitch_of(speaker))
    }

    /// Every speaker among `speakers` this bot hears, in order.
    pub fn hears_among(&self, speakers: impl IntoIterator<Item = u32>) -> Vec<u32> {
        speakers.into_iter().filter(|s| self.hears(*s)).collect()
    }

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
pub fn entry_points(ticket: &Ticket) -> Vec<Peer> {
    ticket
        .entry_points
        .iter()
        .map(|entry| Peer {
            key: entry.key,
            addresses: entry.addresses.clone(),
            relay_url: None,
        })
        .collect()
}
