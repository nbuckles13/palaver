use palaver_control::{
    LocalView, Output as ControlOutput, PlanVerdict, REPORT_TIMEOUT, ReportVerdict,
};
use palaver_khef_core::{
    Action, AudioOut, AudioPlan, Khef, KhefConfig, LinkWatchConfig, Output as KhefOutput, Role,
    StreamId, StreamKind, TopNConfig, VideoConfig, VideoFrom, VideoPlan, VideoRoute,
};
use palaver_moq_wire::Setup;
use palaver_proto::{
    Edge, Link, MediaTrack, MeetingTag, ParticipantId, Plan, Redacted, Report, Signer, Source,
    Timestamp, Track, TrackName, Verifier,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;

/// How many control tracks a khef remembers.
const MAX_CONTROL_TRACKS: usize = 1024;

/// What a node is told once, at start.
#[derive(Debug, Clone)]
pub struct Config {
    pub meeting: MeetingTag,
    /// The name the user chose.
    pub name: Redacted<String>,
    /// The sequence number of the node's first report; see
    /// [`palaver_control::Config::first_sequence`].
    pub first_sequence: u64,
    /// What the node's khef announces about itself when a session opens.
    pub setup: Setup,
    pub link_watch: LinkWatchConfig,
    /// Every media track the node can send.
    pub tracks: Vec<MediaTrack>,
    /// Where the node can be reached.
    pub addresses: Vec<Redacted<SocketAddr>>,
}

/// Something for the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Do this to the connection to `peer`.
    Stream { peer: ParticipantId, action: Action },
    /// Send this datagram to `peer`.
    Datagram { peer: ParticipantId, bytes: Vec<u8> },
    /// Connect to `peer`. Answer with [`Node::link_up`] or [`Node::dial_failed`].
    Dial { peer: ParticipantId },
    /// Close the connection to `peer`: the plan no longer names it, or it has fallen
    /// silent. The node has already forgotten the link.
    Hangup { peer: ParticipantId },
    /// Somebody watches this node's own video track `alias` (`wanted`), or nobody does any
    /// more. Make it only while it is wanted, beginning with a key frame, and give it to
    /// [`Node::local_video`].
    Produce { alias: u64, wanted: bool },
    /// Begin a new group, with a key frame, on this node's own video track `alias`, as
    /// soon as the encoder allows.
    KeyFrame { alias: u64 },
    /// Bytes of one group of a video track this node watches, in order. The first
    /// (`start`) begin with the stream's header: read them with
    /// [`palaver_khef_core::GroupReader`]. `fin` when the group is whole.
    Video {
        alias: u64,
        group: u64,
        start: bool,
        bytes: Vec<u8>,
        fin: bool,
    },
    /// A group this node was being given was given up part way.
    VideoReset { alias: u64, group: u64 },
}

/// A video track of the meeting, as the plan this node follows lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrack {
    /// What the track's groups carry, and what to [`Node::watch`].
    pub alias: u64,
    pub participant: ParticipantId,
    pub source: Source,
    pub rendition: Vec<u8>,
    /// This node's own: made here, not watched.
    pub own: bool,
}

#[derive(Debug, Clone, Copy)]
struct LinkInfo {
    rtt: Duration,
    observed: Option<SocketAddr>,
}

/// How long the khef keeps the latest object of a control track.
const fn control_lifetime(track: &Track) -> Option<Duration> {
    match track {
        Track::Report { .. } => Some(REPORT_TIMEOUT),
        _ => None,
    }
}

/// Counts of what a node has done, for diagnostics and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Reports this node has published.
    pub reports_published: u64,
    /// Plans this node has issued as conductor.
    pub plans_issued: u64,
    /// Control objects from neighbours that the control logic took.
    pub control_accepted: u64,
    /// Control objects from neighbours that it turned away: copies, old ones, bad ones.
    pub control_refused: u64,
}

/// One node.
pub struct Node<S, V> {
    control: palaver_control::Node<S, V>,
    khef: Khef<ParticipantId>,
    verifier: V,
    meeting: MeetingTag,
    tracks: Vec<MediaTrack>,
    addresses: Vec<Redacted<SocketAddr>>,
    links: BTreeMap<ParticipantId, LinkInfo>,
    dialing: BTreeSet<ParticipantId>,
    unreachable: BTreeSet<ParticipantId>,
    /// Entry points given at join, not yet tried.
    entry_points: VecDeque<ParticipantId>,
    /// The control logic runs: this node created the meeting, or has had a link.
    started: bool,
    /// The node holds a link the plan does not name, kept until the other end is wired.
    surplus_links: bool,
    /// The alias the plan gives this node's microphone.
    mic_alias: Option<u64>,
    /// What the khef was last told about audio, so it is told again only on a change.
    audio: AudioPlan<ParticipantId>,
    /// The same for video, and the tracks it was made from.
    video: VideoPlan<ParticipantId>,
    video_tracks: Vec<VideoTrack>,
    counters: Counters,
    outputs: VecDeque<Output>,
}

impl<S: Signer, V: Verifier + Clone> Node<S, V> {
    pub fn new(config: Config, signer: S, verifier: V) -> Self {
        let control = palaver_control::Node::new(
            palaver_control::Config {
                meeting: config.meeting.clone(),
                name: config.name,
                first_sequence: config.first_sequence,
            },
            signer,
            verifier.clone(),
        );
        let khef = Khef::new(KhefConfig {
            meeting: config.meeting.clone(),
            setup: config.setup,
            link_watch: config.link_watch,
            control_lifetime,
            max_control_tracks: MAX_CONTROL_TRACKS,
            video: VideoConfig::default(),
        });
        let mut node = Self {
            control,
            khef,
            verifier,
            meeting: config.meeting,
            tracks: config.tracks,
            addresses: config.addresses,
            links: BTreeMap::new(),
            dialing: BTreeSet::new(),
            unreachable: BTreeSet::new(),
            entry_points: VecDeque::new(),
            started: false,
            surplus_links: false,
            mic_alias: None,
            audio: AudioPlan::default(),
            video: VideoPlan::default(),
            video_tracks: Vec::new(),
            counters: Counters::default(),
            outputs: VecDeque::new(),
        };
        node.refresh_view();
        node
    }

    // --- What the caller can ask ---

    /// This node's participant id.
    pub const fn me(&self) -> &ParticipantId {
        self.control.me()
    }

    /// The plan this node follows, once it has one.
    pub const fn plan(&self) -> Option<&Plan> {
        self.control.plan()
    }

    pub const fn is_conductor(&self) -> bool {
        self.control.is_conductor()
    }

    /// The latest report heard from another node.
    pub fn report_of(&self, participant: &ParticipantId) -> Option<&Report> {
        self.control.report_of(participant)
    }

    /// The addresses other nodes say they see `participant` at.
    pub fn observed_addresses_of(&self, participant: &ParticipantId) -> Vec<SocketAddr> {
        self.control.observed_addresses_of(participant)
    }

    /// The nodes the plan pairs this one with.
    pub fn planned_peers_now(&self) -> BTreeSet<ParticipantId> {
        self.control
            .plan()
            .map_or_else(BTreeSet::new, |plan| Self::planned_peers(plan, self.me()))
    }

    pub const fn counters(&self) -> Counters {
        self.counters
    }

    /// The neighbours this node holds a link to, in order.
    pub fn links(&self) -> impl Iterator<Item = &ParticipantId> {
        self.links.keys()
    }

    /// The next thing to do, if any. Drain after every call into the node.
    pub fn poll_output(&mut self) -> Option<Output> {
        self.outputs.pop_front()
    }

    /// When to call [`Node::poll`] next if nothing else happens.
    pub fn next_wake(&self, now: Timestamp) -> Option<Timestamp> {
        let control = self.started.then(|| self.control.next_wake(now));
        match (control, self.khef.next_wake()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    // --- Starting ---

    /// Create a meeting: this node is its conductor.
    pub fn create_meeting(&mut self, now: Timestamp) {
        if self.control.create_meeting() {
            self.started = true;
            self.service(now);
        }
    }

    /// Join a meeting through one of `entry_points`, tried in order.
    pub fn join(&mut self, entry_points: Vec<ParticipantId>, now: Timestamp) {
        self.entry_points = entry_points.into();
        self.look_for_the_meeting();
        self.service(now);
    }

    /// Where this node can be reached has changed. Its next report says so.
    pub fn set_addresses(&mut self, addresses: Vec<Redacted<SocketAddr>>, now: Timestamp) {
        if self.addresses != addresses {
            self.addresses = addresses;
            self.refresh_view();
            self.service(now);
        }
    }

    // --- What the connections report ---

    /// A connection to `peer` is up. `role` says which side dialled.
    pub fn link_up(
        &mut self,
        peer: ParticipantId,
        role: Role,
        rtt: Duration,
        observed: Option<SocketAddr>,
        now: Timestamp,
    ) {
        self.dialing.remove(&peer);
        if peer == *self.me() || self.links.contains_key(&peer) {
            return;
        }
        // It answered, whatever was thought before.
        self.unreachable.remove(&peer);
        self.links.insert(peer.clone(), LinkInfo { rtt, observed });
        self.khef.link_up(peer, role, now);
        self.started = true;
        self.refresh_view();
        self.apply_plan(now);
        self.service(now);
    }

    /// What is known about the path to `peer` changed.
    pub fn link_changed(&mut self, peer: &ParticipantId, rtt: Duration, now: Timestamp) {
        if let Some(info) = self.links.get_mut(peer) {
            info.rtt = rtt;
            self.refresh_view();
            self.service(now);
        }
    }

    /// The connection to `peer` is gone.
    pub fn link_down(&mut self, peer: &ParticipantId, now: Timestamp) {
        if self.links.remove(peer).is_none() {
            return;
        }
        self.khef.link_down(peer, now);
        self.refresh_view();
        // If the plan still names the peer, try to get it back.
        self.apply_plan(now);
        self.look_for_the_meeting();
        self.service(now);
    }

    /// A dial the node asked for gave up.
    pub fn dial_failed(&mut self, peer: &ParticipantId, now: Timestamp) {
        if !self.dialing.remove(peer) {
            return;
        }
        self.unreachable.insert(peer.clone());
        self.refresh_view();
        self.look_for_the_meeting();
        self.service(now);
    }

    /// Bytes arrived on a stream of the connection to `peer`.
    pub fn on_data(
        &mut self,
        peer: &ParticipantId,
        stream: StreamId,
        kind: StreamKind,
        bytes: &[u8],
        fin: bool,
        now: Timestamp,
    ) {
        self.khef.on_data(peer, stream, kind, bytes, fin, now);
        self.service(now);
    }

    /// `peer` reset a stream.
    pub fn on_reset(&mut self, peer: &ParticipantId, stream: StreamId, now: Timestamp) {
        self.khef.on_reset(peer, stream, now);
        self.service(now);
    }

    /// A datagram arrived from `peer`. If it is audio to pass on or to play, says where it
    /// goes; the caller sends the same bytes on.
    pub fn on_datagram(
        &mut self,
        peer: &ParticipantId,
        datagram: &[u8],
        now: Timestamp,
    ) -> Option<AudioOut<ParticipantId>> {
        let out = self.khef.on_datagram(peer, datagram, now);
        self.service(now);
        out
    }

    /// The microphone produced a frame. Appends the datagram to `out` and says which
    /// neighbours to send it to. `None` until the plan lists this node's microphone.
    pub fn local_audio(
        &mut self,
        group: u64,
        level: u8,
        payload: &[u8],
        out: &mut Vec<u8>,
        now: Timestamp,
    ) -> Option<AudioOut<ParticipantId>> {
        self.khef
            .local_audio(self.mic_alias?, group, level, payload, out, now)
    }

    /// Every video track in the plan this node follows.
    #[must_use]
    pub fn video_tracks(&self) -> &[VideoTrack] {
        &self.video_tracks
    }

    /// Begin to watch the video track with `alias`. It stays wanted, through changes of
    /// plan, until [`Node::unwatch`]; its groups come as [`Output::Video`].
    pub fn watch(&mut self, alias: u64, now: Timestamp) {
        self.khef.watch(alias, now);
        self.service(now);
    }

    /// Stop watching a video track.
    pub fn unwatch(&mut self, alias: u64, now: Timestamp) {
        self.khef.unwatch(alias, now);
        self.service(now);
    }

    /// The decoder has lost its place in a track this node watches, and needs a key frame.
    pub fn ask_key_frame(&mut self, alias: u64, now: Timestamp) {
        self.khef.ask_key_frame(alias, now);
        self.service(now);
    }

    /// Bytes of a group of this node's own video track, in order: the first (`start`)
    /// begin with the stream's header, as [`palaver_khef_core::GroupWriter`] writes it,
    /// and `fin` ends the group. Nothing happens unless the track is wanted
    /// ([`Output::Produce`]).
    pub fn local_video(
        &mut self,
        alias: u64,
        group: u64,
        start: bool,
        bytes: &[u8],
        fin: bool,
        now: Timestamp,
    ) {
        self.khef.local_video(alias, group, start, bytes, fin, now);
        self.service(now);
    }

    /// Give up a group of this node's own part way.
    pub fn local_video_reset(&mut self, alias: u64, group: u64, now: Timestamp) {
        self.khef.local_video_reset(alias, group, now);
        self.service(now);
    }

    /// Time has passed: watch the links, and let the control logic send what is due.
    pub fn poll(&mut self, now: Timestamp) {
        self.khef.poll(now);
        self.service(now);
    }

    // --- Inside ---

    /// Tell the control logic what this node's links are now.
    fn refresh_view(&mut self) {
        let view = LocalView {
            links: self
                .links
                .iter()
                .map(|(peer, info)| Link {
                    peer: peer.clone(),
                    rtt: info.rtt,
                    observed: info.observed.map(Redacted::new),
                })
                .collect(),
            unreachable: self.unreachable.iter().cloned().collect(),
            addresses: self.addresses.clone(),
            tracks: self.tracks.clone(),
        };
        self.control.set_view(view);
    }

    /// Carry things between the khef and the control logic until both are quiet.
    fn service(&mut self, now: Timestamp) {
        loop {
            let mut busy = false;
            while let Some(output) = self.khef.poll_output() {
                busy = true;
                self.on_khef_output(output, now);
            }
            while self.started
                && let Some(output) = self.control.poll(now)
            {
                busy = true;
                match output {
                    ControlOutput::PublishReport { group_id, object } => {
                        self.counters.reports_published =
                            self.counters.reports_published.saturating_add(1);
                        let track = Track::Report {
                            participant: self.me().clone(),
                        };
                        self.khef
                            .accept_control(None, &track, group_id, &object, now);
                    }
                    ControlOutput::PublishPlan { group_id, object } => {
                        self.counters.plans_issued = self.counters.plans_issued.saturating_add(1);
                        self.khef
                            .accept_control(None, &Track::Plan, group_id, &object, now);
                        self.apply_plan(now);
                    }
                }
            }
            if !busy {
                return;
            }
        }
    }

    fn on_khef_output(&mut self, output: KhefOutput<ParticipantId>, now: Timestamp) {
        match output {
            KhefOutput::Stream { peer, action } => {
                self.outputs.push_back(Output::Stream { peer, action });
            }
            KhefOutput::Datagram { peer, bytes } => {
                self.outputs.push_back(Output::Datagram { peer, bytes });
            }
            // The control logic checks it; only what it takes is kept and flooded.
            KhefOutput::Control {
                from,
                track,
                group,
                payload,
            } => {
                let (accepted, new_plan) = match &track {
                    Track::Plan => {
                        let adopted = self.control.on_plan(group, &payload) == PlanVerdict::Adopted;
                        (adopted, adopted)
                    }
                    Track::Report { participant } => {
                        let verdict = self.control.on_report(participant, group, &payload, now);
                        (verdict == ReportVerdict::Accepted, false)
                    }
                    _ => (false, false),
                };
                if !accepted {
                    self.counters.control_refused = self.counters.control_refused.saturating_add(1);
                    return;
                }
                self.counters.control_accepted = self.counters.control_accepted.saturating_add(1);
                self.khef
                    .accept_control(Some(&from), &track, group, &payload, now);
                // A new plan changes what the node should be connected to; a report may
                // show that a neighbour is now wired and an unplanned link to it can go.
                if new_plan || self.surplus_links {
                    self.apply_plan(now);
                }
            }
            KhefOutput::Produce { alias, wanted } => {
                self.outputs.push_back(Output::Produce { alias, wanted });
            }
            KhefOutput::KeyFrame { alias } => self.outputs.push_back(Output::KeyFrame { alias }),
            KhefOutput::Video {
                alias,
                group,
                start,
                bytes,
                fin,
            } => self.outputs.push_back(Output::Video {
                alias,
                group,
                start,
                bytes,
                fin,
            }),
            KhefOutput::VideoReset { alias, group } => {
                self.outputs.push_back(Output::VideoReset { alias, group });
            }
            KhefOutput::LinkDead { peer } => {
                self.links.remove(&peer);
                self.outputs.push_back(Output::Hangup { peer });
                self.refresh_view();
                self.apply_plan(now);
                self.look_for_the_meeting();
            }
        }
    }

    fn dial(&mut self, peer: ParticipantId) {
        if peer != *self.me()
            && !self.links.contains_key(&peer)
            && self.dialing.insert(peer.clone())
        {
            self.outputs.push_back(Output::Dial { peer });
        }
    }

    /// With no link and no dial under way, find a way in: the entry points given at join,
    /// then the members of the last plan seen.
    fn look_for_the_meeting(&mut self) {
        if !self.links.is_empty() || !self.dialing.is_empty() {
            return;
        }
        if let Some(entry) = self.entry_points.pop_front() {
            return self.dial(entry);
        }
        let Some(plan) = self.control.plan() else {
            return;
        };
        let next = plan
            .roster
            .iter()
            .map(|member| self.verifier.participant_id(&member.key))
            .find(|peer| peer != self.control.me() && !self.unreachable.contains(peer));
        if let Some(peer) = next {
            self.dial(peer);
        }
    }

    /// The ids of the nodes the plan pairs `node` with.
    fn planned_peers(plan: &Plan, node: &ParticipantId) -> BTreeSet<ParticipantId> {
        plan.peerings
            .iter()
            .filter_map(|edge| {
                if edge.0 == *node {
                    Some(edge.1.clone())
                } else if edge.1 == *node {
                    Some(edge.0.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// For each node the overlay reaches from `me`, the neighbour of `me` on the way to it.
    fn first_hops(overlay: &[Edge], me: &ParticipantId) -> BTreeMap<ParticipantId, ParticipantId> {
        let beside = |at: &ParticipantId| -> Vec<ParticipantId> {
            overlay
                .iter()
                .filter_map(|edge| {
                    if edge.0 == *at {
                        Some(edge.1.clone())
                    } else if edge.1 == *at {
                        Some(edge.0.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        let mut hops = BTreeMap::new();
        let mut edge: VecDeque<(ParticipantId, ParticipantId)> = beside(me)
            .into_iter()
            .map(|first| (first.clone(), first))
            .collect();
        while let Some((at, first)) = edge.pop_front() {
            if at == *me || hops.contains_key(&at) {
                continue;
            }
            hops.insert(at.clone(), first.clone());
            for further in beside(&at) {
                edge.push_back((further, first.clone()));
            }
        }
        hops
    }

    /// Where audio goes: every microphone in the plan, over this node's tree links. And
    /// the alias of this node's own microphone.
    fn audio_plan(
        &self,
        plan: &Plan,
        me: &ParticipantId,
    ) -> (AudioPlan<ParticipantId>, Option<u64>) {
        let mut audio = AudioPlan {
            links: plan
                .audio_overlay
                .iter()
                .filter_map(|edge| {
                    if edge.0 == *me {
                        Some(edge.1.clone())
                    } else if edge.1 == *me {
                        Some(edge.0.clone())
                    } else {
                        None
                    }
                })
                .collect(),
            tracks: BTreeMap::new(),
            own: BTreeSet::new(),
            top_n: TopNConfig {
                n: usize::try_from(plan.audio_top_n).unwrap_or(usize::MAX),
                ..TopNConfig::default()
            },
        };
        let mut mic_alias = None;
        for member in &plan.roster {
            let participant = self.verifier.participant_id(&member.key);
            for assigned in &member.tracks {
                if !assigned.track.audio || assigned.track.source != Source::Main {
                    continue;
                }
                let name = TrackName {
                    meeting: self.meeting.clone(),
                    track: assigned.track.of(participant.clone()),
                };
                let Some(name) = name.to_full_name() else {
                    continue;
                };
                audio.tracks.insert(assigned.alias, name);
                if participant == *me {
                    audio.own.insert(assigned.alias);
                    mic_alias.get_or_insert(assigned.alias);
                }
            }
        }
        (audio, mic_alias)
    }

    /// Where video goes: every video track in the plan, each from this node itself or
    /// from the neighbour on the way to whoever makes it. To begin with the way is the
    /// audio tree's.
    fn video_plan(
        &self,
        plan: &Plan,
        me: &ParticipantId,
    ) -> (VideoPlan<ParticipantId>, Vec<VideoTrack>) {
        let hops = Self::first_hops(&plan.audio_overlay, me);
        let mut video = VideoPlan::default();
        let mut listed = Vec::new();
        for member in &plan.roster {
            let participant = self.verifier.participant_id(&member.key);
            let own = participant == *me;
            for assigned in member
                .tracks
                .iter()
                .filter(|assigned| !assigned.track.audio)
            {
                let name = TrackName {
                    meeting: self.meeting.clone(),
                    track: assigned.track.of(participant.clone()),
                };
                let Some(name) = name.to_full_name() else {
                    continue;
                };
                listed.push(VideoTrack {
                    alias: assigned.alias,
                    participant: participant.clone(),
                    source: assigned.track.source,
                    rendition: assigned.track.rendition.clone(),
                    own,
                });
                // A track of someone the tree does not reach yet is listed, and has no way.
                let from = if own {
                    VideoFrom::Here
                } else if let Some(hop) = hops.get(&participant) {
                    VideoFrom::Link(hop.clone())
                } else {
                    continue;
                };
                video
                    .tracks
                    .insert(assigned.alias, VideoRoute { name, from });
            }
        }
        (video, listed)
    }

    /// Bring this node's connections and its khef into line with the plan it holds.
    fn apply_plan(&mut self, now: Timestamp) {
        let Some(plan) = self.control.plan() else {
            return;
        };
        let me = self.control.me().clone();
        let planned = Self::planned_peers(plan, &me);
        let roster: BTreeSet<ParticipantId> = plan
            .roster
            .iter()
            .map(|member| self.verifier.participant_id(&member.key))
            .collect();

        // The lower participant id dials.
        let to_dial: Vec<ParticipantId> = planned
            .iter()
            .filter(|peer| {
                me < **peer && !self.links.contains_key(*peer) && !self.unreachable.contains(*peer)
            })
            .cloned()
            .collect();

        // A link the plan does not name goes only once both ends are wired as the plan
        // says: until then it may be the other end's only way to receive the plan, or to be
        // heard of at all if the plan does not know it yet. How the other end stands is
        // read from its report. (Not "has applied this plan": a node the plan asks nothing
        // new of says so only in its next regular report, and a node holding all its
        // planned links receives plans over them.)
        let all_up = planned.iter().all(|peer| self.links.contains_key(peer));
        let surplus: Vec<ParticipantId> = self
            .links
            .keys()
            .filter(|peer| !planned.contains(*peer))
            .cloned()
            .collect();
        let to_close: Vec<ParticipantId> = if roster.contains(&me) && all_up {
            surplus
                .iter()
                .filter(|peer| {
                    roster.contains(*peer)
                        && self.control.report_of(peer).is_some_and(|report| {
                            Self::planned_peers(plan, peer)
                                .iter()
                                .all(|other| report.links.iter().any(|link| link.peer == *other))
                        })
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        self.surplus_links = surplus.len() > to_close.len();

        let (audio, mic_alias) = self.audio_plan(plan, &me);
        let (video, video_tracks) = self.video_plan(plan, &me);
        self.mic_alias = mic_alias;
        if audio != self.audio {
            self.audio = audio.clone();
            self.khef.set_audio_plan(audio, now);
        }
        self.video_tracks = video_tracks;
        if video != self.video {
            self.video = video.clone();
            self.khef.set_video_plan(video, now);
        }
        for peer in to_dial {
            self.dial(peer);
        }
        if !to_close.is_empty() {
            for peer in to_close {
                self.links.remove(&peer);
                self.khef.link_down(&peer, now);
                self.outputs.push_back(Output::Hangup { peer });
            }
            self.refresh_view();
        }
    }
}
