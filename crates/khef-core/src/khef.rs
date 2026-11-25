//! The khef: the `MoQ` relay inside every node.
//!
//! One [`Khef`] holds a [`Session`] and a [`LinkWatch`] for each neighbour and does four
//! things across them:
//!
//! - **Control tracks flood.** A report or plan that the node's control logic has accepted is
//!   passed to every neighbour except the one it came from, and a neighbour that connects is
//!   given the latest of each (see [`Flood`]). The khef does not judge control objects: it
//!   hands each arrival up ([`Output::Control`]) and floods only what is handed back down
//!   ([`Khef::accept_control`]), so nothing unverified spreads.
//! - **Audio follows the plan's tree.** On each link of the tree every audio track is
//!   announced, and a packet that arrives on one link (or from the node's own microphone) is
//!   offered to the top-N selector of every other tree link, and of the node's own client.
//!   Where it is selected it goes out **exactly as it arrived**: every khef uses the alias
//!   the plan assigns, so there is nothing to rewrite, and the khef only says which links to
//!   send the caller's buffer to.
//! - **Video goes where it is watched.** A track is asked for by name, hop by hop along the
//!   way the plan gives, and its groups are passed back as they arrive (see [`video`]).
//! - **Links are watched.** A link that falls silent is reported dead; a link with nothing
//!   to carry is given a filler.
//!
//! Sans-IO: the caller owns the connections, feeds in what arrives with the time, and
//! carries out what comes back. `N` names a neighbour.

mod video;

pub use video::{VideoConfig, VideoFrom, VideoPlan, VideoRoute};

use crate::session::NOT_SUPPORTED;
use crate::{
    Action, Dedupe, Event, Flood, LinkWatch, LinkWatchConfig, Offer, Role, Session, StreamId,
    StreamKind, TopN, TopNConfig,
};
use core::ops::Range;
use palaver_moq_wire::{
    Datagram, FullTrackName, Kvp, Namespace, ObjectBody, PROPERTY_AUDIO_LEVEL, Setup, Value,
    encode_kvps, int_property,
};
use palaver_proto::{MeetingTag, Source, Timestamp, Track, TrackName};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

/// The loudest audio level a packet can carry.
const MAX_LEVEL: u64 = 127;
/// How many recent groups of an audio track are remembered to drop a repeat.
const AUDIO_DEDUPE_WINDOW: usize = 64;

#[derive(Debug, Clone)]
pub struct KhefConfig {
    pub meeting: MeetingTag,
    /// What this khef announces about itself when a session opens.
    pub setup: Setup,
    pub link_watch: LinkWatchConfig,
    /// How long the latest object of a control track is kept (`None`: until replaced).
    pub control_lifetime: fn(&Track) -> Option<Duration>,
    /// How many control tracks are remembered.
    pub max_control_tracks: usize,
    pub video: VideoConfig,
}

/// What the plan says about audio, as far as this khef is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPlan<N> {
    /// The neighbours joined to this node by a link of the audio tree.
    pub links: BTreeSet<N>,
    /// Every audio track of the meeting, by the alias the plan gives it.
    pub tracks: BTreeMap<u64, FullTrackName>,
    /// The aliases of this node's own audio tracks: never played back to its own client.
    pub own: BTreeSet<u64>,
    /// Selection on each link and for the client.
    pub top_n: TopNConfig,
}

impl<N> Default for AudioPlan<N> {
    fn default() -> Self {
        Self {
            links: BTreeSet::new(),
            tracks: BTreeMap::new(),
            own: BTreeSet::new(),
            top_n: TopNConfig::default(),
        }
    }
}

/// Something for the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output<N> {
    /// Do this to the connection to `peer`.
    Stream { peer: N, action: Action },
    /// Send this datagram to `peer`: a filler.
    Datagram { peer: N, bytes: Vec<u8> },
    /// A control object arrived from `from`. Check it; if it is genuine and news, hand it
    /// back with [`Khef::accept_control`].
    Control {
        from: N,
        track: Track,
        group: u64,
        payload: Vec<u8>,
    },
    /// The link to `peer` has fallen silent and has been dropped. Close the connection.
    LinkDead { peer: N },
    /// Somebody watches this node's own video track `alias` (`wanted`), or nobody does any
    /// more. Make it only while it is wanted, beginning with a key frame, and give it to
    /// [`Khef::local_video`].
    Produce { alias: u64, wanted: bool },
    /// Begin a new group, with a key frame, on this node's own video track `alias`.
    KeyFrame { alias: u64 },
    /// Bytes of one group of a video track the client watches, in order. The first
    /// (`start`) begin with the stream's header: read them with [`crate::GroupReader`].
    /// `fin` when the group is whole.
    Video {
        alias: u64,
        group: u64,
        start: bool,
        bytes: Vec<u8>,
        fin: bool,
    },
    /// A group the client was being given was given up part way.
    VideoReset { alias: u64, group: u64 },
}

/// Where an audio packet goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOut<N> {
    /// Send the datagram, unchanged, to each of these neighbours.
    pub forward_to: Vec<N>,
    /// Play it: the node's own client has this track among its loudest.
    pub for_client: bool,
    pub alias: u64,
    /// The frame counter.
    pub group: u64,
    /// The `audio_level` the packet carries.
    pub level: u8,
    /// Where the encoded frame lies in the datagram, for a client that is to play it.
    pub payload: Range<usize>,
}

struct Link {
    session: Session,
    watch: LinkWatch,
    /// Our requests to the peer, and whether the peer has asked us for the same.
    control_request: Option<StreamId>,
    audio_request: Option<StreamId>,
    their_control_request: Option<StreamId>,
    their_audio_request: Option<StreamId>,
    /// Tracks the peer has announced to us, by alias.
    their_tracks: BTreeMap<u64, Track>,
    /// Aliases of the tracks we have announced to the peer.
    announced: BTreeSet<u64>,
    /// Selection for this link, while it is part of the audio tree.
    selector: Option<TopN<u64>>,
}

/// One node's khef.
pub struct Khef<N> {
    config: KhefConfig,
    control_prefix: Option<Namespace>,
    audio_prefix: Option<Namespace>,
    links: BTreeMap<N, Link>,
    flood: Flood<Track, N>,
    audio: AudioPlan<N>,
    client: TopN<u64>,
    dedupe: Dedupe<u64>,
    video: VideoPlan<N>,
    watching: BTreeMap<u64, video::Watched<N>>,
    outputs: VecDeque<Output<N>>,
}

impl<N: Ord + Clone> Khef<N> {
    #[must_use]
    pub fn new(config: KhefConfig) -> Self {
        let control_prefix = config.meeting.control_prefix();
        let audio_prefix = config.meeting.audio_prefix(Source::Main);
        let flood = Flood::new(config.control_lifetime, config.max_control_tracks);
        Self {
            config,
            control_prefix,
            audio_prefix,
            links: BTreeMap::new(),
            flood,
            audio: AudioPlan::default(),
            client: TopN::new(TopNConfig::default()),
            dedupe: Dedupe::new(AUDIO_DEDUPE_WINDOW),
            video: VideoPlan::default(),
            watching: BTreeMap::new(),
            outputs: VecDeque::new(),
        }
    }

    /// The next thing to do, if any. Drain after every call into the khef.
    pub fn poll_output(&mut self) -> Option<Output<N>> {
        self.outputs.pop_front()
    }

    /// The neighbours this khef holds a link to.
    pub fn neighbours(&self) -> impl Iterator<Item = &N> {
        self.links.keys()
    }

    /// The latest object held for a control track.
    #[must_use]
    pub fn latest_control(&self, track: &Track, now: Timestamp) -> Option<(u64, &[u8])> {
        self.flood.latest(track, now)
    }

    // --- Links ---

    /// A connection to `peer` is up. `role` says which side dialled.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the khef keeps `peer` as the link's key, and callers in other crates pass it by value"
    )]
    pub fn link_up(&mut self, peer: N, role: Role, now: Timestamp) {
        if self.links.contains_key(&peer) {
            return;
        }
        let mut session = Session::new(role, self.config.setup.clone());
        session.start();
        // Everyone exchanges control tracks with every neighbour.
        let control_request = self
            .control_prefix
            .clone()
            .and_then(|prefix| session.subscribe_tracks(prefix));
        let mut link = Link {
            session,
            watch: LinkWatch::new(self.config.link_watch, now),
            control_request,
            audio_request: None,
            their_control_request: None,
            their_audio_request: None,
            their_tracks: BTreeMap::new(),
            announced: BTreeSet::new(),
            selector: None,
        };
        self.wire_audio(&peer, &mut link);
        self.links.insert(peer.clone(), link);
        self.pump(&peer, now);
        self.video_link_up(now);
    }

    /// The connection to `peer` is gone.
    pub fn link_down(&mut self, peer: &N, now: Timestamp) {
        if self.links.remove(peer).is_some() {
            self.flood.disconnect(peer);
            self.video_link_down(peer, now);
        }
    }

    /// Bytes arrived on a stream of the connection to `peer`.
    pub fn on_data(
        &mut self,
        peer: &N,
        stream: StreamId,
        kind: StreamKind,
        bytes: &[u8],
        fin: bool,
        now: Timestamp,
    ) {
        let Some(link) = self.links.get_mut(peer) else {
            return;
        };
        link.watch.heard(now);
        link.session.on_data(stream, kind, bytes, fin);
        self.pump(peer, now);
    }

    /// `peer` reset a stream.
    pub fn on_reset(&mut self, peer: &N, stream: StreamId, now: Timestamp) {
        let Some(link) = self.links.get_mut(peer) else {
            return;
        };
        link.watch.heard(now);
        link.session.on_reset(stream);
        self.pump(peer, now);
    }

    /// Look at every link: report the ones that have fallen silent, and give a filler to
    /// the ones with nothing else to carry. Call when [`Khef::next_wake`] says, at least.
    pub fn poll(&mut self, now: Timestamp) {
        let dead: Vec<N> = self
            .links
            .iter()
            .filter(|(_, link)| link.watch.is_dead(now))
            .map(|(peer, _)| peer.clone())
            .collect();
        for peer in dead {
            self.link_down(&peer, now);
            self.outputs.push_back(Output::LinkDead { peer });
        }
        for (peer, link) in &mut self.links {
            if link.watch.filler_due(now) {
                link.watch.sent(now);
                let mut bytes = Vec::new();
                Session::encode_filler(&mut bytes);
                self.outputs.push_back(Output::Datagram {
                    peer: peer.clone(),
                    bytes,
                });
            }
        }
        self.video_poll(now);
    }

    /// When to call [`Khef::poll`] next if nothing else happens. `None` with no links.
    #[must_use]
    pub fn next_wake(&self) -> Option<Timestamp> {
        self.links
            .values()
            .map(|link| link.watch.next_wake())
            .chain(self.video_wake())
            .min()
    }

    // --- Control tracks ---

    /// A control object that the node's control logic has checked and found to be news:
    /// keep it and pass it on. `from` is the neighbour it came from, or `None` if this
    /// node made it. Returns whether the khef, too, found it newer than what it held.
    pub fn accept_control(
        &mut self,
        from: Option<&N>,
        track: &Track,
        group: u64,
        payload: &[u8],
        now: Timestamp,
    ) -> bool {
        let Offer::Fresh { forward_to } = self.flood.offer(from, track, group, payload, now) else {
            return false;
        };
        for peer in forward_to {
            self.send_control(&peer, track, group, payload);
            self.pump(&peer, now);
        }
        true
    }

    /// Send a control object to `peer`, announcing its track first if need be.
    #[expect(
        clippy::set_contains_or_insert,
        reason = "the alias is recorded only after the announcement is sent; the early returns between must leave it out"
    )]
    fn send_control(&mut self, peer: &N, track: &Track, group: u64, payload: &[u8]) {
        let Some(link) = self.links.get_mut(peer) else {
            return;
        };
        let Some(alias) = track.control_alias() else {
            return;
        };
        if !link.announced.contains(&alias) {
            let name = TrackName {
                meeting: self.config.meeting.clone(),
                track: track.clone(),
            };
            let Some(name) = name.to_full_name() else {
                return;
            };
            if !link.session.publish(name, alias) {
                return;
            }
            link.announced.insert(alias);
        }
        link.session.send_object(alias, group, payload);
    }

    // --- Audio ---

    /// The plan changed what this khef does with audio.
    pub fn set_audio_plan(&mut self, plan: AudioPlan<N>, now: Timestamp) {
        if plan.top_n != self.audio.top_n {
            self.client = TopN::new(plan.top_n);
            for link in self.links.values_mut() {
                link.selector = None;
            }
        }
        // Tracks that are gone are forgotten everywhere.
        let gone: Vec<u64> = self
            .audio
            .tracks
            .keys()
            .filter(|alias| !plan.tracks.contains_key(alias))
            .copied()
            .collect();
        self.audio = plan;
        for alias in &gone {
            self.client.remove(alias);
            self.dedupe.remove(alias);
        }
        let peers: Vec<N> = self.links.keys().cloned().collect();
        for peer in peers {
            if let Some(mut link) = self.links.remove(&peer) {
                for alias in &gone {
                    if link.announced.remove(alias) {
                        link.session.end_track(*alias);
                    }
                    if let Some(selector) = &mut link.selector {
                        selector.remove(alias);
                    }
                }
                self.wire_audio(&peer, &mut link);
                self.links.insert(peer.clone(), link);
            }
            self.pump(&peer, now);
        }
    }

    /// Bring one link into line with the audio plan: ask for the peer's audio and select
    /// for it if the link is part of the tree; announce the tracks the peer has asked for.
    fn wire_audio(&self, peer: &N, link: &mut Link) {
        if self.audio.links.contains(peer) {
            if link.selector.is_none() {
                link.selector = Some(TopN::new(self.audio.top_n));
            }
            if link.audio_request.is_none() {
                link.audio_request = self
                    .audio_prefix
                    .clone()
                    .and_then(|prefix| link.session.subscribe_tracks(prefix));
            }
        } else {
            link.selector = None;
            if let Some(request) = link.audio_request.take() {
                link.session.unsubscribe_tracks(request);
            }
        }
        if link.their_audio_request.is_some() {
            for (alias, name) in &self.audio.tracks {
                if !link.announced.contains(alias) && link.session.publish(name.clone(), *alias) {
                    link.announced.insert(*alias);
                }
            }
        }
    }

    /// A datagram arrived from `peer`. If it is an audio packet to pass on or to play, says
    /// where it goes; the caller sends the same bytes on. `None` for anything else.
    pub fn on_datagram(
        &mut self,
        peer: &N,
        datagram: &[u8],
        now: Timestamp,
    ) -> Option<AudioOut<N>> {
        let link = self.links.get_mut(peer)?;
        link.watch.heard(now);
        let crate::session::DatagramIn::Object {
            alias,
            group,
            properties,
            payload,
            ..
        } = link.session.on_datagram(datagram)
        else {
            // A malformed datagram closes the session; let the caller hear of it.
            self.pump(peer, now);
            return None;
        };
        // Audio is taken only over the tree, and only for tracks the plan knows.
        let wanted = link.selector.is_some()
            && self.audio.tracks.contains_key(&alias)
            && matches!(link.their_tracks.get(&alias), Some(Track::Audio { .. }));
        if !wanted {
            return None;
        }
        let level = int_property(properties, PROPERTY_AUDIO_LEVEL)
            .ok()
            .flatten()
            .unwrap_or(0);
        // The payload is the last thing in a datagram.
        let payload = datagram.len().saturating_sub(payload.len())..datagram.len();
        self.route(Some(peer), alias, group, level, payload, now)
    }

    /// The node's own client produced an audio frame. Appends the datagram to `out` and says
    /// which neighbours to send it to. `None`, appending nothing, if `alias` is not one of
    /// this node's audio tracks in the plan.
    pub fn local_audio(
        &mut self,
        alias: u64,
        group: u64,
        level: u8,
        payload: &[u8],
        out: &mut Vec<u8>,
        now: Timestamp,
    ) -> Option<AudioOut<N>> {
        if !self.audio.own.contains(&alias) || !self.audio.tracks.contains_key(&alias) {
            return None;
        }
        let level = u64::from(level).min(MAX_LEVEL);
        let mut properties = Vec::new();
        let pair = Kvp {
            ty: PROPERTY_AUDIO_LEVEL,
            value: Value::Int(level),
        };
        encode_kvps(&[pair], &mut properties).ok()?;
        let datagram = Datagram {
            track_alias: alias,
            group,
            object: 0,
            priority: None,
            end_of_group: true,
            properties: &properties,
            body: ObjectBody::Payload(payload),
        };
        let start = out.len();
        if datagram.encode(out).is_err() {
            out.truncate(start);
            return None;
        }
        let payload = out.len().saturating_sub(payload.len())..out.len();
        self.route(None, alias, group, level, payload, now)
    }

    /// Decide where a packet of `alias` goes: every tree link but the one it came in on,
    /// and the client, each by its own selection.
    fn route(
        &mut self,
        from: Option<&N>,
        alias: u64,
        group: u64,
        level: u64,
        payload: Range<usize>,
        now: Timestamp,
    ) -> Option<AudioOut<N>> {
        if !self.dedupe.accept(&alias, group, 0) {
            return None;
        }
        let level = u8::try_from(level.min(MAX_LEVEL)).unwrap_or(0);
        let mut forward_to = Vec::new();
        for (peer, link) in &mut self.links {
            if Some(peer) == from || !link.announced.contains(&alias) {
                continue;
            }
            let Some(selector) = &mut link.selector else {
                continue;
            };
            if selector.on_packet(alias, level, now) {
                link.watch.sent(now);
                forward_to.push(peer.clone());
            }
        }
        let for_client =
            !self.audio.own.contains(&alias) && self.client.on_packet(alias, level, now);
        Some(AudioOut {
            forward_to,
            for_client,
            alias,
            group,
            level,
            payload,
        })
    }

    // --- Inside ---

    /// Carry out what `peer`'s session has to say.
    fn pump(&mut self, peer: &N, now: Timestamp) {
        loop {
            let Some(link) = self.links.get_mut(peer) else {
                return;
            };
            if let Some(action) = link.session.poll_action() {
                if matches!(action, Action::Send { .. }) {
                    link.watch.sent(now);
                }
                self.outputs.push_back(Output::Stream {
                    peer: peer.clone(),
                    action,
                });
                continue;
            }
            let Some(event) = link.session.poll_event() else {
                return;
            };
            self.on_event(peer, event, now);
        }
    }

    fn on_event(&mut self, peer: &N, event: Event, now: Timestamp) {
        let Some(link) = self.links.get_mut(peer) else {
            return;
        };
        match event {
            Event::TracksWanted { request, prefix } => {
                if Some(&prefix) == self.control_prefix.as_ref() {
                    link.session.accept_tracks(request);
                    link.their_control_request = Some(request);
                    // The peer gets the latest of every control track, then what follows.
                    for cached in self.flood.connect(peer.clone(), now) {
                        self.send_control(peer, &cached.track, cached.group, &cached.payload);
                    }
                } else if Some(&prefix) == self.audio_prefix.as_ref() {
                    link.session.accept_tracks(request);
                    link.their_audio_request = Some(request);
                    if let Some(mut link) = self.links.remove(peer) {
                        self.wire_audio(peer, &mut link);
                        self.links.insert(peer.clone(), link);
                    }
                } else {
                    link.session
                        .refuse(request, NOT_SUPPORTED, "not a prefix this khef serves");
                }
            }
            Event::TracksUnwanted { request } => {
                if link.their_control_request == Some(request) {
                    link.their_control_request = None;
                    self.flood.disconnect(peer);
                } else if link.their_audio_request == Some(request) {
                    link.their_audio_request = None;
                }
            }
            Event::TrackOffered { track, alias } => {
                // Only tracks of this meeting, named as this meeting names them.
                if let Some(name) = TrackName::parse(&track)
                    && name.meeting == self.config.meeting
                {
                    link.their_tracks.insert(alias, name.track);
                }
            }
            Event::TrackEnded { alias } => {
                link.their_tracks.remove(&alias);
            }
            Event::TrackRefused { alias, .. } => {
                link.announced.remove(&alias);
            }
            Event::Object {
                alias,
                group,
                payload,
                ..
            } => {
                if let Some(track @ (Track::Report { .. } | Track::Plan)) =
                    link.their_tracks.get(&alias)
                {
                    self.outputs.push_back(Output::Control {
                        from: peer.clone(),
                        track: track.clone(),
                        group,
                        payload,
                    });
                }
            }
            Event::TracksRefused { request, .. } => {
                if link.control_request == Some(request) {
                    link.control_request = None;
                } else if link.audio_request == Some(request) {
                    link.audio_request = None;
                }
            }
            // Single tracks by name: how video travels.
            event @ (Event::Wanted { .. }
            | Event::Unwanted { .. }
            | Event::GroupWanted { .. }
            | Event::Subscribed { .. }
            | Event::SubscribeRefused { .. }
            | Event::SubscriptionEnded { .. }
            | Event::Group { .. }
            | Event::GroupReset { .. }) => self.video_event(peer, event, now),
            Event::Ready { .. } | Event::TracksAccepted { .. } | Event::TrackAccepted { .. } => {}
        }
    }
}
