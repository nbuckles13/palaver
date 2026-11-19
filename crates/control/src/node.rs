//! What every node does on the control plane, conductor or not:
//!
//! - publishes its own signed report once a second, and sooner when something in it changes
//!   (but never two within [`REPORT_MIN_GAP`]);
//! - keeps the latest verified report from every other node;
//! - holds the current plan, adopting a new one only under the replacement rule.
//!
//! And what the conductor does on top: it keeps the roster (every node whose reports arrive)
//! and issues a new plan whenever the roster or the wiring it calls for changes.
//!
//! Sans-IO: the caller hands in received objects and the current time, and asks what to send.

use palaver_proto::{
    DecodeError, Edge, Link, MeetingTag, Member, ParticipantId, Plan, PlanStamp, Redacted, Report,
    Signer, Timestamp, Verifier,
};
use palaver_topology::{Input, ReportedLink, wire};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

/// How often a node reports when nothing has changed. Also the conductor's heartbeat.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// The least time between two reports from a node. A change is reported at once unless a
/// report went out less than this long ago; then it waits, and several changes share one
/// report. Without it every plan makes every node report at once, and each of those reports
/// floods to every node.
pub const REPORT_MIN_GAP: Duration = Duration::from_millis(200);
/// A node whose last report is older than this is no longer considered present.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(3);
/// The term of a meeting's first conductor, its creator.
pub const FIRST_TERM: u32 = 1;
/// The N of top-N audio selection that a conductor puts in its plans.
pub const AUDIO_TOP_N: u64 = 3;

/// What a node is told once, at start.
#[derive(Debug, Clone)]
pub struct Config {
    pub meeting: MeetingTag,
    /// The name the user chose.
    pub name: Redacted<String>,
    /// The sequence number of this node's first report. Must be higher than any it used before
    /// with the same key, or other nodes will take its reports for old ones; the time the node
    /// started, in milliseconds since some fixed date, serves.
    pub first_sequence: u64,
}

/// The parts of a report that come from the rest of the node: its links and addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalView {
    pub links: Vec<Link>,
    pub unreachable: Vec<ParticipantId>,
    pub addresses: Vec<Redacted<SocketAddr>>,
    pub relay_url: Option<String>,
}

/// Something for the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Publish `object` on this node's report track, as the only object of group `group_id`.
    PublishReport { group_id: u64, object: Vec<u8> },
    /// Publish `object` on the plan track, as the only object of group `group_id`. Only the
    /// conductor produces this. The node has already adopted the plan.
    PublishPlan { group_id: u64, object: Vec<u8> },
}

/// What became of a plan object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanVerdict {
    /// It is now the current plan.
    Adopted,
    /// It is the plan already held.
    Duplicate,
    /// Genuine, but not one this node may move to from the plan it holds: an older plan, or a
    /// conductor that is not entitled to issue it.
    NotAllowed,
    /// Malformed, or not signed by its conductor.
    Invalid(DecodeError),
}

/// What became of a report object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportVerdict {
    /// It is now the latest report from that node.
    Accepted,
    /// Not newer than the one already held, or this node's own report coming back.
    Stale,
    /// Malformed, or not signed by the node the track belongs to.
    Invalid(DecodeError),
}

#[derive(Debug, Clone)]
struct Heard {
    report: Report,
    at: Timestamp,
}

/// Whether two reports from one node would lead the conductor to the same plan: the same
/// identity, the same links by the same kind of path, the same unreachable nodes. Round-trip
/// times are left out: they move all the time, and matter only when an edge is next chosen.
fn same_for_planning(old: &Report, new: &Report) -> bool {
    let paths = |report: &Report| -> Vec<_> {
        report
            .links
            .iter()
            .map(|link| (link.peer.clone(), link.path))
            .collect()
    };
    old.key == new.key
        && old.name == new.name
        && old.unreachable == new.unreachable
        && paths(old) == paths(new)
}

/// One node's control-plane state.
pub struct Node<S, V> {
    config: Config,
    signer: S,
    verifier: V,
    me: ParticipantId,
    plan: Option<Plan>,
    reports: BTreeMap<ParticipantId, Heard>,
    view: LocalView,
    /// Something in the next report differs from the last one sent.
    changed: bool,
    next_sequence: u64,
    last_sent: Option<Timestamp>,
    /// Set while this node is conductor.
    conducting: Option<Conducting>,
}

/// The conductor's own state.
#[derive(Debug, Clone, Copy)]
struct Conducting {
    term: u32,
    /// Reports or this node's links changed since the plan was last checked against them.
    stale: bool,
}

impl<S: Signer, V: Verifier> Node<S, V> {
    pub fn new(config: Config, signer: S, verifier: V) -> Self {
        let me = verifier.participant_id(&signer.public_key());
        let next_sequence = config.first_sequence;
        Self {
            config,
            signer,
            verifier,
            me,
            plan: None,
            reports: BTreeMap::new(),
            view: LocalView::default(),
            changed: true,
            next_sequence,
            last_sent: None,
            conducting: None,
        }
    }

    /// Become the conductor of a new meeting: this node created it. The first plan, term
    /// [`FIRST_TERM`] with a roster of one, comes from the next [`Node::poll`]. Returns
    /// `false`, and does nothing, if the node already follows a plan.
    pub fn create_meeting(&mut self) -> bool {
        if self.plan.is_some() {
            return false;
        }
        self.conducting = Some(Conducting {
            term: FIRST_TERM,
            stale: true,
        });
        true
    }

    /// Whether this node is the conductor.
    pub fn is_conductor(&self) -> bool {
        self.conducting.is_some()
    }

    fn mark_stale(&mut self) {
        if let Some(conducting) = &mut self.conducting {
            conducting.stale = true;
        }
    }

    /// This node's participant id.
    pub fn me(&self) -> &ParticipantId {
        &self.me
    }

    /// The plan this node follows, once it has one.
    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    /// The latest report from another node.
    pub fn report_of(&self, participant: &ParticipantId) -> Option<&Report> {
        self.reports.get(participant).map(|heard| &heard.report)
    }

    /// Tell the node what its links and addresses are now. A change is reported at once.
    pub fn set_view(&mut self, view: LocalView) {
        if view != self.view {
            self.view = view;
            self.changed = true;
            self.mark_stale();
        }
    }

    /// An object arrived on the plan track in group `group_id`.
    pub fn on_plan(&mut self, group_id: u64, envelope: &[u8]) -> PlanVerdict {
        // A plan floods in over every link, so most arrivals are copies of the one held, or
        // older. Those are settled by the group id alone, before paying for a signature
        // check; nothing is adopted on this path, so a forged copy gains nothing.
        if let Some(held) = &self.plan {
            let held_group = held.stamp.group_id();
            if group_id == held_group {
                return PlanVerdict::Duplicate;
            }
            if group_id < held_group {
                return PlanVerdict::NotAllowed;
            }
        }
        let next = match Plan::open(&self.config.meeting, group_id, envelope, &self.verifier) {
            Ok(plan) => plan,
            Err(e) => return PlanVerdict::Invalid(e),
        };
        match &self.plan {
            Some(held) if held.stamp == next.stamp && held.conductor == next.conductor => {
                return PlanVerdict::Duplicate;
            }
            Some(held) if !held.may_replace(&next) => return PlanVerdict::NotAllowed,
            // A node with no plan has nothing to check against and takes the first verified one.
            _ => {}
        }
        // Someone else's plan is in force: this node is not, or no longer, the conductor.
        self.conducting = None;
        self.plan = Some(next);
        self.changed = true; // the report says which plan is applied
        PlanVerdict::Adopted
    }

    /// An object arrived on `participant`'s report track in group `group_id`, at time `now`.
    pub fn on_report(
        &mut self,
        participant: &ParticipantId,
        group_id: u64,
        envelope: &[u8],
        now: Timestamp,
    ) -> ReportVerdict {
        if participant == &self.me {
            return ReportVerdict::Stale;
        }
        if self
            .reports
            .get(participant)
            .is_some_and(|heard| group_id <= heard.report.sequence)
        {
            return ReportVerdict::Stale;
        }
        match Report::open(
            &self.config.meeting,
            participant,
            group_id,
            envelope,
            &self.verifier,
        ) {
            Ok(report) => {
                let previous = self
                    .reports
                    .insert(participant.clone(), Heard { report, at: now });
                let latest = self.reports.get(participant).map(|heard| &heard.report);
                // Most reports are the once-a-second repeat. The conductor looks at its plan
                // again only when one says something the plan depends on.
                if !previous
                    .zip(latest)
                    .is_some_and(|(old, new)| same_for_planning(&old.report, new))
                {
                    self.mark_stale();
                }
                ReportVerdict::Accepted
            }
            Err(e) => ReportVerdict::Invalid(e),
        }
    }

    /// The reports of the other nodes present at `now`.
    fn heard(&self, now: Timestamp) -> impl Iterator<Item = (&ParticipantId, &Report)> {
        self.reports
            .iter()
            .filter(move |(_, heard)| now.saturating_since(heard.at) < REPORT_TIMEOUT)
            .map(|(id, heard)| (id, &heard.report))
    }

    /// The plan the conductor would issue at `now`, with the stamp left for the caller: the
    /// roster is every node present, wired by the topology rule starting from the plan held.
    fn draft(&self, term: u32, now: Timestamp) -> Plan {
        let mut roster = vec![(
            self.me.clone(),
            Member {
                key: self.signer.public_key(),
                name: self.config.name.clone(),
            },
        )];
        let mut links: Vec<ReportedLink> = Vec::new();
        let mut unreachable: Vec<Edge> = Vec::new();
        let mut see = |from: &ParticipantId, theirs: &[Link], cannot: &[ParticipantId]| {
            links.extend(theirs.iter().map(|link| ReportedLink {
                from: from.clone(),
                to: link.peer.clone(),
                path: link.path,
                rtt: link.rtt,
            }));
            unreachable.extend(cannot.iter().map(|peer| Edge(from.clone(), peer.clone())));
        };
        see(&self.me, &self.view.links, &self.view.unreachable);
        for (id, report) in self.heard(now) {
            see(id, &report.links, &report.unreachable);
            roster.push((
                id.clone(),
                Member {
                    key: report.key,
                    name: report.name.clone(),
                },
            ));
        }
        roster.sort_by(|a, b| a.0.cmp(&b.0));
        let (previous_peerings, previous_audio_overlay) = match &self.plan {
            Some(held) => (held.peerings.clone(), held.audio_overlay.clone()),
            None => (Vec::new(), Vec::new()),
        };
        let wiring = wire(&Input {
            members: roster.iter().map(|(id, _)| id.clone()).collect(),
            root: Some(self.me.clone()),
            links,
            unreachable,
            previous_peerings,
            previous_audio_overlay,
        });
        Plan {
            stamp: PlanStamp {
                term,
                generation: 0,
            },
            conductor: self.me.clone(),
            roster: roster.into_iter().map(|(_, member)| member).collect(),
            peerings: wiring.peerings,
            audio_overlay: wiring.audio_overlay,
            audio_top_n: AUDIO_TOP_N,
            // Phase 1 has no succession.
            successors: Vec::new(),
        }
    }

    /// Whether the roster of the plan held is exactly the nodes present at `now`.
    fn roster_is_current(&self, now: Timestamp) -> bool {
        let Some(held) = &self.plan else {
            return false;
        };
        let mut present = self.present(now);
        let mut roster: Vec<ParticipantId> = held
            .roster
            .iter()
            .map(|member| self.verifier.participant_id(&member.key))
            .collect();
        present.sort();
        roster.sort();
        present == roster
    }

    /// As conductor: issue a new plan if the one held no longer matches the meeting.
    fn poll_plan(&mut self, now: Timestamp) -> Option<Output> {
        let conducting = self.conducting.as_mut()?;
        let stale = core::mem::replace(&mut conducting.stale, false);
        let term = conducting.term;
        if !stale && self.roster_is_current(now) {
            return None;
        }
        let mut next = self.draft(term, now);
        if let Some(held) = &self.plan {
            next.stamp = held.stamp;
            if next == *held {
                return None;
            }
            // A term with no generations left cannot issue; a later term is another node's.
            next.stamp.generation = held.stamp.generation.checked_add(1)?;
        }
        let object = next.seal(&self.config.meeting, &self.signer);
        let group_id = next.stamp.group_id();
        self.plan = Some(next);
        self.changed = true; // the report says which plan is applied
        Some(Output::PublishPlan { group_id, object })
    }

    /// Ask what to send. Returns a plan if this node is conductor and the meeting changed,
    /// then a report when one is due: the first time, when something in it changed, or a
    /// second after the last. Call again until it returns `None`.
    pub fn poll(&mut self, now: Timestamp) -> Option<Output> {
        if let Some(plan) = self.poll_plan(now) {
            return Some(plan);
        }
        let wait = if self.changed {
            REPORT_MIN_GAP
        } else {
            REPORT_INTERVAL
        };
        if self
            .last_sent
            .is_some_and(|sent| now.saturating_since(sent) < wait)
        {
            return None;
        }
        let report = Report {
            key: self.signer.public_key(),
            sequence: self.next_sequence,
            name: self.config.name.clone(),
            applied_plan: self.plan.as_ref().map(|plan| plan.stamp),
            links: self.view.links.clone(),
            unreachable: self.view.unreachable.clone(),
            addresses: self.view.addresses.clone(),
            relay_url: self.view.relay_url.clone(),
        };
        let object = report.seal(&self.config.meeting, &self.signer);
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.last_sent = Some(now);
        self.changed = false;
        Some(Output::PublishReport {
            group_id: report.sequence,
            object,
        })
    }

    /// When to call [`Node::poll`] next if nothing else happens.
    pub fn next_wake(&self, now: Timestamp) -> Timestamp {
        let report = match self.last_sent {
            Some(sent) if self.changed => sent.saturating_add(REPORT_MIN_GAP).max(now),
            Some(sent) => sent.saturating_add(REPORT_INTERVAL),
            None => now,
        };
        let Some(conducting) = &self.conducting else {
            return report;
        };
        if conducting.stale {
            return now;
        }
        // The conductor must also notice the next member whose reports stop.
        self.reports
            .values()
            .map(|heard| heard.at.saturating_add(REPORT_TIMEOUT))
            .filter(|expiry| *expiry > now)
            .fold(report, Timestamp::min)
    }

    /// The nodes considered present at `now`: this one, and every node whose latest report
    /// arrived less than [`REPORT_TIMEOUT`] ago.
    pub fn present(&self, now: Timestamp) -> Vec<ParticipantId> {
        let mut out = vec![self.me.clone()];
        out.extend(self.heard(now).map(|(id, _)| id.clone()));
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use palaver_proto::PathKind;
    use palaver_proto::sign::fake::{FakeIdentity, FakeVerifier};

    /// A time, given in milliseconds.
    fn at(ms: u64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    fn meeting() -> MeetingTag {
        MeetingTag::new(b"m-7f3a").unwrap()
    }

    fn id_of(seed: u8) -> ParticipantId {
        FakeVerifier.participant_id(&FakeIdentity(seed).public_key())
    }

    fn node(seed: u8) -> Node<FakeIdentity, FakeVerifier> {
        let config = Config {
            meeting: meeting(),
            name: Redacted::new(format!("node {seed}")),
            first_sequence: 1000,
        };
        Node::new(config, FakeIdentity(seed), FakeVerifier)
    }

    /// A plan for members `1..=members`, issued and signed by `conductor`.
    fn plan(
        term: u32,
        generation: u32,
        conductor: u8,
        successors: &[u8],
        members: u8,
    ) -> (u64, Vec<u8>) {
        plan_signed_by(term, generation, conductor, successors, members, conductor)
    }

    /// The same, signed by `signer`, who need not be the conductor it names.
    fn plan_signed_by(
        term: u32,
        generation: u32,
        conductor: u8,
        successors: &[u8],
        members: u8,
        signer: u8,
    ) -> (u64, Vec<u8>) {
        let roster = (1..=members)
            .map(|m| Member {
                key: FakeIdentity(m).public_key(),
                name: Redacted::new(format!("node {m}")),
            })
            .collect();
        let plan = Plan {
            stamp: PlanStamp { term, generation },
            conductor: id_of(conductor),
            roster,
            peerings: vec![Edge(id_of(1), id_of(2))],
            audio_overlay: vec![Edge(id_of(1), id_of(2))],
            audio_top_n: 3,
            successors: successors.iter().map(|s| id_of(*s)).collect(),
        };
        (
            plan.stamp.group_id(),
            plan.seal(&meeting(), &FakeIdentity(signer)),
        )
    }

    fn opened(output: Option<Output>, from: u8) -> Report {
        let Some(Output::PublishReport { group_id, object }) = output else {
            panic!("no report")
        };
        Report::open(&meeting(), &id_of(from), group_id, &object, &FakeVerifier).unwrap()
    }

    #[test]
    fn a_node_reports_at_once_then_once_a_second() {
        let mut n = node(1);
        assert_eq!(n.next_wake(at(0)), at(0));
        let first = opened(n.poll(at(0)), 1);
        assert_eq!((first.sequence, first.applied_plan), (1000, None));
        assert_eq!(n.poll(at(0)), None);
        assert_eq!(n.poll(at(999)), None);
        assert_eq!(n.next_wake(at(500)), at(1000));
        assert_eq!(opened(n.poll(at(1000)), 1).sequence, 1001);
        assert_eq!(n.poll(at(1500)), None);
        assert_eq!(opened(n.poll(at(2100)), 1).sequence, 1002);
    }

    #[test]
    fn a_change_is_reported_at_once_but_never_within_the_minimum_gap() {
        let mut n = node(1);
        n.poll(at(0));
        let view = LocalView {
            links: vec![Link {
                peer: id_of(2),
                path: PathKind::Direct,
                rtt: Duration::from_millis(3),
            }],
            addresses: vec![Redacted::new("192.168.1.5:4433".parse().unwrap())],
            ..LocalView::default()
        };
        n.set_view(view.clone());
        // A report went out 10 ms ago, so this one waits for the gap to pass.
        assert_eq!(n.next_wake(at(10)), at(200));
        assert_eq!(n.poll(at(10)), None);
        assert_eq!(n.poll(at(199)), None);
        let report = opened(n.poll(at(200)), 1);
        assert_eq!(report.links, view.links);
        assert_eq!(report.addresses, view.addresses);
        n.set_view(view);
        assert_eq!(n.poll(at(400)), None, "an unchanged view is not a change");
        // The one-second cadence now runs from the report just sent.
        assert_eq!(n.poll(at(1199)), None);
        assert!(n.poll(at(1200)).is_some());
        // Once the gap has passed, a change is reported at once.
        n.set_view(LocalView::default());
        assert_eq!(n.next_wake(at(1500)), at(1500));
        assert!(n.poll(at(1500)).is_some());
    }

    #[test]
    fn a_joiner_adopts_the_first_verified_plan_and_reports_it() {
        let mut n = node(3);
        n.poll(at(0));
        let (group, envelope) = plan(4, 7, 1, &[2], 2);
        assert_eq!(n.on_plan(group, &envelope), PlanVerdict::Adopted);
        assert_eq!(
            n.plan().map(|p| p.stamp),
            Some(PlanStamp {
                term: 4,
                generation: 7
            })
        );
        assert_eq!(
            opened(n.poll(at(200)), 3).applied_plan,
            Some(PlanStamp {
                term: 4,
                generation: 7
            })
        );
    }

    #[test]
    fn plans_are_adopted_only_under_the_replacement_rule() {
        let mut n = node(3);
        let send = |n: &mut Node<_, _>, term, generation, conductor, successors: &[u8]| {
            let (group, envelope) = plan(term, generation, conductor, successors, 3);
            n.on_plan(group, &envelope)
        };
        assert_eq!(send(&mut n, 1, 5, 1, &[2]), PlanVerdict::Adopted);
        assert_eq!(send(&mut n, 1, 5, 1, &[2]), PlanVerdict::Duplicate);
        assert_eq!(
            send(&mut n, 1, 4, 1, &[2]),
            PlanVerdict::NotAllowed,
            "older generation"
        );
        assert_eq!(
            send(&mut n, 1, 6, 1, &[2]),
            PlanVerdict::Adopted,
            "later generation"
        );
        assert_eq!(
            send(&mut n, 1, 7, 2, &[1]),
            PlanVerdict::NotAllowed,
            "another conductor in the same term"
        );
        assert_eq!(
            send(&mut n, 2, 0, 3, &[1]),
            PlanVerdict::NotAllowed,
            "not on the successor list"
        );
        assert_eq!(
            send(&mut n, 3, 0, 2, &[1]),
            PlanVerdict::NotAllowed,
            "a skipped term"
        );
        assert_eq!(
            send(&mut n, 2, 0, 2, &[1]),
            PlanVerdict::Adopted,
            "the successor takes the next term"
        );
        assert_eq!(n.plan().map(|p| p.conductor.clone()), Some(id_of(2)));
        assert_eq!(
            send(&mut n, 1, 9, 1, &[2]),
            PlanVerdict::NotAllowed,
            "the old conductor, after the hand-over"
        );
    }

    #[test]
    fn a_forged_or_damaged_plan_is_invalid_and_changes_nothing() {
        let mut n = node(3);
        let (group, envelope) = plan(1, 1, 1, &[2], 2);
        assert_eq!(n.on_plan(group, &envelope), PlanVerdict::Adopted);
        // Names node 1 as conductor but is signed by node 2.
        let (group, forged) = plan_signed_by(1, 2, 1, &[2], 2, 2);
        assert!(matches!(
            n.on_plan(group, &forged),
            PlanVerdict::Invalid(DecodeError::BadSignature(_))
        ));
        let (group, mut damaged) = plan(1, 3, 1, &[2], 2);
        *damaged.last_mut().unwrap() ^= 1;
        assert!(matches!(
            n.on_plan(group, &damaged),
            PlanVerdict::Invalid(DecodeError::BadSignature(_))
        ));
        assert_eq!(
            n.plan().map(|p| p.stamp),
            Some(PlanStamp {
                term: 1,
                generation: 1
            })
        );
    }

    #[test]
    fn reports_from_others_are_kept_if_genuine_and_newer() {
        let mut a = node(1);
        let mut b = node(2);
        let Some(Output::PublishReport { group_id, object }) = b.poll(at(0)) else {
            panic!()
        };
        assert_eq!(
            a.on_report(&id_of(2), group_id, &object, at(5)),
            ReportVerdict::Accepted
        );
        assert_eq!(a.report_of(&id_of(2)).map(|r| r.sequence), Some(1000));
        // The same report again, as a flood delivers it over a second path.
        assert_eq!(
            a.on_report(&id_of(2), group_id, &object, at(6)),
            ReportVerdict::Stale
        );
        // Presented on someone else's track.
        assert!(matches!(
            a.on_report(&id_of(3), group_id, &object, at(6)),
            ReportVerdict::Invalid(_)
        ));
        // The next one replaces it.
        let Some(Output::PublishReport { group_id, object }) = b.poll(at(1000)) else {
            panic!()
        };
        assert_eq!(
            a.on_report(&id_of(2), group_id, &object, at(1005)),
            ReportVerdict::Accepted
        );
        assert_eq!(a.report_of(&id_of(2)).map(|r| r.sequence), Some(1001));
        // A node's own report coming back is ignored.
        let Some(Output::PublishReport { group_id, object }) = a.poll(at(0)) else {
            panic!()
        };
        assert_eq!(
            a.on_report(&id_of(1), group_id, &object, at(0)),
            ReportVerdict::Stale
        );
    }

    #[test]
    fn a_node_is_present_until_three_seconds_pass_without_a_report() {
        let mut a = node(1);
        let mut b = node(2);
        assert_eq!(a.present(at(0)), [id_of(1)]);
        let Some(Output::PublishReport { group_id, object }) = b.poll(at(0)) else {
            panic!()
        };
        a.on_report(&id_of(2), group_id, &object, at(100));
        let mut both = vec![id_of(1), id_of(2)];
        both.sort();
        assert_eq!(a.present(at(100)), both);
        assert_eq!(a.present(at(3099)), both);
        assert_eq!(a.present(at(3100)), [id_of(1)]);
        // A fresh report brings it back.
        let Some(Output::PublishReport { group_id, object }) = b.poll(at(4000)) else {
            panic!()
        };
        a.on_report(&id_of(2), group_id, &object, at(4010));
        assert_eq!(a.present(at(4010)), both);
    }

    // --- The conductor ---

    fn stamp(term: u32, generation: u32) -> PlanStamp {
        PlanStamp { term, generation }
    }

    /// The plan `output` publishes, checked as any other node would check it.
    fn issued(output: Option<Output>) -> Plan {
        let Some(Output::PublishPlan { group_id, object }) = output else {
            panic!("no plan: {output:?}")
        };
        Plan::open(&meeting(), group_id, &object, &FakeVerifier).unwrap()
    }

    /// Run `node` until it has nothing to send, returning what it sent.
    fn drain(node: &mut Node<FakeIdentity, FakeVerifier>, now: Timestamp) -> Vec<Output> {
        core::iter::from_fn(|| node.poll(now)).collect()
    }

    /// Deliver `from`'s report, if it has one to send, to `to`.
    fn report(
        from: &mut Node<FakeIdentity, FakeVerifier>,
        to: &mut Node<FakeIdentity, FakeVerifier>,
        now: Timestamp,
    ) {
        for output in drain(from, now) {
            if let Output::PublishReport { group_id, object } = output {
                let sender = from.me().clone();
                assert_eq!(
                    to.on_report(&sender, group_id, &object, now),
                    ReportVerdict::Accepted
                );
            }
        }
    }

    fn roster_of(plan: &Plan) -> Vec<ParticipantId> {
        plan.roster
            .iter()
            .map(|m| FakeVerifier.participant_id(&m.key))
            .collect()
    }

    fn sorted(mut ids: Vec<ParticipantId>) -> Vec<ParticipantId> {
        ids.sort();
        ids
    }

    fn edge(a: u8, b: u8) -> Edge {
        let (a, b) = (id_of(a), id_of(b));
        if a < b { Edge(a, b) } else { Edge(b, a) }
    }

    #[test]
    fn the_creator_issues_a_first_plan_with_a_roster_of_one() {
        let mut c = node(1);
        assert!(!c.is_conductor());
        assert!(c.create_meeting());
        assert!(c.is_conductor());
        let plan = issued(c.poll(at(0)));
        assert_eq!(plan.stamp, stamp(FIRST_TERM, 0));
        assert_eq!(plan.conductor, id_of(1));
        assert_eq!(roster_of(&plan), [id_of(1)]);
        assert_eq!(plan.roster[0].name.reveal(), "node 1");
        assert_eq!(plan.audio_top_n, AUDIO_TOP_N);
        assert!(plan.peerings.is_empty() && plan.audio_overlay.is_empty());
        assert_eq!(c.plan(), Some(&plan), "the conductor follows its own plan");
        // Then its report, which names the plan; then nothing.
        assert_eq!(opened(c.poll(at(0)), 1).applied_plan, Some(plan.stamp));
        assert_eq!(c.poll(at(0)), None);
    }

    #[test]
    fn a_node_that_is_not_conductor_never_issues_a_plan() {
        let (mut a, mut b) = (node(1), node(2));
        report(&mut b, &mut a, at(0));
        for output in drain(&mut a, at(0)) {
            assert!(matches!(output, Output::PublishReport { .. }));
        }
        assert_eq!(a.plan(), None);
    }

    #[test]
    fn a_node_whose_report_arrives_joins_the_roster_and_is_wired() {
        let (mut c, mut j) = (node(1), node(2));
        c.create_meeting();
        drain(&mut c, at(0));
        report(&mut j, &mut c, at(100));
        assert_eq!(c.next_wake(at(100)), at(100));
        let plan = issued(c.poll(at(100)));
        assert_eq!(plan.stamp, stamp(FIRST_TERM, 1));
        assert_eq!(sorted(roster_of(&plan)), sorted(vec![id_of(1), id_of(2)]));
        assert_eq!(plan.peerings, [edge(1, 2)]);
        assert_eq!(plan.audio_overlay, [edge(1, 2)]);

        // The joiner adopts it and says so in its next report.
        let group = plan.stamp.group_id();
        let object = plan.seal(&meeting(), &FakeIdentity(1));
        assert_eq!(j.on_plan(group, &object), PlanVerdict::Adopted);
        assert_eq!(opened(j.poll(at(300)), 2).applied_plan, Some(plan.stamp));
        // Its own plan coming back round the flood changes nothing for the conductor.
        assert_eq!(c.on_plan(group, &object), PlanVerdict::Duplicate);
        assert!(c.is_conductor());
    }

    #[test]
    fn no_plan_is_issued_while_nothing_it_says_changes() {
        let (mut c, mut j) = (node(1), node(2));
        c.create_meeting();
        report(&mut j, &mut c, at(0));
        let first = issued(c.poll(at(0)));
        drain(&mut c, at(0));
        // Reports keep coming, with a round-trip time that moves about.
        for second in 1..10u64 {
            let now = at(second * 1000);
            j.set_view(LocalView {
                links: vec![Link {
                    peer: id_of(1),
                    path: PathKind::Direct,
                    rtt: Duration::from_millis(second),
                }],
                ..LocalView::default()
            });
            report(&mut j, &mut c, now);
            for output in drain(&mut c, now) {
                assert!(matches!(output, Output::PublishReport { .. }));
            }
        }
        assert_eq!(c.plan(), Some(&first));
    }

    #[test]
    fn a_member_whose_reports_stop_is_dropped_after_three_seconds() {
        let (mut c, mut j, mut k) = (node(1), node(2), node(3));
        c.create_meeting();
        report(&mut j, &mut c, at(0));
        report(&mut k, &mut c, at(0));
        let plan = issued(c.poll(at(0)));
        assert_eq!(roster_of(&plan).len(), 3);
        drain(&mut c, at(0));

        // 3 keeps reporting; 2 goes quiet. The conductor asks to be woken when 2 is due out.
        for ms in [1000, 2000] {
            report(&mut k, &mut c, at(ms));
            drain(&mut c, at(ms));
        }
        assert_eq!(c.next_wake(at(2000)), at(3000), "its own next report");
        report(&mut k, &mut c, at(2999));
        assert!(drain(&mut c, at(2999)).is_empty());
        assert_eq!(c.next_wake(at(2999)), at(3000));

        let plan = issued(c.poll(at(3000)));
        assert_eq!(sorted(roster_of(&plan)), sorted(vec![id_of(1), id_of(3)]));
        assert_eq!(plan.audio_overlay, [edge(1, 3)]);
        assert!(plan.peerings.iter().all(|e| *e == edge(1, 3)));
    }

    #[test]
    fn the_conductor_is_woken_for_a_timeout_that_falls_before_its_next_report() {
        let (mut c, mut j) = (node(1), node(2));
        c.create_meeting();
        drain(&mut c, at(0));
        report(&mut j, &mut c, at(200));
        drain(&mut c, at(200)); // plan, and a report naming it
        drain(&mut c, at(3000)); // a report; the next is due at 4000
        assert_eq!(c.next_wake(at(3000)), at(3200));
        assert_eq!(roster_of(&issued(c.poll(at(3200)))), [id_of(1)]);
    }

    #[test]
    fn a_node_that_comes_back_is_added_again() {
        let (mut c, mut j) = (node(1), node(2));
        c.create_meeting();
        report(&mut j, &mut c, at(0));
        drain(&mut c, at(0));
        assert_eq!(roster_of(&issued(c.poll(at(3000)))), [id_of(1)]);
        drain(&mut c, at(3000));
        report(&mut j, &mut c, at(5000));
        let plan = issued(c.poll(at(5000)));
        assert_eq!(roster_of(&plan).len(), 2);
        assert_eq!(plan.stamp, stamp(FIRST_TERM, 2));
    }

    #[test]
    fn a_changed_name_is_a_new_plan() {
        let (mut c, mut j) = (node(1), node(2));
        c.create_meeting();
        report(&mut j, &mut c, at(0));
        drain(&mut c, at(0));
        let config = Config {
            meeting: meeting(),
            name: Redacted::new("Bob".to_owned()),
            first_sequence: 2000,
        };
        let mut renamed = Node::new(config, FakeIdentity(2), FakeVerifier);
        report(&mut renamed, &mut c, at(500));
        let plan = issued(c.poll(at(500)));
        assert!(plan.roster.iter().any(|m| m.name.reveal() == "Bob"));
    }

    #[test]
    fn links_and_unreachable_pairs_from_reports_shape_the_wiring() {
        // 1 conducts. 2 and 3 are connected to each other; 3 cannot reach 1.
        let (mut c, mut j, mut k) = (node(1), node(2), node(3));
        c.create_meeting();
        let direct = |peer: u8| Link {
            peer: id_of(peer),
            path: PathKind::Direct,
            rtt: Duration::from_millis(2),
        };
        c.set_view(LocalView {
            links: vec![direct(2)],
            ..LocalView::default()
        });
        j.set_view(LocalView {
            links: vec![direct(1), direct(3)],
            ..LocalView::default()
        });
        k.set_view(LocalView {
            links: vec![direct(2)],
            unreachable: vec![id_of(1)],
            ..LocalView::default()
        });
        report(&mut j, &mut c, at(0));
        report(&mut k, &mut c, at(0));
        let plan = issued(c.poll(at(0)));
        let mut line = vec![edge(1, 2), edge(2, 3)];
        line.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        assert_eq!(plan.audio_overlay, line);
        assert_eq!(plan.peerings, line);
    }

    #[test]
    fn a_change_in_the_conductors_own_links_is_planned_for() {
        let (mut c, mut j, mut k) = (node(1), node(2), node(3));
        c.create_meeting();
        report(&mut j, &mut c, at(0));
        report(&mut k, &mut c, at(0));
        let before = issued(c.poll(at(0)));
        assert!(before.audio_overlay.contains(&edge(1, 3)));
        drain(&mut c, at(0));
        c.set_view(LocalView {
            unreachable: vec![id_of(3)],
            ..LocalView::default()
        });
        assert_eq!(c.next_wake(at(10)), at(10));
        let after = issued(c.poll(at(10)));
        assert_eq!(after.stamp, stamp(FIRST_TERM, 1));
        assert!(!after.audio_overlay.contains(&edge(1, 3)));
        assert!(after.audio_overlay.contains(&edge(2, 3)));
    }

    #[test]
    fn existing_wiring_is_kept_as_nodes_join() {
        let mut c = node(1);
        c.create_meeting();
        let mut others: Vec<_> = (2..=12).map(node).collect();
        let mut held = issued(c.poll(at(0)));
        for (i, joiner) in others.iter_mut().enumerate() {
            let now = at(100 * (i as u64 + 1));
            report(joiner, &mut c, now);
            let next = issued(c.poll(now));
            assert!(held.may_replace(&next));
            for e in &held.audio_overlay {
                assert!(next.audio_overlay.contains(e));
            }
            assert_eq!(next.audio_overlay.len(), held.audio_overlay.len() + 1);
            held = next;
        }
        assert_eq!(held.roster.len(), 12);
    }

    #[test]
    fn a_node_that_has_a_plan_cannot_create_a_meeting() {
        let mut n = node(3);
        let (group, envelope) = plan(1, 0, 1, &[], 3);
        assert_eq!(n.on_plan(group, &envelope), PlanVerdict::Adopted);
        assert!(!n.create_meeting());
        assert!(!n.is_conductor());
    }

    #[test]
    fn a_creator_that_adopts_another_plan_stops_conducting() {
        // It created a meeting but, before issuing anything, received the plan of one that
        // already exists.
        let mut n = node(3);
        n.create_meeting();
        let (group, envelope) = plan(1, 4, 1, &[], 3);
        assert_eq!(n.on_plan(group, &envelope), PlanVerdict::Adopted);
        assert!(!n.is_conductor());
        for output in drain(&mut n, at(0)) {
            assert!(matches!(output, Output::PublishReport { .. }));
        }
    }

    #[test]
    fn a_plan_from_another_node_does_not_unseat_a_conductor_with_a_plan() {
        let mut c = node(1);
        c.create_meeting();
        drain(&mut c, at(0));
        let (group, envelope) = plan(1, 5, 2, &[], 2);
        assert_eq!(c.on_plan(group, &envelope), PlanVerdict::NotAllowed);
        let (group, envelope) = plan(2, 0, 2, &[], 2);
        assert_eq!(c.on_plan(group, &envelope), PlanVerdict::NotAllowed);
        assert!(c.is_conductor());
    }

    #[test]
    fn a_copy_of_the_plan_held_or_an_older_one_is_settled_without_checking_it() {
        let mut n = node(3);
        let (group, envelope) = plan(1, 5, 1, &[2], 3);
        assert_eq!(n.on_plan(group, &envelope), PlanVerdict::Adopted);
        // Garbage under the same group id, and under an older one: dropped, nothing changes.
        assert_eq!(n.on_plan(group, b"not a plan"), PlanVerdict::Duplicate);
        assert_eq!(n.on_plan(group - 1, b"not a plan"), PlanVerdict::NotAllowed);
        // Under a newer one it is checked, and refused.
        assert!(matches!(
            n.on_plan(group + 1, b"not a plan"),
            PlanVerdict::Invalid(_)
        ));
        assert_eq!(n.plan().map(|p| p.stamp.group_id()), Some(group));
    }

    #[test]
    fn the_conductor_replans_on_a_report_that_matters_and_not_on_a_repeat() {
        let (mut c, mut j, mut k) = (node(1), node(2), node(3));
        c.create_meeting();
        report(&mut j, &mut c, at(0));
        report(&mut k, &mut c, at(0));
        let before = issued(c.poll(at(0)));
        drain(&mut c, at(0));
        // A repeat: no reason to look at the plan, so no wake before the next report.
        report(&mut k, &mut c, at(500));
        assert_eq!(c.next_wake(at(500)), at(1000));
        // 3 now says it cannot reach 1.
        k.set_view(LocalView {
            unreachable: vec![id_of(1)],
            ..LocalView::default()
        });
        report(&mut k, &mut c, at(600));
        assert_eq!(c.next_wake(at(600)), at(600));
        let after = issued(c.poll(at(600)));
        assert!(before.audio_overlay.contains(&edge(1, 3)));
        assert!(!after.audio_overlay.contains(&edge(1, 3)));
    }

    #[test]
    fn several_changes_inside_the_gap_share_one_report() {
        let mut n = node(3);
        n.poll(at(0));
        for (ms, generation) in [(20, 0), (60, 1), (100, 2)] {
            let (group, envelope) = plan(1, generation, 1, &[], 3);
            assert_eq!(n.on_plan(group, &envelope), PlanVerdict::Adopted);
            assert_eq!(n.poll(at(ms)), None);
        }
        assert_eq!(opened(n.poll(at(200)), 3).applied_plan, Some(stamp(1, 2)));
        assert_eq!(n.poll(at(200)), None);
    }
}
