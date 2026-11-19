//! What every node does on the control plane, conductor or not:
//!
//! - publishes its own signed report once a second, and at once when something in it changes;
//! - keeps the latest verified report from every other node;
//! - holds the current plan, adopting a new one only under the replacement rule.
//!
//! Sans-IO: the caller hands in received objects and the current time, and asks what to send.

use palaver_proto::{
    DecodeError, Link, MeetingTag, Micros, ParticipantId, Plan, Redacted, Report, Signer, Verifier,
};
use std::collections::BTreeMap;
use std::net::SocketAddr;

/// How often a node reports when nothing has changed. Also the conductor's heartbeat.
pub const REPORT_INTERVAL: Micros = 1_000_000;
/// A node whose last report is older than this is no longer considered present.
pub const REPORT_TIMEOUT: Micros = 3_000_000;

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
    at: Micros,
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
    last_sent: Option<Micros>,
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
        }
    }

    /// An object arrived on the plan track in group `group_id`.
    pub fn on_plan(&mut self, group_id: u64, envelope: &[u8]) -> PlanVerdict {
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
        now: Micros,
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
                self.reports
                    .insert(participant.clone(), Heard { report, at: now });
                ReportVerdict::Accepted
            }
            Err(e) => ReportVerdict::Invalid(e),
        }
    }

    /// Ask what to send. Returns a report when one is due: the first time, when something in
    /// it changed, or a second after the last. Call again until it returns `None`.
    pub fn poll(&mut self, now: Micros) -> Option<Output> {
        let due = self
            .last_sent
            .is_none_or(|sent| now.saturating_sub(sent) >= REPORT_INTERVAL);
        if !self.changed && !due {
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
    pub fn next_wake(&self, now: Micros) -> Micros {
        match self.last_sent {
            Some(sent) if !self.changed => sent.saturating_add(REPORT_INTERVAL),
            _ => now,
        }
    }

    /// The nodes considered present at `now`: this one, and every node whose latest report
    /// arrived less than [`REPORT_TIMEOUT`] ago.
    pub fn present(&self, now: Micros) -> Vec<ParticipantId> {
        let mut out = vec![self.me.clone()];
        out.extend(
            self.reports
                .iter()
                .filter(|(_, heard)| now.saturating_sub(heard.at) < REPORT_TIMEOUT)
                .map(|(id, _)| id.clone()),
        );
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use palaver_proto::sign::fake::{FakeIdentity, FakeVerifier};
    use palaver_proto::{Edge, Member, PathKind, PlanStamp};

    const MS: Micros = 1000;

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
        assert_eq!(n.next_wake(0), 0);
        let first = opened(n.poll(0), 1);
        assert_eq!((first.sequence, first.applied_plan), (1000, None));
        assert_eq!(n.poll(0), None);
        assert_eq!(n.poll(999 * MS), None);
        assert_eq!(n.next_wake(500 * MS), 1000 * MS);
        assert_eq!(opened(n.poll(1000 * MS), 1).sequence, 1001);
        assert_eq!(n.poll(1500 * MS), None);
        assert_eq!(opened(n.poll(2100 * MS), 1).sequence, 1002);
    }

    #[test]
    fn a_change_is_reported_at_once_and_an_unchanged_view_is_not() {
        let mut n = node(1);
        n.poll(0);
        let view = LocalView {
            links: vec![Link {
                peer: id_of(2),
                path: PathKind::Direct,
                rtt_ms: 3,
            }],
            addresses: vec![Redacted::new("192.168.1.5:4433".parse().unwrap())],
            ..LocalView::default()
        };
        n.set_view(view.clone());
        assert_eq!(n.next_wake(10 * MS), 10 * MS);
        let report = opened(n.poll(10 * MS), 1);
        assert_eq!(report.links, view.links);
        assert_eq!(report.addresses, view.addresses);
        n.set_view(view);
        assert_eq!(n.poll(20 * MS), None);
        // The one-second cadence now runs from the report just sent.
        assert_eq!(n.poll(1000 * MS), None);
        assert!(n.poll(1010 * MS).is_some());
    }

    #[test]
    fn a_joiner_adopts_the_first_verified_plan_and_reports_it_at_once() {
        let mut n = node(3);
        n.poll(0);
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
            opened(n.poll(5 * MS), 3).applied_plan,
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
        let Some(Output::PublishReport { group_id, object }) = b.poll(0) else {
            panic!()
        };
        assert_eq!(
            a.on_report(&id_of(2), group_id, &object, 5 * MS),
            ReportVerdict::Accepted
        );
        assert_eq!(a.report_of(&id_of(2)).map(|r| r.sequence), Some(1000));
        // The same report again, as a flood delivers it over a second path.
        assert_eq!(
            a.on_report(&id_of(2), group_id, &object, 6 * MS),
            ReportVerdict::Stale
        );
        // Presented on someone else's track.
        assert!(matches!(
            a.on_report(&id_of(3), group_id, &object, 6 * MS),
            ReportVerdict::Invalid(_)
        ));
        // The next one replaces it.
        let Some(Output::PublishReport { group_id, object }) = b.poll(1000 * MS) else {
            panic!()
        };
        assert_eq!(
            a.on_report(&id_of(2), group_id, &object, 1005 * MS),
            ReportVerdict::Accepted
        );
        assert_eq!(a.report_of(&id_of(2)).map(|r| r.sequence), Some(1001));
        // A node's own report coming back is ignored.
        let Some(Output::PublishReport { group_id, object }) = a.poll(0) else {
            panic!()
        };
        assert_eq!(
            a.on_report(&id_of(1), group_id, &object, 0),
            ReportVerdict::Stale
        );
    }

    #[test]
    fn a_node_is_present_until_three_seconds_pass_without_a_report() {
        let mut a = node(1);
        let mut b = node(2);
        assert_eq!(a.present(0), [id_of(1)]);
        let Some(Output::PublishReport { group_id, object }) = b.poll(0) else {
            panic!()
        };
        a.on_report(&id_of(2), group_id, &object, 100 * MS);
        let mut both = vec![id_of(1), id_of(2)];
        both.sort();
        assert_eq!(a.present(100 * MS), both);
        assert_eq!(a.present(3099 * MS), both);
        assert_eq!(a.present(3100 * MS), [id_of(1)]);
        // A fresh report brings it back.
        let Some(Output::PublishReport { group_id, object }) = b.poll(4000 * MS) else {
            panic!()
        };
        a.on_report(&id_of(2), group_id, &object, 4010 * MS);
        assert_eq!(a.present(4010 * MS), both);
    }
}
