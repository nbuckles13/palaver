//! What every node does on the control plane, conductor or not:
//!
//! - publishes its own signed report when something in it changes (but never two within
//!   [`REPORT_MIN_GAP`]), and every [`REPORT_REFRESH`] regardless;
//! - keeps the latest verified report from every other node;
//! - holds the current plan, adopting a new one only under the replacement rule.
//!
//! And what the conductor does on top: it keeps the roster and issues a new plan whenever
//! the roster or the wiring it calls for changes.
//!
//! Who is in the roster is read from the links nodes report, not from a heartbeat. Every
//! node watches its own links (any traffic shows the neighbour is alive) and reports a
//! change at once. A link counts as up while both its ends list it. A node is in the roster
//! while a chain of such links joins it to the conductor. So a node that dies is dropped as
//! soon as its neighbours notice and say so, and no node has to send anything periodically
//! for the others to know it is there.
//!
//! Sans-IO: the caller hands in received objects and the current time, and asks what to send.

use palaver_proto::{
    AssignedTrack, DecodeError, Edge, Link, MediaTrack, MeetingTag, Member, ParticipantId, Plan,
    PlanStamp, Redacted, Report, Signer, Timestamp, Verifier, WIDE_ALIAS_START,
};
use palaver_topology::{Input, ReportedLink, wire};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::time::Duration;

/// How often a node repeats its report when nothing has changed. A safety net only: changes
/// are reported at once, and whether a node is still there is judged from its links, which
/// its neighbours watch, not from this.
pub const REPORT_REFRESH: Duration = Duration::from_secs(30);
/// The least time between two reports from a node. A change is reported at once unless a
/// report went out less than this long ago; then it waits, and several changes share one
/// report. Without it every plan makes every node report at once, and each of those reports
/// floods to every node.
pub const REPORT_MIN_GAP: Duration = Duration::from_millis(200);
/// A report older than this is not believed any more: its node missed three refreshes. This
/// catches a node whose connections are alive but which has stopped working.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(100);
/// How long the conductor keeps a node in the roster after it stops being reachable, so
/// that a link which drops and comes straight back, or a node moving from one link to
/// another, does not cost two plans.
pub const DROP_HOLD: Duration = Duration::from_millis(500);
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
    /// Every media track this node can send, whether or not it is sending it now.
    pub tracks: Vec<MediaTrack>,
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
/// identity, the same tracks on offer, the same links by the same kind of path, the same
/// unreachable nodes. Round-trip
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
        && old.tracks == new.tracks
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
    /// Nodes that were reachable when the conductor last looked.
    connected: BTreeSet<ParticipantId>,
    /// Of those, the ones that no longer are, and since when.
    lost: BTreeMap<ParticipantId, Timestamp>,
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
            connected: BTreeSet::new(),
            lost: BTreeMap::new(),
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

    /// Make `next` the plan this node follows. The report names the plan applied; it is sent
    /// at once only if the plan asks something new of this node, which is when the conductor
    /// is waiting to hear. Otherwise the news rides on the next regular report: if every
    /// node reported every plan at once, each plan would set off a report from everyone,
    /// flooded to everyone.
    fn adopt(&mut self, next: Plan) {
        let asks_something_new = self
            .plan
            .as_ref()
            .is_none_or(|held| next.changes_for(held, &self.me));
        if asks_something_new {
            self.changed = true;
        }
        self.plan = Some(next);
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

    /// The addresses other nodes say they see `participant` at, each once, in the order
    /// of the ids of those who say so.
    pub fn observed_addresses_of(&self, participant: &ParticipantId) -> Vec<SocketAddr> {
        let mut seen = Vec::new();
        for heard in self.reports.values() {
            for link in &heard.report.links {
                if let Some(observed) = &link.observed
                    && link.peer == *participant
                    && !seen.contains(observed.reveal())
                {
                    seen.push(*observed.reveal());
                }
            }
        }
        seen
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
        self.adopt(next);
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

    /// The reports that are still believed at `now`.
    fn fresh(&self, now: Timestamp) -> impl Iterator<Item = (&ParticipantId, &Report)> {
        self.reports
            .iter()
            .filter(move |(_, heard)| now.saturating_since(heard.at) < REPORT_TIMEOUT)
            .map(|(id, heard)| (id, &heard.report))
    }

    /// The nodes joined to this one by a chain of links that both ends report as up.
    fn reachable(&self, now: Timestamp) -> BTreeSet<ParticipantId> {
        let reports: BTreeMap<&ParticipantId, &Report> = self.fresh(now).collect();
        let lists = |links: &[Link], peer: &ParticipantId| links.iter().any(|l| l.peer == *peer);
        // The peers `node` lists, which list it back.
        let neighbours = |node: &ParticipantId| -> Vec<ParticipantId> {
            let mine: &[Link] = if *node == self.me {
                &self.view.links
            } else {
                reports.get(node).map(|r| r.links.as_slice()).unwrap_or(&[])
            };
            mine.iter()
                .map(|link| &link.peer)
                .filter(|peer| {
                    if **peer == self.me {
                        lists(&self.view.links, node)
                    } else {
                        reports.get(peer).is_some_and(|r| lists(&r.links, node))
                    }
                })
                .cloned()
                .collect()
        };
        let mut seen = BTreeSet::from([self.me.clone()]);
        let mut todo = vec![self.me.clone()];
        while let Some(node) = todo.pop() {
            for peer in neighbours(&node) {
                if seen.insert(peer.clone()) {
                    todo.push(peer);
                }
            }
        }
        seen.remove(&self.me);
        seen
    }

    /// Bring the record of who is connected, and who was lost when, up to `now`.
    fn note_reachability(&mut self, now: Timestamp) {
        let reachable = self.reachable(now);
        for id in &self.connected {
            if !reachable.contains(id) {
                self.lost.entry(id.clone()).or_insert(now);
            }
        }
        for id in reachable {
            self.lost.remove(&id);
            self.connected.insert(id);
        }
        let gone: Vec<ParticipantId> = self
            .lost
            .iter()
            .filter(|(_, since)| now.saturating_since(**since) >= DROP_HOLD)
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            self.lost.remove(&id);
            self.connected.remove(&id);
        }
    }

    /// The plan the conductor would issue at `now`, with the stamp left for the caller: the
    /// roster is every node present, wired by the topology rule starting from the plan held.
    fn draft(&self, term: u32, now: Timestamp) -> Plan {
        let mut roster = vec![(
            self.me.clone(),
            Member {
                key: self.signer.public_key(),
                name: self.config.name.clone(),
                tracks: Vec::new(),
            },
            self.view.tracks.clone(),
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
        let present = self.present(now);
        for (id, report) in self.fresh(now) {
            if !present.contains(id) {
                continue;
            }
            see(id, &report.links, &report.unreachable);
            roster.push((
                id.clone(),
                Member {
                    key: report.key,
                    name: report.name.clone(),
                    tracks: Vec::new(),
                },
                report.tracks.clone(),
            ));
        }
        // A link is up only while both ends list it; one end's word is a link that is coming
        // up or has just gone.
        let listed: BTreeSet<(ParticipantId, ParticipantId)> = links
            .iter()
            .map(|link| (link.from.clone(), link.to.clone()))
            .collect();
        links.retain(|link| listed.contains(&(link.to.clone(), link.from.clone())));
        // A node that has just dropped out of reach is kept as it was until its hold runs
        // out: its neighbours failing to reach it is the same news again, not a reason to
        // wire around it.
        unreachable
            .retain(|pair| !self.lost.contains_key(&pair.0) && !self.lost.contains_key(&pair.1));
        roster.sort_by(|a, b| a.0.cmp(&b.0));
        // Every track on offer gets an alias. A track keeps the one it has; a new one takes
        // the next: a small alias for audio, whose packets are small, a wide one otherwise.
        // An alias is never given twice, even after its track or its member has gone.
        let held_tracks: BTreeMap<ParticipantId, &[AssignedTrack]> = self
            .plan
            .iter()
            .flat_map(|held| &held.roster)
            .map(|member| {
                (
                    self.verifier.participant_id(&member.key),
                    member.tracks.as_slice(),
                )
            })
            .collect();
        let (mut next_small_alias, mut next_wide_alias) = match &self.plan {
            Some(held) => (held.next_small_alias, held.next_wide_alias),
            None => (0, WIDE_ALIAS_START),
        };
        for (id, member, offered) in &mut roster {
            let held = held_tracks.get(id).copied().unwrap_or(&[]);
            for track in offered.iter() {
                if member
                    .tracks
                    .iter()
                    .any(|assigned| assigned.track == *track)
                {
                    continue; // offered twice
                }
                let alias = match held.iter().find(|assigned| assigned.track == *track) {
                    Some(assigned) => assigned.alias,
                    None => {
                        // When the small ones run out, audio takes wide ones too.
                        let small = track.audio && next_small_alias < WIDE_ALIAS_START;
                        let next = if small {
                            &mut next_small_alias
                        } else {
                            &mut next_wide_alias
                        };
                        let alias = *next;
                        *next = next.saturating_add(1);
                        alias
                    }
                };
                member.tracks.push(AssignedTrack {
                    track: track.clone(),
                    alias,
                });
            }
        }
        let (previous_peerings, previous_audio_overlay) = match &self.plan {
            Some(held) => (held.peerings.clone(), held.audio_overlay.clone()),
            None => (Vec::new(), Vec::new()),
        };
        let wiring = wire(&Input {
            members: roster.iter().map(|(id, _, _)| id.clone()).collect(),
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
            roster: roster.into_iter().map(|(_, member, _)| member).collect(),
            peerings: wiring.peerings,
            audio_overlay: wiring.audio_overlay,
            audio_top_n: AUDIO_TOP_N,
            next_small_alias,
            next_wide_alias,
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
        self.conducting.as_ref()?;
        self.note_reachability(now);
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
        self.adopt(next);
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
            REPORT_REFRESH
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
            tracks: self.view.tracks.clone(),
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
            Some(sent) => sent.saturating_add(REPORT_REFRESH),
            None => now,
        };
        let Some(conducting) = &self.conducting else {
            return report;
        };
        if conducting.stale {
            return now;
        }
        // The conductor must also notice when a lost node's hold runs out, and when a report
        // stops being believed.
        let holds = self
            .lost
            .values()
            .map(|since| since.saturating_add(DROP_HOLD));
        let reports = self
            .reports
            .values()
            .map(|heard| heard.at.saturating_add(REPORT_TIMEOUT));
        holds
            .chain(reports)
            .filter(|expiry| *expiry > now)
            .fold(report, Timestamp::min)
    }

    /// The nodes in the meeting at `now`, as this node sees it: itself, every node joined to
    /// it by links that both ends report as up, and (on the conductor) nodes that stopped
    /// being reachable less than [`DROP_HOLD`] ago. Sorted.
    pub fn present(&self, now: Timestamp) -> Vec<ParticipantId> {
        let mut out = self.reachable(now);
        let held = self
            .lost
            .iter()
            .filter(|(id, since)| {
                now.saturating_since(**since) < DROP_HOLD
                    && self
                        .reports
                        .get(*id)
                        .is_some_and(|heard| now.saturating_since(heard.at) < REPORT_TIMEOUT)
            })
            .map(|(id, _)| id.clone());
        out.extend(held);
        out.insert(self.me.clone());
        out.into_iter().collect()
    }

    /// This node's links and addresses, as last set.
    pub fn view(&self) -> &LocalView {
        &self.view
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
                tracks: vec![],
            })
            .collect();
        let plan = Plan {
            stamp: PlanStamp { term, generation },
            conductor: id_of(conductor),
            roster,
            peerings: vec![Edge(id_of(1), id_of(2))],
            audio_overlay: vec![Edge(id_of(1), id_of(2))],
            audio_top_n: 3,
            next_small_alias: 0,
            next_wide_alias: WIDE_ALIAS_START,
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
    fn a_node_reports_at_once_then_only_every_thirty_seconds_while_nothing_changes() {
        let mut n = node(1);
        assert_eq!(n.next_wake(at(0)), at(0));
        let first = opened(n.poll(at(0)), 1);
        assert_eq!((first.sequence, first.applied_plan), (1000, None));
        assert_eq!(n.poll(at(0)), None);
        assert_eq!(n.poll(at(29_999)), None);
        assert_eq!(n.next_wake(at(500)), at(30_000));
        assert_eq!(opened(n.poll(at(30_000)), 1).sequence, 1001);
        assert_eq!(n.poll(at(45_000)), None);
        assert_eq!(opened(n.poll(at(61_000)), 1).sequence, 1002);
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
                observed: None,
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
        // The refresh now runs from the report just sent.
        assert_eq!(n.poll(at(30_199)), None);
        assert!(n.poll(at(30_200)).is_some());
        // Once the gap has passed, a change is reported at once.
        n.set_view(LocalView::default());
        assert_eq!(n.next_wake(at(31_000)), at(31_000));
        assert!(n.poll(at(31_000)).is_some());
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
        let Some(Output::PublishReport { group_id, object }) = b.poll(at(30_000)) else {
            panic!()
        };
        assert_eq!(
            a.on_report(&id_of(2), group_id, &object, at(30_005)),
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

    // --- Who is present, and the conductor ---

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

    /// The plans among what `node` sends at `now`.
    fn plans(node: &mut Node<FakeIdentity, FakeVerifier>, now: Timestamp) -> Vec<Plan> {
        drain(node, now)
            .into_iter()
            .filter(|output| matches!(output, Output::PublishPlan { .. }))
            .map(|output| issued(Some(output)))
            .collect()
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

    fn direct(peer: u8) -> Link {
        Link {
            peer: id_of(peer),
            path: PathKind::Direct,
            rtt: Duration::from_millis(2),
            observed: None,
        }
    }

    /// Tell `node` that its links are now exactly those to `peers`.
    fn links(node: &mut Node<FakeIdentity, FakeVerifier>, peers: &[u8]) {
        let view = LocalView {
            links: peers.iter().map(|peer| direct(*peer)).collect(),
            ..node.view().clone()
        };
        node.set_view(view);
    }

    fn roster_of(plan: &Plan) -> Vec<ParticipantId> {
        sorted(
            plan.roster
                .iter()
                .map(|m| FakeVerifier.participant_id(&m.key))
                .collect(),
        )
    }

    fn ids(seeds: &[u8]) -> Vec<ParticipantId> {
        sorted(seeds.iter().map(|seed| id_of(*seed)).collect())
    }

    fn sorted(mut ids: Vec<ParticipantId>) -> Vec<ParticipantId> {
        ids.sort();
        ids
    }

    fn edge(a: u8, b: u8) -> Edge {
        let (a, b) = (id_of(a), id_of(b));
        if a < b { Edge(a, b) } else { Edge(b, a) }
    }

    /// A conductor (node 1) with nodes 2 and 3 linked to it, the plan issued and everything
    /// sent, at time 0.
    fn three() -> [Node<FakeIdentity, FakeVerifier>; 3] {
        let (mut c, mut j, mut k) = (node(1), node(2), node(3));
        c.create_meeting();
        links(&mut c, &[2, 3]);
        links(&mut j, &[1]);
        links(&mut k, &[1]);
        report(&mut j, &mut c, at(0));
        report(&mut k, &mut c, at(0));
        drain(&mut c, at(0));
        assert_eq!(roster_of(c.plan().unwrap()), ids(&[1, 2, 3]));
        [c, j, k]
    }

    #[test]
    fn a_node_is_present_only_while_both_ends_of_a_link_to_it_say_so() {
        let (mut a, mut b) = (node(1), node(2));
        assert_eq!(a.present(at(0)), ids(&[1]));
        // B's report alone, even naming A, is not enough: A does not hold the link.
        links(&mut b, &[1]);
        report(&mut b, &mut a, at(0));
        assert_eq!(a.present(at(0)), ids(&[1]));
        links(&mut a, &[2]);
        assert_eq!(a.present(at(0)), ids(&[1, 2]));
        // Nor is A's side alone: B says the link is gone.
        links(&mut b, &[]);
        report(&mut b, &mut a, at(300));
        assert_eq!(a.present(at(300)), ids(&[1]));
    }

    #[test]
    fn a_node_is_present_through_a_chain_of_links() {
        // 1 - 2 - 3 - 4: node 1 holds only the link to 2.
        let mut a = node(1);
        links(&mut a, &[2]);
        let (mut b, mut c, mut d) = (node(2), node(3), node(4));
        links(&mut b, &[1, 3]);
        links(&mut c, &[2, 4]);
        links(&mut d, &[3]);
        for other in [&mut d, &mut c] {
            report(other, &mut a, at(0));
        }
        // Without 2's report the chain has no first link.
        assert_eq!(a.present(at(0)), ids(&[1]));
        report(&mut b, &mut a, at(0));
        assert_eq!(a.present(at(0)), ids(&[1, 2, 3, 4]));
        // 3 loses its link to 4: 4 is cut off though its own report still names 3.
        links(&mut c, &[2]);
        report(&mut c, &mut a, at(300));
        assert_eq!(a.present(at(300)), ids(&[1, 2, 3]));
        // 2 goes: everything behind it goes with it.
        links(&mut a, &[]);
        assert_eq!(a.present(at(300)), ids(&[1]));
    }

    #[test]
    fn a_report_is_not_believed_after_a_hundred_seconds_without_a_refresh() {
        let (mut a, mut b) = (node(1), node(2));
        links(&mut a, &[2]);
        links(&mut b, &[1]);
        report(&mut b, &mut a, at(0));
        assert_eq!(a.present(at(99_999)), ids(&[1, 2]));
        assert_eq!(a.present(at(100_000)), ids(&[1]));
        // A refresh keeps it.
        report(&mut b, &mut a, at(30_000));
        assert_eq!(a.present(at(100_000)), ids(&[1, 2]));
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
        assert_eq!(roster_of(&plan), ids(&[1]));
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
        links(&mut a, &[2]);
        links(&mut b, &[1]);
        report(&mut b, &mut a, at(0));
        assert!(plans(&mut a, at(0)).is_empty());
        assert_eq!(a.plan(), None);
    }

    #[test]
    fn a_node_joins_the_roster_once_its_link_is_reported_from_both_ends() {
        let (mut c, mut j) = (node(1), node(2));
        c.create_meeting();
        drain(&mut c, at(0));
        // The joiner's report arrives first, naming its link to the conductor.
        links(&mut j, &[1]);
        report(&mut j, &mut c, at(100));
        assert!(
            plans(&mut c, at(100)).is_empty(),
            "the link is not up here yet"
        );
        // Then the conductor's own end comes up.
        links(&mut c, &[2]);
        assert_eq!(c.next_wake(at(110)), at(110));
        let plan = issued(c.poll(at(110)));
        assert_eq!(plan.stamp, stamp(FIRST_TERM, 1));
        assert_eq!(roster_of(&plan), ids(&[1, 2]));
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
    fn a_node_two_links_away_joins_the_roster() {
        // 3 joined through 2; the conductor, 1, has no link to it.
        let [mut c, mut j, mut k] = three();
        let mut far = node(4);
        links(&mut far, &[3]);
        links(&mut k, &[1, 4]);
        report(&mut far, &mut c, at(300));
        assert!(plans(&mut c, at(300)).is_empty());
        report(&mut k, &mut c, at(310));
        let plan = issued(c.poll(at(310)));
        assert_eq!(roster_of(&plan), ids(&[1, 2, 3, 4]));
        assert!(plan.audio_overlay.contains(&edge(3, 4)));
        let _ = &mut j;
    }

    #[test]
    fn no_plan_is_issued_while_nothing_it_says_changes() {
        let [mut c, mut j, _] = three();
        let first = c.plan().unwrap().clone();
        // Refreshes keep coming, with a round-trip time that moves about.
        for round in 1..10u64 {
            let now = at(round * 30_000);
            j.set_view(LocalView {
                links: vec![Link {
                    rtt: Duration::from_millis(round),
                    observed: None,
                    ..direct(1)
                }],
                ..LocalView::default()
            });
            report(&mut j, &mut c, now);
            // (Node 3 sends nothing in this test, so its report is let go in the end.)
            if round < 3 {
                assert!(plans(&mut c, now).is_empty());
                assert_eq!(c.plan(), Some(&first));
            }
        }
    }

    #[test]
    fn a_node_whose_link_goes_is_dropped_after_the_hold() {
        let [mut c, _, _] = three();
        // The conductor's link to 3 dies at 1000 ms.
        links(&mut c, &[2]);
        assert_eq!(c.next_wake(at(1000)), at(1000));
        assert!(plans(&mut c, at(1000)).is_empty(), "held for now");
        assert_eq!(c.present(at(1000)), ids(&[1, 2, 3]));
        // It asks to be woken when the hold runs out.
        assert_eq!(c.next_wake(at(1200)), at(1500));
        assert!(plans(&mut c, at(1499)).is_empty());
        let plan = issued(c.poll(at(1500)));
        assert_eq!(roster_of(&plan), ids(&[1, 2]));
        assert_eq!(plan.audio_overlay, [edge(1, 2)]);
        assert_eq!(c.present(at(1500)), ids(&[1, 2]));
    }

    #[test]
    fn a_neighbour_failing_to_reach_a_node_in_its_hold_is_not_a_reason_to_rewire() {
        // A triangle. 3 dies: both its links go, and 2 then reports it cannot reach 3.
        let [mut c, mut j, mut k] = three();
        links(&mut j, &[1, 3]);
        links(&mut k, &[1, 2]);
        report(&mut j, &mut c, at(300));
        report(&mut k, &mut c, at(300));
        drain(&mut c, at(300));
        links(&mut c, &[2]);
        links(&mut j, &[1]);
        report(&mut j, &mut c, at(1000));
        assert!(plans(&mut c, at(1000)).is_empty());
        j.set_view(LocalView {
            links: vec![direct(1)],
            unreachable: vec![id_of(3)],
            ..LocalView::default()
        });
        report(&mut j, &mut c, at(1300));
        assert!(
            plans(&mut c, at(1300)).is_empty(),
            "still held, wiring untouched"
        );
        // One plan, when the hold runs out.
        let after = plans(&mut c, at(1500));
        assert_eq!(after.len(), 1);
        assert_eq!(roster_of(&after[0]), ids(&[1, 2]));
    }

    #[test]
    fn a_link_that_comes_straight_back_costs_no_plan() {
        let [mut c, _, _] = three();
        let before = c.plan().unwrap().clone();
        links(&mut c, &[2]);
        assert!(plans(&mut c, at(1000)).is_empty());
        links(&mut c, &[2, 3]);
        assert!(plans(&mut c, at(1300)).is_empty());
        assert!(plans(&mut c, at(5000)).is_empty());
        assert_eq!(c.plan(), Some(&before));
    }

    #[test]
    fn a_node_that_moves_to_another_link_stays_in_the_roster() {
        let [mut c, mut j, mut k] = three();
        // 3 loses its link to the conductor and, 300 ms later, has one to 2 instead.
        links(&mut c, &[2]);
        drain(&mut c, at(1000));
        links(&mut k, &[2]);
        links(&mut j, &[1, 3]);
        report(&mut k, &mut c, at(1300));
        report(&mut j, &mut c, at(1300));
        // 3 never left, and the plan still asks for the link between 1 and 3: nobody has
        // said it cannot be made again.
        assert!(plans(&mut c, at(1300)).is_empty());
        assert!(plans(&mut c, at(5000)).is_empty());
        assert_eq!(c.present(at(5000)), ids(&[1, 2, 3]));
        // Once the conductor finds it cannot, the wiring moves to the link that works.
        c.set_view(LocalView {
            links: vec![direct(2)],
            unreachable: vec![id_of(3)],
            ..LocalView::default()
        });
        let plan = issued(c.poll(at(6000)));
        assert_eq!(roster_of(&plan), ids(&[1, 2, 3]));
        assert_eq!(plan.audio_overlay.len(), 2);
        assert!(plan.audio_overlay.contains(&edge(2, 3)));
    }

    #[test]
    fn a_node_reported_gone_by_its_neighbour_is_dropped_with_all_behind_it() {
        // 1 - 2 - 3 - 4, conductor 1.
        let [mut c, mut j, mut k] = three();
        let mut far = node(4);
        links(&mut c, &[2]);
        links(&mut j, &[1, 3]);
        links(&mut k, &[2, 4]);
        links(&mut far, &[3]);
        for other in [&mut j, &mut k, &mut far] {
            report(other, &mut c, at(1000));
        }
        drain(&mut c, at(1000));
        drain(&mut c, at(2000));
        assert_eq!(roster_of(c.plan().unwrap()), ids(&[1, 2, 3, 4]));
        // 3 dies. Only 2 can say so; 4's last report still names its link to 3.
        links(&mut j, &[1]);
        report(&mut j, &mut c, at(5000));
        drain(&mut c, at(5000));
        let plan = issued(c.poll(at(5500)));
        assert_eq!(roster_of(&plan), ids(&[1, 2]));
    }

    #[test]
    fn a_node_that_comes_back_is_added_again() {
        let [mut c, _, mut k] = three();
        links(&mut c, &[2]);
        drain(&mut c, at(1000));
        assert_eq!(roster_of(&issued(c.poll(at(1500)))), ids(&[1, 2]));
        drain(&mut c, at(1500));
        links(&mut c, &[2, 3]);
        let plan = issued(c.poll(at(9000)));
        assert_eq!(roster_of(&plan), ids(&[1, 2, 3]));
        assert_eq!(plan.stamp, stamp(FIRST_TERM, 2));
        let _ = &mut k;
    }

    #[test]
    fn a_node_whose_connections_live_but_which_stops_reporting_is_dropped_in_the_end() {
        let [mut c, mut j, _] = three();
        // 2 refreshes every 30 s; 3 says nothing more, though its link stays up.
        for round in 1..=3u64 {
            report(&mut j, &mut c, at(round * 30_000));
            assert!(plans(&mut c, at(round * 30_000)).is_empty());
        }
        assert_eq!(c.next_wake(at(95_000)), at(100_000));
        let plan = issued(c.poll(at(100_000)));
        assert_eq!(roster_of(&plan), ids(&[1, 2]));
    }

    #[test]
    fn a_changed_name_is_a_new_plan() {
        let [mut c, _, _] = three();
        let config = Config {
            meeting: meeting(),
            name: Redacted::new("Bob".to_owned()),
            first_sequence: 2000,
        };
        let mut renamed = Node::new(config, FakeIdentity(2), FakeVerifier);
        links(&mut renamed, &[1]);
        report(&mut renamed, &mut c, at(500));
        let plan = issued(c.poll(at(500)));
        assert!(plan.roster.iter().any(|m| m.name.reveal() == "Bob"));
    }

    #[test]
    fn links_and_unreachable_pairs_from_reports_shape_the_wiring() {
        // 1 conducts. 2 and 3 are connected to each other; 3 cannot reach 1.
        let (mut c, mut j, mut k) = (node(1), node(2), node(3));
        c.create_meeting();
        links(&mut c, &[2]);
        links(&mut j, &[1, 3]);
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
    fn a_change_in_the_conductors_own_view_is_planned_for() {
        // 1 - 2 - 3 and 1 - 3: a triangle.
        let [mut c, mut j, mut k] = three();
        links(&mut j, &[1, 3]);
        links(&mut k, &[1, 2]);
        report(&mut j, &mut c, at(300));
        report(&mut k, &mut c, at(300));
        drain(&mut c, at(300));
        let before = c.plan().unwrap().clone();
        assert!(before.audio_overlay.contains(&edge(1, 3)));
        // The conductor finds it can no longer reach 3.
        c.set_view(LocalView {
            links: vec![direct(2)],
            unreachable: vec![id_of(3)],
            ..LocalView::default()
        });
        assert_eq!(c.next_wake(at(1000)), at(1000));
        let after = issued(c.poll(at(1000)));
        assert_eq!(
            roster_of(&after),
            ids(&[1, 2, 3]),
            "3 is still there, through 2"
        );
        assert!(!after.audio_overlay.contains(&edge(1, 3)));
        assert!(after.audio_overlay.contains(&edge(2, 3)));
    }

    #[test]
    fn existing_wiring_is_kept_as_nodes_join() {
        let mut c = node(1);
        c.create_meeting();
        let mut held = issued(c.poll(at(0)));
        let mut joined = vec![];
        for (i, seed) in (2..=12u8).enumerate() {
            let now = at(300 * (i as u64 + 1));
            let mut joiner = node(seed);
            links(&mut joiner, &[1]);
            report(&mut joiner, &mut c, now);
            joined.push(seed);
            links(&mut c, &joined);
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
        let [mut c, mut j, mut k] = three();
        links(&mut j, &[1, 3]);
        links(&mut k, &[1, 2]);
        report(&mut j, &mut c, at(300));
        report(&mut k, &mut c, at(300));
        drain(&mut c, at(300));
        let before = c.plan().unwrap().clone();
        // A refresh: no reason to look at the plan.
        report(&mut k, &mut c, at(30_300));
        assert!(plans(&mut c, at(30_300)).is_empty());
        assert_eq!(c.plan(), Some(&before));
        // 3 now says it cannot reach 1.
        k.set_view(LocalView {
            links: vec![direct(2)],
            unreachable: vec![id_of(1)],
            ..LocalView::default()
        });
        report(&mut k, &mut c, at(40_000));
        assert_eq!(c.next_wake(at(40_000)), at(40_000));
        let after = issued(c.poll(at(40_000)));
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

    /// A plan by conductor 1 for members 1 to 4 with the given peerings, also its audio links.
    fn plan_with(generation: u32, edges: &[(u8, u8)]) -> (u64, Vec<u8>) {
        let edges: Vec<Edge> = edges.iter().map(|(a, b)| edge(*a, *b)).collect();
        let plan = Plan {
            stamp: stamp(1, generation),
            conductor: id_of(1),
            roster: (1..=4)
                .map(|m| Member {
                    key: FakeIdentity(m).public_key(),
                    name: Redacted::new(format!("node {m}")),
                    tracks: vec![],
                })
                .collect(),
            peerings: edges.clone(),
            audio_overlay: edges,
            audio_top_n: 3,
            next_small_alias: 0,
            next_wide_alias: WIDE_ALIAS_START,
            successors: vec![],
        };
        (
            plan.stamp.group_id(),
            plan.seal(&meeting(), &FakeIdentity(1)),
        )
    }

    #[test]
    fn a_plan_that_asks_nothing_new_of_a_node_is_reported_with_the_next_regular_report() {
        let mut n = node(3);
        let (group, envelope) = plan_with(0, &[(1, 2), (2, 3)]);
        n.on_plan(group, &envelope);
        assert_eq!(opened(n.poll(at(0)), 3).applied_plan, Some(stamp(1, 0)));
        // 4 is attached to 1: nothing changes for 3.
        let (group, envelope) = plan_with(1, &[(1, 2), (2, 3), (1, 4)]);
        assert_eq!(n.on_plan(group, &envelope), PlanVerdict::Adopted);
        assert_eq!(n.plan().map(|p| p.stamp), Some(stamp(1, 1)));
        assert_eq!(n.next_wake(at(300)), at(30_000));
        assert_eq!(n.poll(at(300)), None);
        assert_eq!(
            opened(n.poll(at(30_000)), 3).applied_plan,
            Some(stamp(1, 1))
        );
    }

    #[test]
    fn a_plan_that_changes_a_nodes_own_links_is_reported_at_once() {
        let mut n = node(3);
        let (group, envelope) = plan_with(0, &[(1, 2), (2, 3)]);
        n.on_plan(group, &envelope);
        n.poll(at(0));
        // 4 is attached to 3.
        let (group, envelope) = plan_with(1, &[(1, 2), (2, 3), (3, 4)]);
        assert_eq!(n.on_plan(group, &envelope), PlanVerdict::Adopted);
        assert_eq!(n.next_wake(at(300)), at(300));
        assert_eq!(opened(n.poll(at(300)), 3).applied_plan, Some(stamp(1, 1)));
    }

    fn mic() -> MediaTrack {
        MediaTrack {
            audio: true,
            source: palaver_proto::Source::Main,
            rendition: b"opus".to_vec(),
        }
    }

    fn video(rendition: &str) -> MediaTrack {
        MediaTrack {
            audio: false,
            source: palaver_proto::Source::Main,
            rendition: rendition.as_bytes().to_vec(),
        }
    }

    /// Tell `node` which tracks it can send, keeping its links.
    fn offers(node: &mut Node<FakeIdentity, FakeVerifier>, tracks: &[MediaTrack]) {
        let view = LocalView {
            tracks: tracks.to_vec(),
            ..node.view().clone()
        };
        node.set_view(view);
    }

    /// The alias the plan held by `node` gives `track` of member `seed`.
    fn alias(node: &Node<FakeIdentity, FakeVerifier>, seed: u8, track: &MediaTrack) -> Option<u64> {
        node.plan()?.alias_of(&track.of(id_of(seed)), &FakeVerifier)
    }

    /// A conductor (node 1) and node 2, linked, each offering a microphone and two camera
    /// renditions; the plan issued, at time 0.
    fn two_with_tracks() -> [Node<FakeIdentity, FakeVerifier>; 2] {
        let (mut c, mut j) = (node(1), node(2));
        c.create_meeting();
        let tracks = [mic(), video("180p"), video("1080p")];
        c.set_view(LocalView {
            links: vec![direct(2)],
            tracks: tracks.to_vec(),
            ..LocalView::default()
        });
        j.set_view(LocalView {
            links: vec![direct(1)],
            tracks: tracks.to_vec(),
            ..LocalView::default()
        });
        report(&mut j, &mut c, at(0));
        drain(&mut c, at(0));
        [c, j]
    }

    #[test]
    fn every_track_a_node_offers_gets_an_alias_small_for_audio_and_wide_for_the_rest() {
        let [c, _] = two_with_tracks();
        let plan = c.plan().unwrap();
        let mut small = vec![];
        let mut wide = vec![];
        for seed in [1, 2] {
            small.push(alias(&c, seed, &mic()).unwrap());
            wide.push(alias(&c, seed, &video("180p")).unwrap());
            wide.push(alias(&c, seed, &video("1080p")).unwrap());
        }
        small.sort_unstable();
        wide.sort_unstable();
        // Microphones take 0 and 1: one byte in every audio packet.
        assert_eq!(small, [0, 1]);
        assert_eq!(
            wide,
            (WIDE_ALIAS_START..WIDE_ALIAS_START + 4).collect::<Vec<_>>()
        );
        assert_eq!(
            (plan.next_small_alias, plan.next_wide_alias),
            (2, WIDE_ALIAS_START + 4)
        );
        // A track nobody offered has none.
        assert_eq!(alias(&c, 2, &video("720p")), None);
    }

    #[test]
    fn a_track_keeps_its_alias_and_a_new_one_is_a_new_plan() {
        let [mut c, mut j] = two_with_tracks();
        let aliases = |c: &Node<FakeIdentity, FakeVerifier>| -> Vec<Option<u64>> {
            [mic(), video("180p"), video("1080p")]
                .iter()
                .map(|t| alias(c, 2, t))
                .collect()
        };
        let before = aliases(&c);
        // 2 can now also share its screen.
        let screen = MediaTrack {
            source: palaver_proto::Source::Screen,
            ..video("1080p")
        };
        offers(
            &mut j,
            &[mic(), video("180p"), video("1080p"), screen.clone()],
        );
        report(&mut j, &mut c, at(1000));
        let plan = issued(c.poll(at(1000)));
        assert_eq!(
            plan.alias_of(&screen.of(id_of(2)), &FakeVerifier),
            Some(WIDE_ALIAS_START + 4)
        );
        assert_eq!(aliases(&c), before);
    }

    #[test]
    fn an_alias_is_not_given_again_after_its_track_or_its_member_has_gone() {
        let [mut c, mut j] = two_with_tracks();
        let old_mic = alias(&c, 2, &mic()).unwrap();
        let old_low = alias(&c, 2, &video("180p")).unwrap();
        // 2 stops offering the low rendition, then offers it again.
        offers(&mut j, &[mic(), video("1080p")]);
        report(&mut j, &mut c, at(1000));
        drain(&mut c, at(1000));
        assert_eq!(alias(&c, 2, &video("180p")), None);
        offers(&mut j, &[mic(), video("1080p"), video("180p")]);
        report(&mut j, &mut c, at(2000));
        drain(&mut c, at(2000));
        let new_low = alias(&c, 2, &video("180p")).unwrap();
        assert!(new_low > old_low);
        assert_eq!(alias(&c, 2, &mic()), Some(old_mic), "untouched");

        // 2 leaves and comes back: all its tracks get new aliases.
        let tracks = c.view().tracks.clone();
        c.set_view(LocalView {
            tracks: tracks.clone(),
            ..LocalView::default()
        });
        drain(&mut c, at(3000));
        drain(&mut c, at(3500));
        assert_eq!(alias(&c, 2, &mic()), None);
        c.set_view(LocalView {
            links: vec![direct(2)],
            tracks,
            ..LocalView::default()
        });
        drain(&mut c, at(9000));
        let returned = alias(&c, 2, &mic()).unwrap();
        assert!(returned > old_mic, "{returned}");
        assert!(alias(&c, 2, &video("180p")).unwrap() > new_low);
    }

    #[test]
    fn a_track_offered_twice_is_given_one_alias() {
        let mut c = node(1);
        c.create_meeting();
        offers(&mut c, &[mic(), mic()]);
        drain(&mut c, at(0));
        let plan = c.plan().unwrap();
        assert_eq!(plan.roster[0].tracks.len(), 1);
        assert_eq!(plan.next_small_alias, 1);
    }

    #[test]
    fn a_repeat_of_the_same_offer_is_not_a_new_plan() {
        let [mut c, mut j] = two_with_tracks();
        let before = c.plan().unwrap().clone();
        report(&mut j, &mut c, at(30_000));
        assert!(plans(&mut c, at(30_000)).is_empty());
        assert_eq!(c.plan(), Some(&before));
    }
}
