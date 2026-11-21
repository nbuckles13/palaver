//! A node's report: what it is and what it sees. Signed by the node, sent once a second and on
//! change, flooded on the control namespace (design section 5).

use crate::fields::{DecodeError, Writer, fields, push};
use crate::plan::PlanStamp;
use crate::sign::{Kind, PublicKey, Sealed, Signer, Verifier, seal};
use crate::{MediaTrack, MeetingTag, ParticipantId, Redacted};
use palaver_moq_wire::Value;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// Longest display name, in bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// Most addresses a node advertises. A reader keeps the first this many and ignores the rest.
pub const MAX_ADDRESSES: usize = 8;
/// Most media tracks a node offers. A reader keeps the first this many and ignores the rest.
pub const MAX_TRACKS: usize = 32;

// Field numbers. Even numbers carry integers, odd ones bytes. Never reuse a number:
// 11 was a relay's URL, and 2 inside a link was the kind of path, when a relay server was
// part of the design.
const KEY: u64 = 1;
const SEQUENCE: u64 = 2;
const NAME: u64 = 3;
const APPLIED_PLAN: u64 = 4;
const LINK: u64 = 5;
const UNREACHABLE: u64 = 7;
const ADDRESS: u64 = 9;
const TRACK: u64 = 13;

const LINK_PEER: u64 = 1;
const LINK_OBSERVED: u64 = 3;
const LINK_RTT_MS: u64 = 4;

/// One connection the node holds to another node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub peer: ParticipantId,
    /// Round-trip time. Sent in whole milliseconds.
    pub rtt: Duration,
    /// The address this node sees the peer's packets come from. Behind a NAT that is an
    /// address the peer cannot see for itself, and the one others must send to to reach
    /// it. Redacted in debug output.
    pub observed: Option<Redacted<SocketAddr>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The node's per-meeting public key. Its participant id is derived from it.
    pub key: PublicKey,
    /// Counts up with every report from this node; equals the `MoQ` group id it is sent in.
    pub sequence: u64,
    /// The name the user chose.
    pub name: Redacted<String>,
    /// The last plan this node applied, if any.
    pub applied_plan: Option<PlanStamp>,
    pub links: Vec<Link>,
    /// Nodes this node tried to connect to and could not.
    pub unreachable: Vec<ParticipantId>,
    /// Where the node can be reached: one address per usable interface, at most
    /// [`MAX_ADDRESSES`]. They are hints; the node is identified by its key. A peer tries all
    /// of them. Redacted in debug output.
    pub addresses: Vec<Redacted<SocketAddr>>,
    /// The iroh-relay through which the node can be reached by key, if it uses one.
    /// Every media track the node can send, including those it is not sending now (the
    /// simulcast renditions, a screen share not yet started), at most [`MAX_TRACKS`]. The
    /// conductor gives each an alias in the plan; a track not listed here cannot be sent.
    pub tracks: Vec<MediaTrack>,
}

/// An address on the wire: the IP's 4 or 16 bytes, then the port, big-endian.
pub fn encode_address(address: &SocketAddr) -> Vec<u8> {
    let mut out = match address.ip() {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    };
    out.extend_from_slice(&address.port().to_be_bytes());
    out
}

pub fn decode_address(bytes: &[u8]) -> Option<SocketAddr> {
    let (ip, port) = bytes.split_at_checked(bytes.len().checked_sub(2)?)?;
    let port = u16::from_be_bytes(port.try_into().ok()?);
    let ip = match ip.len() {
        4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(ip).ok()?)),
        16 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(ip).ok()?)),
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Whether a node should advertise `address` to the meeting.
///
/// Not the unspecified address, not multicast or broadcast, not port 0, and not link-local (it
/// only means something on the sender's own interface). Loopback only when `allow_loopback` is
/// set, which is for tests and for running several nodes on a machine with no network.
#[must_use]
pub const fn advertisable(address: &SocketAddr, allow_loopback: bool) -> bool {
    if address.port() == 0 {
        return false;
    }
    match address.ip() {
        IpAddr::V4(ip) => {
            !(ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() || ip.is_link_local())
                && (allow_loopback || !ip.is_loopback())
        }
        IpAddr::V6(ip) => {
            let link_local = ip.segments()[0] & 0xffc0 == 0xfe80;
            !(ip.is_unspecified() || ip.is_multicast() || link_local)
                && (allow_loopback || !ip.is_loopback())
        }
    }
}

impl Link {
    fn encode(&self) -> Vec<u8> {
        let observed = self.observed.as_ref().map(|a| encode_address(a.reveal()));
        let mut w = Writer::default();
        w.bytes(LINK_PEER, self.peer.as_bytes());
        w.int(
            LINK_RTT_MS,
            u64::try_from(self.rtt.as_millis()).unwrap_or(u64::MAX),
        );
        if let Some(observed) = &observed {
            w.bytes(LINK_OBSERVED, observed);
        }
        w.finish()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (mut peer, mut rtt) = (None, Duration::ZERO);
        let mut observed = None;
        for pair in fields(bytes) {
            let pair = pair?;
            match (pair.ty, pair.value) {
                (LINK_PEER, Value::Bytes(b)) => peer = ParticipantId::new(b),
                (LINK_RTT_MS, Value::Int(v)) => rtt = Duration::from_millis(v),
                (LINK_OBSERVED, Value::Bytes(b)) => {
                    let address = decode_address(b).ok_or(DecodeError::Invalid("address"))?;
                    observed = Some(Redacted::new(address));
                }
                _ => {}
            }
        }
        Ok(Self {
            peer: peer.ok_or(DecodeError::Missing("link peer"))?,
            rtt,
            observed,
        })
    }
}

impl Report {
    /// The payload, unsigned.
    pub fn encode(&self) -> Vec<u8> {
        let links: Vec<Vec<u8>> = self.links.iter().map(Link::encode).collect();
        let addresses: Vec<Vec<u8>> = self
            .addresses
            .iter()
            .take(MAX_ADDRESSES)
            .map(|a| encode_address(a.reveal()))
            .collect();
        let tracks: Vec<Vec<u8>> = self
            .tracks
            .iter()
            .take(MAX_TRACKS)
            .map(|track| track.encode(None))
            .collect();
        let mut w = Writer::default();
        w.bytes(KEY, &self.key.0);
        w.int(SEQUENCE, self.sequence);
        w.bytes(NAME, self.name.reveal().as_bytes());
        if let Some(stamp) = self.applied_plan {
            w.int(APPLIED_PLAN, stamp.group_id());
        }
        for link in &links {
            w.bytes(LINK, link);
        }
        for peer in &self.unreachable {
            w.bytes(UNREACHABLE, peer.as_bytes());
        }
        for address in &addresses {
            w.bytes(ADDRESS, address);
        }
        for track in &tracks {
            w.bytes(TRACK, track);
        }
        w.finish()
    }

    /// Decode a payload. Fields with unknown numbers are skipped.
    ///
    /// # Errors
    ///
    /// Fails if the payload is not well-formed fields, the key, sequence or name is missing,
    /// a field has a value that is not allowed, or there are too many links or unreachable
    /// peers.
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let (mut key, mut sequence, mut name, mut applied_plan) = (None, None, None, None);
        let (mut links, mut unreachable, mut addresses) = (vec![], vec![], vec![]);
        let mut tracks = vec![];
        for pair in fields(payload) {
            let pair = pair?;
            match (pair.ty, pair.value) {
                (KEY, Value::Bytes(b)) => {
                    key = Some(PublicKey::from_slice(b).ok_or(DecodeError::Invalid("public key"))?);
                }
                (SEQUENCE, Value::Int(v)) => sequence = Some(v),
                (NAME, Value::Bytes(b)) => {
                    if b.len() > MAX_NAME_BYTES {
                        return Err(DecodeError::Invalid("name too long"));
                    }
                    let text = core::str::from_utf8(b)
                        .map_err(|_| DecodeError::Invalid("name is not UTF-8"))?;
                    name = Some(Redacted::new(text.to_owned()));
                }
                (APPLIED_PLAN, Value::Int(v)) => applied_plan = Some(PlanStamp::from_group_id(v)),
                (LINK, Value::Bytes(b)) => push(&mut links, Link::decode(b)?, "too many links")?,
                (UNREACHABLE, Value::Bytes(b)) => {
                    let peer =
                        ParticipantId::new(b).ok_or(DecodeError::Invalid("unreachable peer id"))?;
                    push(&mut unreachable, peer, "too many unreachable peers")?;
                }
                (ADDRESS, Value::Bytes(b)) => {
                    let address = decode_address(b).ok_or(DecodeError::Invalid("address"))?;
                    if addresses.len() < MAX_ADDRESSES {
                        addresses.push(Redacted::new(address));
                    }
                }
                (TRACK, Value::Bytes(b)) => {
                    let (track, _) = MediaTrack::decode(b)?;
                    if tracks.len() < MAX_TRACKS && !tracks.contains(&track) {
                        tracks.push(track);
                    }
                }
                _ => {}
            }
        }
        Ok(Self {
            key: key.ok_or(DecodeError::Missing("public key"))?,
            sequence: sequence.ok_or(DecodeError::Missing("sequence"))?,
            name: name.ok_or(DecodeError::Missing("name"))?,
            applied_plan,
            links,
            unreachable,
            addresses,
            tracks,
        })
    }

    /// The signed object to publish on this node's report track, in group `self.sequence`.
    /// `signer` must hold the key in `self.key`.
    pub fn seal(&self, meeting: &MeetingTag, signer: &dyn Signer) -> Vec<u8> {
        seal(Kind::Report, meeting, self.sequence, &self.encode(), signer)
    }

    /// Verify and decode an object received on `participant`'s report track in group
    /// `group_id`.
    ///
    /// # Errors
    ///
    /// Fails on the first of these checks that does not hold, in order: the envelope and
    /// payload are well formed; the sequence number is the group id; the key belongs to the
    /// participant the track is named for; the signature is by that key over these bytes for
    /// this meeting and sequence number.
    pub fn open(
        meeting: &MeetingTag,
        participant: &ParticipantId,
        group_id: u64,
        envelope: &[u8],
        verifier: &dyn Verifier,
    ) -> Result<Self, DecodeError> {
        let sealed = Sealed::split(envelope)?;
        let report = Self::decode(sealed.payload)?;
        if report.sequence != group_id {
            return Err(DecodeError::Invalid("sequence does not match the group id"));
        }
        if &verifier.participant_id(&report.key) != participant {
            return Err(DecodeError::BadSignature(
                "key does not belong to this participant",
            ));
        }
        sealed.verify(Kind::Report, meeting, group_id, &report.key, verifier)?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::fake::{FakeIdentity, FakeVerifier};

    fn meeting() -> MeetingTag {
        MeetingTag::new(b"m-7f3a").unwrap()
    }

    fn id_of(seed: u8) -> ParticipantId {
        FakeVerifier.participant_id(&FakeIdentity(seed).public_key())
    }

    fn sample() -> Report {
        Report {
            key: FakeIdentity(1).public_key(),
            sequence: 42,
            name: Redacted::new(String::from("Alice")),
            applied_plan: Some(PlanStamp {
                term: 2,
                generation: 17,
            }),
            links: vec![
                Link {
                    peer: id_of(2),
                    rtt: Duration::from_millis(12),
                    observed: Some(addr("203.0.113.9:50123")),
                },
                Link {
                    peer: id_of(3),
                    rtt: Duration::from_millis(180),
                    observed: None,
                },
            ],
            unreachable: vec![id_of(4)],
            addresses: vec![addr("192.168.1.5:4433"), addr("[2001:db8::7]:4433")],
            tracks: vec![
                MediaTrack {
                    audio: true,
                    source: crate::Source::Main,
                    rendition: b"opus".to_vec(),
                },
                MediaTrack {
                    audio: false,
                    source: crate::Source::Screen,
                    rendition: b"1080p".to_vec(),
                },
            ],
        }
    }

    fn addr(text: &str) -> Redacted<SocketAddr> {
        Redacted::new(text.parse().unwrap())
    }

    #[test]
    fn a_report_round_trips() {
        assert_eq!(Report::decode(&sample().encode()), Ok(sample()));
        let minimal = Report {
            applied_plan: None,
            links: vec![],
            unreachable: vec![],
            addresses: vec![],
            tracks: vec![],
            ..sample()
        };
        assert_eq!(Report::decode(&minimal.encode()), Ok(minimal));
    }

    #[test]
    fn fields_from_a_newer_version_are_skipped() {
        // Unknown top-level fields 100 (integer) and 101 (bytes), and an unknown field inside a
        // link, as a later version might add.
        let mut link = Writer::default();
        let peer = id_of(2);
        link.bytes(LINK_PEER, peer.as_bytes());
        link.int(LINK_RTT_MS, 12);
        link.bytes(77, b"future");
        let link = link.finish();
        let key = FakeIdentity(1).public_key();
        let mut w = Writer::default();
        w.bytes(KEY, &key.0);
        w.int(SEQUENCE, 42);
        w.bytes(NAME, b"Alice");
        w.bytes(LINK, &link);
        w.int(100, 5);
        w.bytes(101, b"future");
        let decoded = Report::decode(&w.finish()).unwrap();
        assert_eq!(
            decoded.links,
            [Link {
                peer: id_of(2),
                rtt: Duration::from_millis(12),
                observed: None,
            }]
        );
        assert_eq!(decoded.sequence, 42);
    }

    #[test]
    fn missing_and_invalid_fields_are_reported() {
        let key = FakeIdentity(1).public_key();
        let build = |with_key: bool, with_seq: bool, name: &'static [u8]| {
            let mut w = Writer::default();
            if with_key {
                w.bytes(KEY, &key.0);
            }
            if with_seq {
                w.int(SEQUENCE, 1);
            }
            w.bytes(NAME, name);
            Report::decode(&w.finish())
        };
        assert_eq!(
            build(false, true, b"A"),
            Err(DecodeError::Missing("public key"))
        );
        assert_eq!(
            build(true, false, b"A"),
            Err(DecodeError::Missing("sequence"))
        );
        assert_eq!(
            build(true, true, &[0xff, 0xfe]),
            Err(DecodeError::Invalid("name is not UTF-8"))
        );
        assert_eq!(
            build(true, true, &[b'a'; 129]),
            Err(DecodeError::Invalid("name too long"))
        );
        let mut w = Writer::default();
        w.bytes(KEY, &[1, 2, 3]);
        assert_eq!(
            Report::decode(&w.finish()),
            Err(DecodeError::Invalid("public key"))
        );
        assert_eq!(Report::decode(&[1, 9]), Err(DecodeError::Malformed));
    }

    #[test]
    fn a_sealed_report_opens_for_its_own_participant_group_and_meeting() {
        let envelope = sample().seal(&meeting(), &FakeIdentity(1));
        assert_eq!(
            Report::open(&meeting(), &id_of(1), 42, &envelope, &FakeVerifier),
            Ok(sample())
        );
    }

    #[test]
    fn tampering_replay_and_impersonation_are_refused() {
        let envelope = sample().seal(&meeting(), &FakeIdentity(1));
        let open = |m: &MeetingTag, who: &ParticipantId, group: u64, bytes: &[u8]| {
            Report::open(m, who, group, bytes, &FakeVerifier)
        };
        // One payload byte changed (inside the name).
        let mut altered = envelope.clone();
        let at = altered.windows(5).position(|w| w == b"Alice").unwrap();
        altered[at] = b'M';
        assert!(matches!(
            open(&meeting(), &id_of(1), 42, &altered),
            Err(DecodeError::BadSignature(_))
        ));
        // One signature byte changed.
        let mut altered = envelope.clone();
        *altered.last_mut().unwrap() ^= 1;
        assert!(matches!(
            open(&meeting(), &id_of(1), 42, &altered),
            Err(DecodeError::BadSignature(_))
        ));
        // Replayed as a later group, into another meeting, or on another participant's track.
        assert!(matches!(
            open(&meeting(), &id_of(1), 43, &envelope),
            Err(DecodeError::Invalid(_))
        ));
        let other = MeetingTag::new(b"m-0000").unwrap();
        assert!(matches!(
            open(&other, &id_of(1), 42, &envelope),
            Err(DecodeError::BadSignature(_))
        ));
        assert!(matches!(
            open(&meeting(), &id_of(2), 42, &envelope),
            Err(DecodeError::BadSignature(_))
        ));
        // Signed by someone else while claiming this key.
        let forged = sample().seal(&meeting(), &FakeIdentity(9));
        assert!(matches!(
            open(&meeting(), &id_of(1), 42, &forged),
            Err(DecodeError::BadSignature(_))
        ));
        // Truncated.
        assert_eq!(
            open(&meeting(), &id_of(1), 42, &envelope[..envelope.len() - 1]),
            Err(DecodeError::Malformed)
        );
    }

    #[test]
    fn the_name_and_addresses_are_redacted_in_debug_output() {
        let out = format!("{:?}", sample());
        assert!(
            !out.contains("Alice") && !out.contains("192.168") && !out.contains("2001"),
            "{out}"
        );
    }

    #[test]
    fn addresses_have_the_expected_bytes_and_bad_ones_are_refused() {
        assert_eq!(
            encode_address(addr("192.168.1.5:4433").reveal()),
            [192, 168, 1, 5, 0x11, 0x51]
        );
        let v6 = encode_address(addr("[2001:db8::7]:4433").reveal());
        assert_eq!(v6.len(), 18);
        assert_eq!(
            decode_address(&v6),
            Some(*addr("[2001:db8::7]:4433").reveal())
        );
        for bad in [&[][..], &[1], &[1, 2, 3, 4, 5], &[0; 7], &[0; 19]] {
            assert_eq!(decode_address(bad), None, "{bad:?}");
        }
        let key = FakeIdentity(1).public_key();
        let mut w = Writer::default();
        w.bytes(KEY, &key.0);
        w.int(SEQUENCE, 1);
        w.bytes(NAME, b"A");
        w.bytes(ADDRESS, &[1, 2, 3]);
        assert_eq!(
            Report::decode(&w.finish()),
            Err(DecodeError::Invalid("address"))
        );
    }

    #[test]
    fn only_the_first_eight_addresses_are_sent_and_kept() {
        let many: Vec<Redacted<SocketAddr>> =
            (0..12).map(|i| addr(&format!("10.0.0.{i}:4433"))).collect();
        let sent = Report {
            addresses: many.clone(),
            ..sample()
        };
        assert_eq!(Report::decode(&sent.encode()).unwrap().addresses, many[..8]);
        // A sender that ignores the cap: the reader keeps the first eight and carries on.
        let key = FakeIdentity(1).public_key();
        let encoded: Vec<Vec<u8>> = many.iter().map(|a| encode_address(a.reveal())).collect();
        let mut w = Writer::default();
        w.bytes(KEY, &key.0);
        w.int(SEQUENCE, 1);
        w.bytes(NAME, b"A");
        for a in &encoded {
            w.bytes(ADDRESS, a);
        }
        assert_eq!(Report::decode(&w.finish()).unwrap().addresses, many[..8]);
    }

    #[test]
    fn which_addresses_a_node_advertises() {
        let yes = [
            "192.168.1.5:4433",
            "10.0.0.7:1",
            "203.0.113.9:4433",
            "[2001:db8::7]:4433",
            "[fd00::1]:4433",
        ];
        let no = [
            "0.0.0.0:4433",
            "192.168.1.5:0",
            "169.254.3.4:4433",
            "224.0.0.1:4433",
            "255.255.255.255:4433",
            "[::]:4433",
            "[fe80::1]:4433",
            "[ff02::1]:4433",
        ];
        for a in yes {
            assert!(advertisable(&a.parse().unwrap(), false), "{a}");
        }
        for a in no {
            assert!(!advertisable(&a.parse().unwrap(), true), "{a}");
        }
        for a in ["127.0.0.1:4433", "[::1]:4433"] {
            assert!(!advertisable(&a.parse().unwrap(), false), "{a}");
            assert!(
                advertisable(&a.parse().unwrap(), true),
                "{a} with the test option"
            );
        }
    }

    #[test]
    fn at_most_thirty_two_tracks_are_kept_and_a_repeat_is_dropped() {
        let track = |n: usize| MediaTrack {
            audio: false,
            source: crate::Source::Main,
            rendition: format!("r{n}").into_bytes(),
        };
        let mut report = sample();
        report.tracks = (0..40).map(track).collect();
        let read = Report::decode(&report.encode()).unwrap();
        assert_eq!(read.tracks, (0..MAX_TRACKS).map(track).collect::<Vec<_>>());
        report.tracks = vec![track(1), track(2), track(1)];
        let read = Report::decode(&report.encode()).unwrap();
        assert_eq!(read.tracks, [track(1), track(2)]);
    }
}
