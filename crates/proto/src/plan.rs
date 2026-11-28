//! The conductor's plan: who is in the meeting and how the khefs are wired. Signed by the
//! conductor and flooded on the control namespace; everyone holds the whole plan (design
//! section 5). Phase 1 carries what audio needs; routing tables, priorities and budgets are
//! added as fields later.

use crate::fields::{DecodeError, Writer, fields, push};
use crate::names::PLAN_ALIAS;
use crate::sign::{Kind, PublicKey, Sealed, Signer, Verifier, seal};
use crate::{MediaTrack, MeetingTag, ParticipantId, Redacted, Track};
use palaver_moq_wire::Value;

// Field numbers. Even numbers carry integers, odd ones bytes. Never reuse a number.
const CONDUCTOR: u64 = 1;
const STAMP: u64 = 2;
const MEMBER: u64 = 3;
const AUDIO_TOP_N: u64 = 4;
const NEXT_SMALL_ALIAS: u64 = 6;
const NEXT_WIDE_ALIAS: u64 = 8;
const PEERING: u64 = 5;
const AUDIO_LINK: u64 = 7;
const SUCCESSOR: u64 = 9;

const MEMBER_KEY: u64 = 1;
const MEMBER_TRACK: u64 = 5;
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
    /// The `MoQ` group id a plan with this stamp is sent in: term in the high half, so group
    /// ids keep rising when the conductor, and so the track's publisher, changes.
    #[must_use]
    pub fn group_id(self) -> u64 {
        u64::from(self.term) << 32 | u64::from(self.generation)
    }

    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "splits the id into its high and low 32 bits; nothing is lost"
    )]
    pub const fn from_group_id(group_id: u64) -> Self {
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
    /// The media tracks the member may send, each with the alias the conductor gave it.
    pub tracks: Vec<AssignedTrack>,
}

/// A media track and the alias every khef uses for it.
///
/// The alias is the short number that stands for the track's name in each of its objects.
/// The conductor assigns it and nobody else changes it, so a khef forwards a datagram exactly
/// as it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignedTrack {
    pub track: MediaTrack,
    pub alias: u64,
}

/// Aliases below this are the small ones: at most two bytes in a packet, and one byte below
/// 128. The conductor keeps them for audio, whose packets are small. Other tracks get
/// aliases from here up.
pub const WIDE_ALIAS_START: u64 = 1 << 14;

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
    /// The next small alias and the next wide alias to assign: each one more than any
    /// given so far, to a member still here or not. An alias is never given twice in a
    /// meeting, so a late packet of a track that has gone cannot be taken for another's.
    /// Carried in the plan so that a new conductor carries on from them.
    pub next_small_alias: u64,
    pub next_wide_alias: u64,
    /// Who takes over as conductor, in order.
    pub successors: Vec<ParticipantId>,
}

impl Member {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        let tracks: Vec<Vec<u8>> = self
            .tracks
            .iter()
            .map(|assigned| assigned.track.encode(Some(assigned.alias)))
            .collect();
        w.bytes(MEMBER_KEY, &self.key.0);
        w.bytes(MEMBER_NAME, self.name.reveal().as_bytes());
        for track in &tracks {
            w.bytes(MEMBER_TRACK, track);
        }
        w.finish()
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (mut key, mut name, mut tracks) = (None, None, vec![]);
        for pair in fields(bytes) {
            let pair = pair?;
            match (pair.ty, pair.value) {
                (MEMBER_KEY, Value::Bytes(b)) => key = PublicKey::from_slice(b),
                (MEMBER_TRACK, Value::Bytes(b)) => {
                    let (track, alias) = MediaTrack::decode(b)?;
                    let alias = alias.ok_or(DecodeError::Missing("track alias"))?;
                    push(
                        &mut tracks,
                        AssignedTrack { track, alias },
                        "too many tracks",
                    )?;
                }
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
            tracks,
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
        w.int(NEXT_SMALL_ALIAS, self.next_small_alias);
        w.int(NEXT_WIDE_ALIAS, self.next_wide_alias);
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
    ///
    /// # Errors
    ///
    /// Fails if the payload is not well-formed fields, a required field is missing, a field
    /// has a value that is not allowed, a list is over its limit, two tracks share an alias,
    /// or an alias is not below the next one to give.
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let (mut conductor, mut stamp, mut audio_top_n) = (None, None, None);
        let (mut next_small_alias, mut next_wide_alias) = (None, None);
        let (mut roster, mut peerings, mut audio_overlay, mut successors) =
            (vec![], vec![], vec![], vec![]);
        for pair in fields(payload) {
            let pair = pair?;
            match (pair.ty, pair.value) {
                (CONDUCTOR, Value::Bytes(b)) => conductor = ParticipantId::new(b),
                (STAMP, Value::Int(v)) => stamp = Some(PlanStamp::from_group_id(v)),
                (AUDIO_TOP_N, Value::Int(v)) => audio_top_n = Some(v),
                (NEXT_SMALL_ALIAS, Value::Int(v)) => next_small_alias = Some(v),
                (NEXT_WIDE_ALIAS, Value::Int(v)) => next_wide_alias = Some(v),
                (MEMBER, Value::Bytes(b)) => {
                    push(&mut roster, Member::decode(b)?, "too many members")?;
                }
                (PEERING, Value::Bytes(b)) => {
                    push(&mut peerings, Edge::decode(b)?, "too many peerings")?;
                }
                (AUDIO_LINK, Value::Bytes(b)) => {
                    push(&mut audio_overlay, Edge::decode(b)?, "too many audio links")?;
                }
                (SUCCESSOR, Value::Bytes(b)) => {
                    let id = ParticipantId::new(b).ok_or(DecodeError::Invalid("successor id"))?;
                    push(&mut successors, id, "too many successors")?;
                }
                _ => {}
            }
        }
        let next_small_alias = next_small_alias.ok_or(DecodeError::Missing("next small alias"))?;
        let next_wide_alias = next_wide_alias.ok_or(DecodeError::Missing("next wide alias"))?;
        // Every alias stands for one track, lies in the range for plan-assigned aliases, and
        // is below the next one to give.
        let mut aliases: Vec<u64> = roster
            .iter()
            .flat_map(|member: &Member| &member.tracks)
            .map(|assigned| assigned.alias)
            .collect();
        aliases.sort_unstable();
        if aliases.windows(2).any(|pair| pair.first() == pair.get(1)) {
            return Err(DecodeError::Invalid("two tracks have the same alias"));
        }
        let in_range = |alias: &u64| {
            if *alias < WIDE_ALIAS_START {
                *alias < next_small_alias
            } else {
                *alias < next_wide_alias && *alias < PLAN_ALIAS
            }
        };
        if !aliases.iter().all(in_range) {
            return Err(DecodeError::Invalid("an alias is not below the next one"));
        }
        Ok(Self {
            stamp: stamp.ok_or(DecodeError::Missing("stamp"))?,
            conductor: conductor.ok_or(DecodeError::Missing("conductor"))?,
            roster,
            peerings,
            audio_overlay,
            audio_top_n: audio_top_n.ok_or(DecodeError::Missing("audio top-N"))?,
            next_small_alias,
            next_wide_alias,
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

    /// The roster entry of the member with id `participant`.
    pub fn member(&self, participant: &ParticipantId, verifier: &dyn Verifier) -> Option<&Member> {
        self.roster
            .iter()
            .find(|member| verifier.participant_id(&member.key) == *participant)
    }

    /// The alias every khef uses for `track` while this plan is in force. For a media track
    /// it is the one the plan assigns; `None` if the plan does not list the node or the
    /// track, in which case the track may not be sent. For a control track it does not
    /// depend on the plan.
    pub fn alias_of(&self, track: &Track, verifier: &dyn Verifier) -> Option<u64> {
        let Some(media) = track.media() else {
            return track.control_alias();
        };
        self.member(track.participant()?, verifier)?
            .tracks
            .iter()
            .find(|assigned| assigned.track.same_name(&media))
            .map(|assigned| assigned.alias)
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

    /// Verify and decode an object received on the plan track in group `group_id`.
    ///
    /// This shows the plan is what its conductor issued. Whether that node is entitled to be
    /// conductor is a separate question; see [`Plan::may_replace`].
    ///
    /// # Errors
    ///
    /// Fails on the first of these checks that does not hold, in order: the envelope and
    /// payload are well formed; the stamp is the group id; the conductor is in the roster;
    /// the signature is by the conductor's key.
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

    /// The parts of the plan that tell `node` what to do: the peerings and audio links it is
    /// an end of. Grows as the plan gains fields (routes, budgets).
    fn entries_for<'a>(&'a self, node: &'a ParticipantId) -> impl Iterator<Item = (u8, &'a Edge)> {
        let mine = move |edge: &&Edge| edge.0 == *node || edge.1 == *node;
        let peerings = self.peerings.iter().filter(mine).map(|edge| (0, edge));
        let audio = self.audio_overlay.iter().filter(mine).map(|edge| (1, edge));
        peerings.chain(audio)
    }

    /// Whether this plan asks `node` to do anything `previous` did not: whether its own
    /// entries differ. A node for which they do applies the plan and says so at once; the
    /// conductor waits only for those nodes. Everyone else just notes the new plan.
    #[must_use]
    pub fn changes_for(&self, previous: &Self, node: &ParticipantId) -> bool {
        !self.entries_for(node).eq(previous.entries_for(node))
    }

    /// The nodes this plan asks something new of, compared with `previous`: every node at
    /// an end of an edge that was added or removed. Sorted, without duplicates.
    #[must_use]
    pub fn changed_nodes(&self, previous: &Self) -> Vec<ParticipantId> {
        let mut out: Vec<ParticipantId> = Vec::new();
        for (new, old) in [
            (&self.peerings, &previous.peerings),
            (&self.audio_overlay, &previous.audio_overlay),
        ] {
            let differing = new
                .iter()
                .filter(|edge| !old.contains(edge))
                .chain(old.iter().filter(|edge| !new.contains(edge)));
            for edge in differing {
                out.push(edge.0.clone());
                out.push(edge.1.clone());
            }
        }
        out.sort();
        out.dedup();
        out
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
    #[must_use]
    pub fn may_replace(&self, next: &Self) -> bool {
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

    fn mic() -> MediaTrack {
        MediaTrack {
            audio: true,
            source: crate::Source::Main,
            rendition: b"opus".to_vec(),
            shape: None,
        }
    }

    fn camera() -> MediaTrack {
        MediaTrack {
            audio: false,
            source: crate::Source::Main,
            rendition: b"360p".to_vec(),
            shape: None,
        }
    }

    /// Member `seed` has a microphone, alias `seed`, and a camera, alias `16384 + seed`.
    fn member(seed: u8, name: &str) -> Member {
        Member {
            key: FakeIdentity(seed).public_key(),
            name: Redacted::new(name.to_owned()),
            tracks: vec![
                AssignedTrack {
                    track: mic(),
                    alias: u64::from(seed),
                },
                AssignedTrack {
                    track: camera(),
                    alias: WIDE_ALIAS_START + u64::from(seed),
                },
            ],
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
            next_small_alias: 10,
            next_wide_alias: WIDE_ALIAS_START + 10,
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
        w.int(NEXT_SMALL_ALIAS, 0);
        w.int(NEXT_WIDE_ALIAS, WIDE_ALIAS_START);
        assert_eq!(
            Plan::decode(&w.finish()),
            Err(DecodeError::Missing("conductor"))
        );
        let mut w = Writer::default();
        let conductor = id_of(1);
        w.bytes(CONDUCTOR, conductor.as_bytes());
        w.int(AUDIO_TOP_N, 3);
        w.int(NEXT_SMALL_ALIAS, 0);
        w.int(NEXT_WIDE_ALIAS, WIDE_ALIAS_START);
        assert_eq!(
            Plan::decode(&w.finish()),
            Err(DecodeError::Missing("stamp"))
        );
        let mut w = Writer::default();
        w.bytes(CONDUCTOR, conductor.as_bytes());
        w.int(STAMP, 1);
        w.int(AUDIO_TOP_N, 3);
        assert_eq!(
            Plan::decode(&w.finish()),
            Err(DecodeError::Missing("next small alias"))
        );
    }

    #[test]
    fn a_sealed_plan_opens_and_tampering_or_replay_is_refused() {
        let plan = sample();
        let group = plan.stamp.group_id();
        let envelope = plan.seal(&meeting(), &FakeIdentity(1));
        assert_eq!(
            Plan::open(&meeting(), group, &envelope, &FakeVerifier),
            Ok(plan)
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

    #[test]
    fn a_plan_changes_things_only_for_the_nodes_at_the_ends_of_changed_edges() {
        let before = sample(); // line 1-2-3-4, plus peering 1-3
        // 5 joins, attached to 4 and peered with 2 as well.
        let mut after = sample();
        after.roster.push(member(5, "Eve"));
        after.peerings.push(Edge(id_of(4), id_of(5)));
        after.peerings.push(Edge(id_of(2), id_of(5)));
        after.audio_overlay.push(Edge(id_of(4), id_of(5)));

        for (node, changed) in [(1, false), (2, true), (3, false), (4, true), (5, true)] {
            assert_eq!(
                after.changes_for(&before, &id_of(node)),
                changed,
                "node {node}"
            );
        }
        let mut expected = vec![id_of(2), id_of(4), id_of(5)];
        expected.sort();
        assert_eq!(after.changed_nodes(&before), expected);
        // And the same set when the edges go away again.
        assert_eq!(before.changed_nodes(&after), expected);
        assert_eq!(before.changed_nodes(&before), []);
    }

    #[test]
    fn a_peering_that_becomes_an_audio_link_is_a_change_for_its_ends_only() {
        let before = sample();
        let mut after = sample();
        after
            .audio_overlay
            .retain(|e| *e != Edge(id_of(2), id_of(3)));
        after.audio_overlay.push(Edge(id_of(1), id_of(3)));
        assert!(after.changes_for(&before, &id_of(1)));
        assert!(after.changes_for(&before, &id_of(2)));
        assert!(after.changes_for(&before, &id_of(3)));
        assert!(!after.changes_for(&before, &id_of(4)));
    }

    #[test]
    fn a_new_stamp_or_roster_alone_changes_nothing_for_anyone() {
        let before = sample();
        let mut after = sample();
        after.stamp.generation += 1;
        after.roster.push(member(9, "Zed"));
        for node in 1..=4 {
            assert!(!after.changes_for(&before, &id_of(node)));
        }
    }

    #[test]
    fn aliases_must_be_distinct_and_below_the_next_one_of_their_range() {
        let mut twice = sample();
        twice.roster[2].tracks[0].alias = twice.roster[0].tracks[0].alias;
        assert_eq!(
            Plan::decode(&twice.encode()),
            Err(DecodeError::Invalid("two tracks have the same alias"))
        );
        for (member, track, alias) in [
            (3, 0, 10),                    // a small alias not below the next small one
            (3, 1, WIDE_ALIAS_START + 10), // a wide one not below the next wide one
            (3, 1, PLAN_ALIAS + 5),        // in the range kept for control tracks
        ] {
            let mut beyond = sample();
            beyond.roster[member].tracks[track].alias = alias;
            beyond.next_wide_alias = beyond.next_wide_alias.max(alias.min(PLAN_ALIAS + 9));
            if alias == WIDE_ALIAS_START + 10 {
                beyond.next_wide_alias = WIDE_ALIAS_START + 10;
            }
            assert_eq!(
                Plan::decode(&beyond.encode()),
                Err(DecodeError::Invalid("an alias is not below the next one")),
                "alias {alias}"
            );
        }
        // Gaps are fine: tracks that went took their aliases with them.
        let mut gaps = sample();
        gaps.roster[3].tracks[0].alias = 9;
        assert_eq!(Plan::decode(&gaps.encode()), Ok(gaps));
        // A member with no tracks is fine.
        let mut silent = sample();
        silent.roster[1].tracks.clear();
        assert_eq!(Plan::decode(&silent.encode()), Ok(silent));
    }

    #[test]
    fn a_track_in_the_plan_must_have_an_alias() {
        let mut w = Writer::default();
        let track = mic().encode(None);
        w.bytes(MEMBER_KEY, &[1; 32]);
        w.bytes(MEMBER_NAME, b"Alice");
        w.bytes(MEMBER_TRACK, &track);
        assert_eq!(
            Member::decode(&w.finish()),
            Err(DecodeError::Missing("track alias"))
        );
    }

    #[test]
    fn the_plan_says_which_alias_stands_for_a_track() {
        let plan = sample();
        assert_eq!(
            plan.alias_of(&mic().of(id_of(3)), &FakeVerifier),
            Some(3),
            "one byte on the wire"
        );
        assert_eq!(
            plan.alias_of(&camera().of(id_of(3)), &FakeVerifier),
            Some(WIDE_ALIAS_START + 3)
        );
        // A node the plan does not list, and a track it does not list, may not be sent.
        assert_eq!(plan.alias_of(&mic().of(id_of(9)), &FakeVerifier), None);
        let screen = MediaTrack {
            source: crate::Source::Screen,
            ..camera()
        };
        assert_eq!(plan.alias_of(&screen.of(id_of(3)), &FakeVerifier), None);
        // Control tracks do not depend on the roster.
        let report = Track::Report {
            participant: id_of(9),
        };
        assert_eq!(
            plan.alias_of(&report, &FakeVerifier),
            report.control_alias()
        );
        assert_eq!(plan.alias_of(&Track::Plan, &FakeVerifier), Some(PLAN_ALIAS));
        assert!(plan.member(&id_of(9), &FakeVerifier).is_none());
    }
}
