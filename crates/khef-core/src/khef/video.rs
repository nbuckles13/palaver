//! Video through the khef: one track at a time, asked for by name, hop by hop.
//!
//! Sound goes to everyone and the loudest are chosen on the way. Video goes only where
//! someone watches it. A khef that wants a track (for its own client, or for a neighbour
//! that asked) asks the neighbour the plan names as the way to it, and that neighbour
//! does the same, until the asking reaches the node whose camera it is. The pictures
//! then come back down the same line, each group on a stream of its own, and every khef
//! passes the bytes on as they arrive, without looking past the stream's header.
//!
//! Nothing is sent, or even made, that nobody watches: the node with the camera is told
//! when the first watcher arrives and when the last has gone.
//!
//! **Key frames** are asked for as the draft has it (`NEW_GROUP_REQUEST`, section
//! 10.2.19): a request names the group it must be answered by, which is the largest
//! group the asker knows of plus one. A khef that has already seen that group, or has
//! already asked for it, has nothing to pass on, so any number of requests for the same
//! key frame become one, and nobody waits on a clock. How often key frames may be made
//! is the encoder's business alone.

use super::{Khef, Output};
use crate::session::NOT_SUPPORTED;
use crate::{Event, StreamId};
use palaver_moq_wire::FullTrackName;
use palaver_proto::Timestamp;
use std::collections::BTreeMap;
use std::time::Duration;

/// How the khef paces what it asks of others for video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    /// How long after a neighbour refuses or ends a track before it is asked again. (It
    /// may only not have had the plan yet.)
    pub retry_after: Duration,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            retry_after: Duration::from_millis(500),
        }
    }
}

/// Where a video track comes from, as this khef sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoFrom<N> {
    /// This node's own camera or screen.
    Here,
    /// The neighbour on the way to the node that makes it.
    Link(N),
}

/// One video track of the meeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoRoute<N> {
    pub name: FullTrackName,
    pub from: VideoFrom<N>,
}

/// What the plan says about video, as far as this khef is concerned: every video track
/// by the alias the plan gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPlan<N> {
    pub tracks: BTreeMap<u64, VideoRoute<N>>,
}

impl<N> Default for VideoPlan<N> {
    fn default() -> Self {
        Self {
            tracks: BTreeMap::new(),
        }
    }
}

/// What a group being passed on is a copy of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    /// A stream of the neighbour the track comes from.
    Stream(StreamId),
    /// A group of this node's own, by its number.
    Local(u64),
}

/// A group part way through: where its bytes are going.
struct Open<N> {
    group: u64,
    to: Vec<(N, StreamId)>,
    client: bool,
}

/// A video track somebody wants from this khef.
pub(super) struct Watched<N> {
    /// The node's own client watches it.
    client: bool,
    /// The neighbours that asked for it, each with its request.
    links: BTreeMap<N, StreamId>,
    /// Our own request to the neighbour it comes from.
    upstream: Option<(N, StreamId)>,
    /// For a track of this node's own: the client has been told to make it.
    producing: bool,
    open: BTreeMap<Origin, Open<N>>,
    /// The largest group of the track seen to begin.
    largest: Option<u64>,
    /// The group we have asked for and not yet seen: a request for that group or an
    /// earlier one has nothing to add.
    outstanding: Option<u64>,
    /// When to ask the neighbour again, after it said no.
    retry: Option<Timestamp>,
}

impl<N> Default for Watched<N> {
    fn default() -> Self {
        Self {
            client: false,
            links: BTreeMap::new(),
            upstream: None,
            producing: false,
            open: BTreeMap::new(),
            largest: None,
            outstanding: None,
            retry: None,
        }
    }
}

impl<N> Watched<N> {
    /// The group that would be new to someone who has none of those begun so far: what a
    /// request for a key frame names. Zero if no group has been seen.
    fn next_group(&self) -> u64 {
        self.largest.map_or(0, |largest| largest.saturating_add(1))
    }
}

impl<N: Ord + Clone> Khef<N> {
    /// The plan changed what this khef does with video. What is being watched goes on
    /// being watched, by the new way if the way changed.
    pub fn set_video_plan(&mut self, plan: VideoPlan<N>, now: Timestamp) {
        let old = core::mem::replace(&mut self.video, plan);
        let watched: Vec<u64> = self.watching.keys().copied().collect();
        for alias in watched {
            let (was, is) = (old.tracks.get(&alias), self.video.tracks.get(&alias));
            if was != is {
                let same_track = matches!((was, is), (Some(a), Some(b)) if a.name == b.name);
                self.cut(alias, now);
                if !same_track {
                    self.end(alias, now);
                }
            }
            self.feed(alias, true, now);
        }
    }

    /// The node's own client wants to see the track with `alias`. It stays wanted, through
    /// changes of plan, until [`Khef::unwatch`].
    pub fn watch(&mut self, alias: u64, now: Timestamp) {
        let watched = self.watching.entry(alias).or_default();
        if watched.client {
            return;
        }
        watched.client = true;
        self.feed(alias, true, now);
    }

    /// The node's own client no longer wants the track.
    pub fn unwatch(&mut self, alias: u64, now: Timestamp) {
        let Some(watched) = self.watching.get_mut(&alias) else {
            return;
        };
        watched.client = false;
        for open in watched.open.values_mut() {
            open.client = false;
        }
        self.starve(alias, now);
    }

    /// The node's own client has lost its place in a track it watches, and needs a key
    /// frame to go on: a group after any that has begun.
    pub fn ask_key_frame(&mut self, alias: u64, now: Timestamp) {
        if let Some(watched) = self.watching.get(&alias)
            && watched.client
        {
            self.key(alias, watched.next_group(), now);
        }
    }

    /// Bytes of a group of this node's own video track, in order: the first (`start`)
    /// begin with the stream's header, as [`crate::GroupWriter`] writes it, and `fin`
    /// ends the group. They go to every neighbour that asked for the track. Nothing
    /// happens unless the khef has said to make the track ([`Output::Produce`]).
    pub fn local_video(
        &mut self,
        alias: u64,
        group: u64,
        start: bool,
        bytes: &[u8],
        fin: bool,
        now: Timestamp,
    ) {
        if self.watching.get(&alias).is_some_and(|w| w.producing) {
            let piece = Piece {
                group,
                start,
                bytes: bytes.to_vec(),
                fin,
            };
            self.carry(alias, Origin::Local(group), piece, now);
        }
    }

    /// Give up a group of this node's own part way: what was to come of it is no longer
    /// worth sending.
    pub fn local_video_reset(&mut self, alias: u64, group: u64, now: Timestamp) {
        self.drop_group(alias, Origin::Local(group), now);
    }

    /// When the khef next has something to do for video, if it does.
    pub(super) fn video_wake(&self) -> Option<Timestamp> {
        self.watching
            .values()
            .filter_map(|watched| watched.retry)
            .min()
    }

    /// Make the requests that were waiting for their time.
    pub(super) fn video_poll(&mut self, now: Timestamp) {
        let watched: Vec<u64> = self.watching.keys().copied().collect();
        for alias in watched {
            let Some(watched) = self.watching.get_mut(&alias) else {
                continue;
            };
            if watched.retry.is_some_and(|at| at <= now) {
                watched.retry = None;
                self.feed(alias, true, now);
            }
        }
    }

    /// A link came up: ask over it for what is watched and comes that way.
    pub(super) fn video_link_up(&mut self, now: Timestamp) {
        let watched: Vec<u64> = self.watching.keys().copied().collect();
        for alias in watched {
            self.feed(alias, true, now);
        }
    }

    /// The link to `peer` is gone, and its session with it.
    pub(super) fn video_link_down(&mut self, peer: &N, now: Timestamp) {
        let watched: Vec<u64> = self.watching.keys().copied().collect();
        for alias in watched {
            let Some(watched) = self.watching.get_mut(&alias) else {
                continue;
            };
            watched.links.remove(peer);
            for open in watched.open.values_mut() {
                open.to.retain(|(to, _)| to != peer);
            }
            if watched.upstream.as_ref().is_some_and(|(up, _)| up == peer) {
                watched.upstream = None;
                self.lost(alias, now);
                // Asked again when a link to that neighbour is up, or the plan changes.
                if let Some(watched) = self.watching.get_mut(&alias) {
                    watched.retry = None;
                }
            }
            self.starve(alias, now);
        }
    }

    /// Something happened on `peer`'s session that concerns video.
    pub(super) fn video_event(&mut self, peer: &N, event: Event, now: Timestamp) {
        match event {
            Event::Wanted {
                request,
                track,
                new_group,
            } => self.wanted(peer, request, &track, new_group, now),
            Event::Unwanted { request } => {
                if let Some(alias) = self.asked_by(peer, request) {
                    if let Some(watched) = self.watching.get_mut(&alias) {
                        watched.links.remove(peer);
                        // The session has already given up the groups it had open.
                        for open in watched.open.values_mut() {
                            open.to.retain(|(to, _)| to != peer);
                        }
                    }
                    self.starve(alias, now);
                }
            }
            Event::GroupWanted { request, group } => {
                if let Some(alias) = self.asked_by(peer, request) {
                    self.key(alias, group, now);
                }
            }
            Event::Subscribed { request, alias } => {
                // Every khef uses the plan's alias: that is what lets a group pass
                // through unchanged. A neighbour that answers with another cannot be used.
                if let Some(ours) = self.asked_of(peer, request)
                    && ours != alias
                {
                    self.cut(ours, now);
                    self.lost(ours, now);
                }
            }
            Event::SubscribeRefused { request, .. } | Event::SubscriptionEnded { request } => {
                if let Some(alias) = self.asked_of(peer, request) {
                    if let Some(watched) = self.watching.get_mut(&alias) {
                        watched.upstream = None;
                    }
                    self.lost(alias, now);
                }
            }
            Event::Group {
                stream,
                alias,
                group,
                start,
                bytes,
                fin,
            } => {
                let from_there = self
                    .watching
                    .get(&alias)
                    .and_then(|watched| watched.upstream.as_ref())
                    .is_some_and(|(up, _)| up == peer);
                if from_there {
                    let piece = Piece {
                        group,
                        start,
                        bytes,
                        fin,
                    };
                    self.carry(alias, Origin::Stream(stream), piece, now);
                }
            }
            Event::GroupReset { stream } => {
                let origin = Origin::Stream(stream);
                let alias = self.watching.iter().find_map(|(alias, watched)| {
                    let from_there = watched.upstream.as_ref().is_some_and(|(up, _)| up == peer);
                    (from_there && watched.open.contains_key(&origin)).then_some(*alias)
                });
                if let Some(alias) = alias {
                    self.drop_group(alias, origin, now);
                }
            }
            _ => {}
        }
    }

    /// `peer` asks for a track by name.
    fn wanted(
        &mut self,
        peer: &N,
        request: StreamId,
        track: &FullTrackName,
        new_group: Option<u64>,
        now: Timestamp,
    ) {
        let found = self
            .video
            .tracks
            .iter()
            .find(|(_, route)| route.name == *track)
            // A track is not sent back the way it comes, nor twice to one neighbour.
            .filter(|(alias, route)| {
                route.from != VideoFrom::Link(peer.clone())
                    && !self
                        .watching
                        .get(alias)
                        .is_some_and(|watched| watched.links.contains_key(peer))
            })
            .map(|(alias, _)| *alias);
        let Some(link) = self.links.get_mut(peer) else {
            return;
        };
        let accepted = found.filter(|alias| link.session.accept_subscription(request, *alias));
        let Some(alias) = accepted else {
            link.session.refuse(
                request,
                NOT_SUPPORTED,
                "not a track this khef can send here",
            );
            return;
        };
        self.watching
            .entry(alias)
            .or_default()
            .links
            .insert(peer.clone(), request);
        // Whatever group the neighbour named, it is new here and has none of the track:
        // what it needs is a group after any that has begun.
        self.feed(alias, new_group.is_some(), now);
    }

    /// The track `peer` asked for with `request`.
    fn asked_by(&self, peer: &N, request: StreamId) -> Option<u64> {
        self.watching
            .iter()
            .find(|(_, watched)| watched.links.get(peer) == Some(&request))
            .map(|(alias, _)| *alias)
    }

    /// The track we asked `peer` for with `request`.
    fn asked_of(&self, peer: &N, request: StreamId) -> Option<u64> {
        self.watching
            .iter()
            .find(|(_, watched)| {
                watched
                    .upstream
                    .as_ref()
                    .is_some_and(|(up, ours)| up == peer && *ours == request)
            })
            .map(|(alias, _)| *alias)
    }

    /// See that a watched track is on its way here: ask the neighbour it comes from, or
    /// tell the client to make it. If it already is and `key`, ask for a key frame, for
    /// whoever has just begun to watch.
    fn feed(&mut self, alias: u64, key: bool, now: Timestamp) {
        let Some(route) = self.video.tracks.get(&alias) else {
            return;
        };
        let Some(watched) = self.watching.get_mut(&alias) else {
            return;
        };
        if !watched.client && watched.links.is_empty() {
            return;
        }
        match &route.from {
            VideoFrom::Here if !watched.producing => {
                watched.producing = true;
                // What begins, begins with a key frame.
                watched.outstanding = Some(0);
                self.outputs.push_back(Output::Produce {
                    alias,
                    wanted: true,
                });
            }
            VideoFrom::Link(up) if watched.upstream.is_none() => {
                let up = up.clone();
                let Some(link) = self.links.get_mut(&up) else {
                    return;
                };
                // What begins for us begins with a key frame.
                if let Some(request) = link.session.subscribe(route.name.clone(), Some(0)) {
                    watched.upstream = Some((up.clone(), request));
                    watched.outstanding = Some(0);
                    watched.retry = None;
                }
                self.pump(&up, now);
            }
            _ if key => {
                let group = watched.next_group();
                self.key(alias, group, now);
            }
            _ => {}
        }
    }

    /// Ask for a key frame on a track that is on its way here: a new group numbered at
    /// least `group` (zero: any). Nothing is asked if that group has already begun, or
    /// has already been asked for.
    fn key(&mut self, alias: u64, group: u64, now: Timestamp) {
        let Some(watched) = self.watching.get_mut(&alias) else {
            return;
        };
        let seen = group != 0 && watched.largest.is_some_and(|largest| group <= largest);
        let asked = watched.outstanding.is_some_and(|asked| group <= asked);
        if seen || asked {
            return;
        }
        if watched.producing {
            watched.outstanding = Some(group);
            self.outputs.push_back(Output::KeyFrame { alias });
        } else if let Some((up, request)) = watched.upstream.clone() {
            watched.outstanding = Some(group);
            if let Some(link) = self.links.get_mut(&up) {
                link.session.ask_for_group(request, group);
            }
            self.pump(&up, now);
        }
    }

    /// Pass on the next bytes of a group.
    fn carry(&mut self, alias: u64, origin: Origin, piece: Piece, now: Timestamp) {
        let Some(watched) = self.watching.get_mut(&alias) else {
            return;
        };
        if piece.start {
            if watched.largest.is_none_or(|largest| piece.group > largest) {
                watched.largest = Some(piece.group);
                watched.outstanding = None;
            }
            let mut to = Vec::new();
            for peer in watched.links.keys() {
                if let Some(link) = self.links.get_mut(peer)
                    && let Some(stream) = link.session.open_group(alias)
                {
                    to.push((peer.clone(), stream));
                }
            }
            let open = Open {
                group: piece.group,
                to,
                // The client does not watch its own camera through the khef.
                client: watched.client && matches!(origin, Origin::Stream(_)),
            };
            watched.open.insert(origin, open);
        }
        // Whoever began to watch after the group did waits for the next.
        let Some(open) = watched.open.get(&origin) else {
            return;
        };
        let (to, client) = (open.to.clone(), open.client);
        if piece.fin {
            watched.open.remove(&origin);
        }
        for (peer, stream) in &to {
            if let Some(link) = self.links.get_mut(peer) {
                link.session.pass(*stream, piece.bytes.clone(), piece.fin);
            }
        }
        if client {
            self.outputs.push_back(Output::Video {
                alias,
                group: piece.group,
                start: piece.start,
                bytes: piece.bytes,
                fin: piece.fin,
            });
        }
        for (peer, _) in &to {
            self.pump(peer, now);
        }
    }

    /// Give up a group part way, wherever it was going.
    fn drop_group(&mut self, alias: u64, origin: Origin, now: Timestamp) {
        let Some(open) = self
            .watching
            .get_mut(&alias)
            .and_then(|watched| watched.open.remove(&origin))
        else {
            return;
        };
        for (peer, stream) in &open.to {
            if let Some(link) = self.links.get_mut(peer) {
                link.session.reset_group(*stream);
            }
            self.pump(peer, now);
        }
        if open.client {
            self.outputs.push_back(Output::VideoReset {
                alias,
                group: open.group,
            });
        }
    }

    /// Stop taking a track from where it has been coming: give up our request or tell the
    /// client to stop making it, and give up the groups part way through.
    fn cut(&mut self, alias: u64, now: Timestamp) {
        let Some(watched) = self.watching.get_mut(&alias) else {
            return;
        };
        watched.outstanding = None;
        watched.retry = None;
        if core::mem::take(&mut watched.producing) {
            self.outputs.push_back(Output::Produce {
                alias,
                wanted: false,
            });
        }
        let upstream = watched.upstream.take();
        let open: Vec<Origin> = watched.open.keys().copied().collect();
        if let Some((up, request)) = upstream {
            if let Some(link) = self.links.get_mut(&up) {
                link.session.unsubscribe(request);
            }
            self.pump(&up, now);
        }
        for origin in open {
            self.drop_group(alias, origin, now);
        }
    }

    /// The neighbour a track came from will send no more of it for now: give up the
    /// groups part way through, and ask again in a while.
    fn lost(&mut self, alias: u64, now: Timestamp) {
        let retry_after = self.config.video.retry_after;
        let Some(watched) = self.watching.get_mut(&alias) else {
            return;
        };
        watched.outstanding = None;
        watched.retry = Some(now.saturating_add(retry_after));
        let open: Vec<Origin> = watched.open.keys().copied().collect();
        for origin in open {
            self.drop_group(alias, origin, now);
        }
    }

    /// The track is no longer in the plan: tell the neighbours that asked for it. The
    /// client's wish for it is kept, in case the track returns.
    fn end(&mut self, alias: u64, now: Timestamp) {
        let Some(watched) = self.watching.get_mut(&alias) else {
            return;
        };
        let links = core::mem::take(&mut watched.links);
        if !watched.client {
            self.watching.remove(&alias);
        }
        for (peer, request) in links {
            if let Some(link) = self.links.get_mut(&peer) {
                link.session.end_subscription(request);
            }
            self.pump(&peer, now);
        }
    }

    /// If nobody watches a track any more, stop it coming.
    fn starve(&mut self, alias: u64, now: Timestamp) {
        let unwatched = self
            .watching
            .get(&alias)
            .is_some_and(|watched| !watched.client && watched.links.is_empty());
        if unwatched {
            self.cut(alias, now);
            self.watching.remove(&alias);
        }
    }
}

/// The next bytes of a group.
struct Piece {
    group: u64,
    start: bool,
    bytes: Vec<u8>,
    fin: bool,
}
