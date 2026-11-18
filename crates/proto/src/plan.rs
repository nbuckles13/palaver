//! The conductor's plan: who is in the meeting and how the khefs are wired. Signed by the
//! conductor and flooded on the control namespace; everyone holds the whole plan (design
//! section 5). Phase 1 carries what audio needs; routing tables, priorities and budgets are
//! added as fields later.

use crate::fields::{DecodeError, Writer, fields, push};
use crate::sign::{Kind, PublicKey, Sealed, Signer, Verifier, seal};
use crate::{MeetingTag, ParticipantId, Redacted};
use palaver_moq_wire::Value;

// Field numbers. Even numbers carry integers, odd ones bytes. Never reuse a number.
const CONDUCTOR: u64 = 1;
const STAMP: u64 = 2;
const MEMBER: u64 = 3;
const AUDIO_TOP_N: u64 = 4;
const PEERING: u64 = 5;
const AUDIO_LINK: u64 = 7;
const SUCCESSOR: u64 = 9;

const MEMBER_KEY: u64 = 1;
const MEMBER_NAME: u64 = 3;
const EDGE_A: u64 = 1;
const EDGE_B: u64 = 3;

/// Identifies one version of the plan. A new conductor starts a new term; each plan it issues
/// has the next generation. Plans are ordered by term, then generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlanStamp {
    pub term: u32,
    pub generation: u32,
}

impl PlanStamp {
    /// The MoQ group id a plan with this stamp is sent in: term in the high half, so group
    /// ids keep rising when the conductor, and so the track's publisher, changes.
    pub fn group_id(self) -> u64 {
        u64::from(self.term) << 32 | u64::from(self.generation)
    }

    pub fn from_group_id(group_id: u64) -> Self {
        Self {
            term: (group_id >> 32) as u32,
            generation: group_id as u32,
        }
    }
}

/// One participant in the roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub key: PublicKey,
    pub name: Redacted<String>,
}

/// A pair of nodes: a planned connection, or a link of the audio overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge(pub ParticipantId, pub ParticipantId);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub stamp: PlanStamp,
    /// The node that issued this plan. Must be in the roster.
    pub conductor: ParticipantId,
    pub roster: Vec<Member>,
    /// Which khefs keep a connection to which.
    pub peerings: Vec<Edge>,
    /// The loop-free tree audio is forwarded along. A subset of the peerings.
    pub audio_overlay: Vec<Edge>,
    /// The N of top-N audio selection.
    pub audio_top_n: u64,
    /// Who takes over as conductor, in order.
    pub successors: Vec<ParticipantId>,
}

impl Member {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.bytes(MEMBER_KEY, &self.key.0);
        w.bytes(MEMBER_NAME, self.name.reveal().as_bytes());
        w.finish()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (mut key, mut name) = (None, None);
        for pair in fields(bytes) {
            let pair = pair?;
            match (pair.ty, pair.value) {
                (MEMBER_KEY, Value::Bytes(b)) => key = PublicKey::from_slice(b),
                (MEMBER_NAME, Value::Bytes(b)) => {
                    if b.len() > crate::report::MAX_NAME_BYTES {
                        return Err(DecodeError::Invalid("name too long"));
                    }
                    let text = core::str::from_utf8(b)
                        .map_err(|_| DecodeError::Invalid("name is not UTF-8"))?;
                    name = Some(Redacted::new(text.to_owned()));
                }
                _ => {}
            }
        }
        Ok(Self {
            key: key.ok_or(DecodeError::Missing("member key"))?,
            name: name.ok_or(DecodeError::Missing("member name"))?,
        })
    }
}

impl Edge {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.bytes(EDGE_A, self.0.as_bytes());
        w.bytes(EDGE_B, self.1.as_bytes());
        w.finish()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (mut a, mut b) = (None, None);
        for pair in fields(bytes) {
            let pair = pair?;
            match (pair.ty, pair.value) {
                (EDGE_A, Value::Bytes(v)) => a = ParticipantId::new(v),
                (EDGE_B, Value::Bytes(v)) => b = ParticipantId::new(v),
                _ => {}
            }
        }
        Ok(Self(
            a.ok_or(DecodeError::Missing("edge end"))?,
            b.ok_or(DecodeError::Missing("edge end"))?,
        ))
    }
}

impl Plan {
    /// The payload, unsigned.
    pub fn encode(&self) -> Vec<u8> {
        let members: Vec<Vec<u8>> = self.roster.iter().map(Member::encode).collect();
        let peerings: Vec<Vec<u8>> = self.peerings.iter().map(Edge::encode).collect();
        let overlay: Vec<Vec<u8>> = self.audio_overlay.iter().map(Edge::encode).collect();
        let mut w = Writer::default();
        w.bytes(CONDUCTOR, self.conductor.as_bytes());
        w.int(STAMP, self.stamp.group_id());
        w.int(AUDIO_TOP_N, self.audio_top_n);
        for m in &members {
            w.bytes(MEMBER, m);
        }
        for e in &peerings {
            w.bytes(PEERING, e);
        }
        for e in &overlay {
            w.bytes(AUDIO_LINK, e);
        }
        for s in &self.successors {
            w.bytes(SUCCESSOR, s.as_bytes());
        }
        w.finish()
    }

    /// Decode a payload. Fields with unknown numbers are skipped.
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let (mut conductor, mut stamp, mut audio_top_n) = (None, None, None);
        let (mut roster, mut peerings, mut audio_overlay, mut successors) =
            (vec![], vec![], vec![], vec![]);
        for pair in fields(payload) {
            let pair = pair?;
            match (pair.ty, pair.value) {
                (CONDUCTOR, Value::Bytes(b)) => conductor = ParticipantId::new(b),
                (STAMP, Value::Int(v)) => stamp = Some(PlanStamp::from_group_id(v)),
                (AUDIO_TOP_N, Value::Int(v)) => audio_top_n = Some(v),
                (MEMBER, Value::Bytes(b)) => {
                    push(&mut roster, Member::decode(b)?, "too many members")?
                }
                (PEERING, Value::Bytes(b)) => {
                    push(&mut peerings, Edge::decode(b)?, "too many peerings")?
                }
                (AUDIO_LINK, Value::Bytes(b)) => {
                    push(&mut audio_overlay, Edge::decode(b)?, "too many audio links")?
                }
                (SUCCESSOR, Value::Bytes(b)) => {
                    let id = ParticipantId::new(b).ok_or(DecodeError::Invalid("successor id"))?;
                    push(&mut successors, id, "too many successors")?;
                }
                _ => {}
            }
        }
        Ok(Self {
            stamp: stamp.ok_or(DecodeError::Missing("stamp"))?,
            conductor: conductor.ok_or(DecodeError::Missing("conductor"))?,
            roster,
            peerings,
            audio_overlay,
            audio_top_n: audio_top_n.ok_or(DecodeError::Missing("audio top-N"))?,
            successors,
        })
    }

    /// The conductor's key, found in the roster by the id each key derives to.
    fn conductor_key(&self, verifier: &dyn Verifier) -> Option<PublicKey> {
        self.roster
            .iter()
            .map(|m| m.key)
            .find(|key| verifier.participant_id(key) == self.conductor)
    }

    /// The signed object to publish on the plan track, in group `self.stamp.group_id()`.
    /// `signer` must be the conductor named in the plan.
    pub fn seal(&self, meeting: &MeetingTag, signer: &dyn Signer) -> Vec<u8> {
        seal(
            Kind::Plan,
            meeting,
            self.stamp.group_id(),
            &self.encode(),
            signer,
        )
    }

    /// Verify and decode an object received on the plan track in group `group_id`. Checks, in
    /// order: the envelope and payload are well formed; the stamp is the group id; the
    /// conductor is in the roster; the signature is by the conductor's key.
    ///
    /// This shows the plan is what its conductor issued. Whether that node is entitled to be
    /// conductor is a separate question; see [`Plan::may_replace`].
    pub fn open(
        meeting: &MeetingTag,
        group_id: u64,
        envelope: &[u8],
        verifier: &dyn Verifier,
    ) -> Result<Self, DecodeError> {
        let sealed = Sealed::split(envelope)?;
        let plan = Self::decode(sealed.payload)?;
        if plan.stamp.group_id() != group_id {
            return Err(DecodeError::Invalid("stamp does not match the group id"));
        }
        let key = plan
            .conductor_key(verifier)
            .ok_or(DecodeError::Invalid("conductor is not in the roster"))?;
        sealed.verify(Kind::Plan, meeting, group_id, &key, verifier)?;
        Ok(plan)
    }

    /// Whether a node holding `self` should adopt `next`, both already verified by
    /// [`Plan::open`]:
    /// - same term: only from the same conductor, with a later generation;
    /// - the next term: only if its conductor is on this plan's successor list (a planned
    ///   hand-over or a takeover after a crash);
    /// - anything else, including an older or a skipped term: no.
    ///
    /// A node with no plan yet (a joiner) has nothing to check against and adopts the first
    /// verified plan it receives.
    pub fn may_replace(&self, next: &Plan) -> bool {
        if next.stamp.term == self.stamp.term {
            next.conductor == self.conductor && next.stamp.generation > self.stamp.generation
        } else if self.stamp.term.checked_add(1) == Some(next.stamp.term) {
            self.successors.contains(&next.conductor)
        } else {
            false
        }
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

    fn member(seed: u8, name: &str) -> Member {
        Member {
            key: FakeIdentity(seed).public_key(),
            name: Redacted::new(name.to_owned()),
        }
    }

    /// Conductor 1; members 1 to 4; a line 1-2-3-4 as peerings and audio overlay.
    fn sample() -> Plan {
        let line = vec![
            Edge(id_of(1), id_of(2)),
            Edge(id_of(2), id_of(3)),
            Edge(id_of(3), id_of(4)),
        ];
        Plan {
            stamp: PlanStamp {
                term: 3,
                generation: 9,
            },
            conductor: id_of(1),
            roster: vec![
                member(1, "Alice"),
                member(2, "Bob"),
                member(3, "Carol"),
                member(4, "Dan"),
            ],
            peerings: [line.clone(), vec![Edge(id_of(1), id_of(3))]].concat(),
            audio_overlay: line,
            audio_top_n: 3,
            successors: vec![id_of(2), id_of(3)],
        }
    }

    #[test]
    fn a_plan_round_trips() {
        assert_eq!(Plan::decode(&sample().encode()), Ok(sample()));
    }

    #[test]
    fn the_group_id_rises_across_generations_and_terms() {
        let stamp = |term, generation| PlanStamp { term, generation };
        assert!(stamp(3, 9).group_id() < stamp(3, 10).group_id());
        assert!(stamp(3, u32::MAX).group_id() < stamp(4, 0).group_id());
        for s in [stamp(0, 0), stamp(3, 9), stamp(u32::MAX, u32::MAX)] {
            assert_eq!(PlanStamp::from_group_id(s.group_id()), s);
        }
        assert!(stamp(3, 9) < stamp(4, 0));
    }

    #[test]
    fn fields_from_a_newer_version_are_skipped() {
        let mut payload = sample().encode();
        // Append unknown fields 200 (integer) and 201 (bytes): deltas 191 and 1 after field 9.
        payload.extend_from_slice(&[0x80, 191, 7, 1, 2, 0xaa, 0xbb]);
        assert_eq!(Plan::decode(&payload), Ok(sample()));
    }

    #[test]
    fn required_fields_must_be_present() {
        let mut w = Writer::default();
        w.int(STAMP, 1);
        w.int(AUDIO_TOP_N, 3);
        assert_eq!(
            Plan::decode(&w.finish()),
            Err(DecodeError::Missing("conductor"))
        );
        let mut w = Writer::default();
        let conductor = id_of(1);
        w.bytes(CONDUCTOR, conductor.as_bytes());
        w.int(AUDIO_TOP_N, 3);
        assert_eq!(
            Plan::decode(&w.finish()),
            Err(DecodeError::Missing("stamp"))
        );
    }

    #[test]
    fn a_sealed_plan_opens_and_tampering_or_replay_is_refused() {
        let plan = sample();
        let group = plan.stamp.group_id();
        let envelope = plan.seal(&meeting(), &FakeIdentity(1));
        assert_eq!(
            Plan::open(&meeting(), group, &envelope, &FakeVerifier),
            Ok(plan.clone())
        );

        let mut altered = envelope.clone();
        let at = altered.windows(3).position(|w| w == b"Bob").unwrap();
        altered[at] = b'R';
        assert!(matches!(
            Plan::open(&meeting(), group, &altered, &FakeVerifier),
            Err(DecodeError::BadSignature(_))
        ));
        assert!(matches!(
            Plan::open(&meeting(), group + 1, &envelope, &FakeVerifier),
            Err(DecodeError::Invalid(_))
        ));
        let other = MeetingTag::new(b"m-0000").unwrap();
        assert!(matches!(
            Plan::open(&other, group, &envelope, &FakeVerifier),
            Err(DecodeError::BadSignature(_))
        ));
    }

    #[test]
    fn a_plan_signed_by_someone_other_than_its_conductor_is_refused() {
        let plan = sample();
        let forged = plan.seal(&meeting(), &FakeIdentity(2));
        let got = Plan::open(&meeting(), plan.stamp.group_id(), &forged, &FakeVerifier);
        assert!(matches!(got, Err(DecodeError::BadSignature(_))));
    }

    #[test]
    fn a_conductor_outside_the_roster_is_refused() {
        let plan = Plan {
            conductor: id_of(9),
            ..sample()
        };
        let envelope = plan.seal(&meeting(), &FakeIdentity(9));
        let got = Plan::open(&meeting(), plan.stamp.group_id(), &envelope, &FakeVerifier);
        assert_eq!(
            got,
            Err(DecodeError::Invalid("conductor is not in the roster"))
        );
    }

    #[test]
    fn a_report_envelope_does_not_open_as_a_plan() {
        // Same signer, meeting and sequence number, but sealed as the other kind.
        let plan = sample();
        let group = plan.stamp.group_id();
        let as_report = seal(
            Kind::Report,
            &meeting(),
            group,
            &plan.encode(),
            &FakeIdentity(1),
        );
        assert!(matches!(
            Plan::open(&meeting(), group, &as_report, &FakeVerifier),
            Err(DecodeError::BadSignature(_))
        ));
    }

    #[test]
    fn replacement_follows_the_term_and_successor_rules() {
        let held = sample(); // term 3, generation 9, conductor 1, successors 2 and 3
        let with = |term, generation, conductor: u8| Plan {
            stamp: PlanStamp { term, generation },
            conductor: id_of(conductor),
            ..sample()
        };
        assert!(
            held.may_replace(&with(3, 10, 1)),
            "later generation from the same conductor"
        );
        assert!(!held.may_replace(&with(3, 9, 1)), "same plan again");
        assert!(!held.may_replace(&with(3, 8, 1)), "older generation");
        assert!(
            !held.may_replace(&with(3, 10, 2)),
            "another node issuing in the same term"
        );
        assert!(
            held.may_replace(&with(4, 0, 2)),
            "first successor takes the next term"
        );
        assert!(
            held.may_replace(&with(4, 0, 3)),
            "second successor takes the next term"
        );
        assert!(
            !held.may_replace(&with(4, 0, 4)),
            "a node not on the successor list"
        );
        assert!(
            !held.may_replace(&with(4, 0, 1)),
            "the old conductor is not its own successor"
        );
        assert!(!held.may_replace(&with(5, 0, 2)), "a skipped term");
        assert!(!held.may_replace(&with(2, 99, 1)), "an older term");
    }

    #[test]
    fn names_are_redacted_in_debug_output() {
        let out = format!("{:?}", sample());
        assert!(!out.contains("Alice") && !out.contains("Bob"), "{out}");
    }
}
