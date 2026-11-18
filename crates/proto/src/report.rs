//! A node's report: what it is and what it sees. Signed by the node, sent once a second and on
//! change, flooded on the control namespace (design section 5).

use crate::fields::{DecodeError, Writer, fields, push};
use crate::plan::PlanStamp;
use crate::sign::{Kind, PublicKey, Sealed, Signer, Verifier, seal};
use crate::{MeetingTag, ParticipantId, Redacted};
use palaver_moq_wire::Value;

/// Longest display name, in bytes.
pub const MAX_NAME_BYTES: usize = 128;

// Field numbers. Even numbers carry integers, odd ones bytes. Never reuse a number.
const KEY: u64 = 1;
const SEQUENCE: u64 = 2;
const NAME: u64 = 3;
const APPLIED_PLAN: u64 = 4;
const LINK: u64 = 5;
const UNREACHABLE: u64 = 7;

const LINK_PEER: u64 = 1;
const LINK_PATH: u64 = 2;
const LINK_RTT_MS: u64 = 4;

/// How packets travel on a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Peer to peer over UDP.
    Direct,
    /// Through an iroh-relay; poor for media.
    IrohRelay,
}

/// One connection the node holds to another node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub peer: ParticipantId,
    pub path: PathKind,
    /// Round-trip time in milliseconds.
    pub rtt_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The node's per-meeting public key. Its participant id is derived from it.
    pub key: PublicKey,
    /// Counts up with every report from this node; equals the MoQ group id it is sent in.
    pub sequence: u64,
    /// The name the user chose.
    pub name: Redacted<String>,
    /// The last plan this node applied, if any.
    pub applied_plan: Option<PlanStamp>,
    pub links: Vec<Link>,
    /// Nodes this node tried to connect to and could not.
    pub unreachable: Vec<ParticipantId>,
}

impl Link {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.bytes(LINK_PEER, self.peer.as_bytes());
        w.int(LINK_PATH, u64::from(self.path == PathKind::IrohRelay));
        w.int(LINK_RTT_MS, u64::from(self.rtt_ms));
        w.finish()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (mut peer, mut path, mut rtt_ms) = (None, PathKind::Direct, 0);
        for pair in fields(bytes) {
            let pair = pair?;
            match (pair.ty, pair.value) {
                (LINK_PEER, Value::Bytes(b)) => peer = ParticipantId::new(b),
                (LINK_PATH, Value::Int(0)) => path = PathKind::Direct,
                (LINK_PATH, Value::Int(1)) => path = PathKind::IrohRelay,
                (LINK_PATH, _) => return Err(DecodeError::Invalid("link path kind")),
                (LINK_RTT_MS, Value::Int(v)) => rtt_ms = u32::try_from(v).unwrap_or(u32::MAX),
                _ => {}
            }
        }
        Ok(Self {
            peer: peer.ok_or(DecodeError::Missing("link peer"))?,
            path,
            rtt_ms,
        })
    }
}

impl Report {
    /// The payload, unsigned.
    pub fn encode(&self) -> Vec<u8> {
        let links: Vec<Vec<u8>> = self.links.iter().map(Link::encode).collect();
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
        w.finish()
    }

    /// Decode a payload. Fields with unknown numbers are skipped.
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let (mut key, mut sequence, mut name, mut applied_plan) = (None, None, None, None);
        let (mut links, mut unreachable) = (vec![], vec![]);
        for pair in fields(payload) {
            let pair = pair?;
            match (pair.ty, pair.value) {
                (KEY, Value::Bytes(b)) => {
                    key = Some(PublicKey::from_slice(b).ok_or(DecodeError::Invalid("public key"))?)
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
        })
    }

    /// The signed object to publish on this node's report track, in group `self.sequence`.
    /// `signer` must hold the key in `self.key`.
    pub fn seal(&self, meeting: &MeetingTag, signer: &dyn Signer) -> Vec<u8> {
        seal(Kind::Report, meeting, self.sequence, &self.encode(), signer)
    }

    /// Verify and decode an object received on `participant`'s report track in group
    /// `group_id`. Checks, in order: the envelope and payload are well formed; the sequence
    /// number is the group id; the key belongs to the participant the track is named for; the
    /// signature is by that key over these bytes for this meeting and sequence number.
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
                    path: PathKind::Direct,
                    rtt_ms: 12,
                },
                Link {
                    peer: id_of(3),
                    path: PathKind::IrohRelay,
                    rtt_ms: 180,
                },
            ],
            unreachable: vec![id_of(4)],
        }
    }

    #[test]
    fn a_report_round_trips() {
        assert_eq!(Report::decode(&sample().encode()), Ok(sample()));
        let minimal = Report {
            applied_plan: None,
            links: vec![],
            unreachable: vec![],
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
                path: PathKind::Direct,
                rtt_ms: 12
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
    fn the_name_is_redacted_in_debug_output() {
        assert!(!format!("{:?}", sample()).contains("Alice"));
    }
}
