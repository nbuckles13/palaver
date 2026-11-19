//! One MoQ session between two khefs: set-up and the requests that announce tracks.
//!
//! Sans-IO. The caller owns the connection and tells the session what arrives on which
//! stream; the session says what to open, send and close ([`Action`]) and what happened
//! ([`Event`]). Draft 22's layout (section 6.3):
//!
//! - each side opens one unidirectional control stream that starts with SETUP;
//! - every request has its own bidirectional stream, the response coming back on it;
//! - objects travel on further unidirectional streams and in datagrams.
//!
//! This first part covers the two requests khefs use with each other:
//!
//! - SUBSCRIBE_TRACKS: "announce to me every track under this prefix";
//! - PUBLISH: "here is a track, and the alias its objects will carry".
//!
//! Object streams and datagrams, and SUBSCRIBE for a single track, are not handled yet.
//!
//! Anything the peer does that the draft forbids closes the session; the reason is kept for
//! the log. Nothing here panics on input.

use palaver_moq_wire::{
    Error, Frame, FullTrackName, Message, Namespace, Params, Publish, PublishDone, Reader,
    RequestError, RequestOk, Setup, SubscribeTracks, UniStream, negotiated_palaver_version,
    uni_stream,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Session termination codes (section 12.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCode {
    InternalError = 0x1,
    ProtocolViolation = 0x3,
    InvalidRequestId = 0x4,
    DuplicateTrackAlias = 0x5,
    KeyValueFormatting = 0x6,
}

/// Request error codes (section 12.3) this profile sends.
pub const NOT_SUPPORTED: u64 = 0x3;
pub const UNINTERESTED: u64 = 0x20;
pub const PREFIX_OVERLAP: u64 = 0x30;
/// PUBLISH_DONE status: the track is no longer being published (section 12.4).
pub const TRACK_ENDED: u64 = 0x2;

/// Most bytes buffered for one stream while waiting for the rest of a message: the longest
/// message, its type and its length.
const MAX_BUFFERED: usize = palaver_moq_wire::MAX_BODY_BYTES + 10;
/// How many of the peer's request ids are remembered to catch a repeat.
const REQUEST_ID_WINDOW: usize = 4096;

/// Which side of the connection this is. It decides only the numbering of requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The side that dialled: even request ids.
    Client,
    /// The side that accepted: odd request ids.
    Server,
}

/// Names a stream of the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StreamId {
    /// A stream this session asked to be opened; the number is the session's own.
    Ours(u64),
    /// A stream the peer opened; the number is the caller's, unique per stream.
    Theirs(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Uni,
    Bidi,
}

/// Something for the caller to do to the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Open a stream; what to send on it follows.
    Open { stream: StreamId, kind: StreamKind },
    /// Send bytes on a stream, then finish our direction of it if `fin`.
    Send {
        stream: StreamId,
        bytes: Vec<u8>,
        fin: bool,
    },
    /// Abandon a request stream: reset our direction and ask the peer to stop sending.
    Reset { stream: StreamId },
    /// Close the connection. Nothing follows.
    Close {
        code: CloseCode,
        reason: &'static str,
    },
}

/// Something that happened, for the khef above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Both SETUPs are in. `palaver_version` is `None` if the peer is not a Palaver endpoint.
    Ready { palaver_version: Option<u64> },
    /// The peer asks for every track under `prefix`. Answer with [`Session::accept_tracks`]
    /// or [`Session::refuse`].
    TracksWanted {
        request: StreamId,
        prefix: Namespace,
    },
    /// The peer no longer wants them.
    TracksUnwanted { request: StreamId },
    /// The peer accepted our request for tracks.
    TracksAccepted { request: StreamId },
    /// The peer refused it, or abandoned it.
    TracksRefused { request: StreamId, code: u64 },
    /// The peer announced a track under a prefix we asked for. Its objects carry `alias`.
    TrackOffered { track: FullTrackName, alias: u64 },
    /// The peer has stopped publishing a track it announced.
    TrackEnded { alias: u64 },
    /// The peer accepted a track we announced.
    TrackAccepted { alias: u64 },
    /// The peer refused a track we announced, or abandoned it. Stop sending its objects.
    TrackRefused { alias: u64, code: u64 },
}

/// A request the peer made, by the stream it arrived on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Incoming {
    /// SUBSCRIBE_TRACKS, not yet answered.
    TracksPending { prefix: Namespace },
    /// SUBSCRIBE_TRACKS, accepted.
    Tracks { prefix: Namespace },
    /// PUBLISH, accepted: we receive the track.
    Track { alias: u64 },
    /// Answered with an error, or finished.
    Done,
}

/// A request we made, by the stream we opened for it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outgoing {
    Tracks { prefix: Namespace, answered: bool },
    Track { alias: u64, answered: bool },
}

#[derive(Debug)]
struct Inbound {
    kind: StreamKind,
    buffer: Vec<u8>,
    /// Known once the first bytes of a unidirectional stream have been read.
    uni: Option<UniStream>,
    finished: bool,
}

/// One MoQ session.
#[derive(Debug)]
pub struct Session {
    role: Role,
    ours: Setup,
    theirs: Option<Setup>,
    started: bool,
    closed: bool,
    next_stream: u64,
    next_request_id: u64,
    next_alias: u64,
    /// The peer's control stream, once seen.
    their_control: Option<StreamId>,
    inbound: BTreeMap<StreamId, Inbound>,
    incoming: BTreeMap<StreamId, Incoming>,
    outgoing: BTreeMap<StreamId, Outgoing>,
    /// Request ids the peer has used, within a window.
    seen_request_ids: BTreeSet<u64>,
    /// Aliases of the tracks the peer publishes to us.
    their_aliases: BTreeSet<u64>,
    actions: VecDeque<Action>,
    events: VecDeque<Event>,
}

fn overlaps(a: &Namespace, b: &Namespace) -> bool {
    a.is_prefix_of(b) || b.is_prefix_of(a)
}

impl Session {
    /// `setup` is what this side announces about itself.
    pub fn new(role: Role, setup: Setup) -> Self {
        Self {
            role,
            ours: setup,
            theirs: None,
            started: false,
            closed: false,
            next_stream: 0,
            next_request_id: match role {
                Role::Client => 0,
                Role::Server => 1,
            },
            next_alias: 0,
            their_control: None,
            inbound: BTreeMap::new(),
            incoming: BTreeMap::new(),
            outgoing: BTreeMap::new(),
            seen_request_ids: BTreeSet::new(),
            their_aliases: BTreeSet::new(),
            actions: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    /// The next thing to do to the connection, if any. Drain after every call into the session.
    pub fn poll_action(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    /// The next thing that happened, if any. Drain after every call into the session.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Whether both SETUPs have been exchanged and the session is open.
    pub fn is_ready(&self) -> bool {
        self.started && self.theirs.is_some() && !self.closed
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The Palaver extension version both sides speak, once ready.
    pub fn palaver_version(&self) -> Option<u64> {
        negotiated_palaver_version(&self.ours, self.theirs.as_ref()?)
    }

    // --- What the khef asks for ---

    /// The connection is up: open our control stream and send SETUP. Requests may be made
    /// straight away, without waiting for the peer's SETUP.
    pub fn start(&mut self) {
        if self.started || self.closed {
            return;
        }
        self.started = true;
        let stream = self.open(StreamKind::Uni);
        let mut bytes = Vec::new();
        if self.ours.encode(&mut bytes).is_err() {
            return self.close(CloseCode::InternalError, "our SETUP does not encode");
        }
        self.send(stream, bytes, false);
    }

    /// Ask the peer to announce every track under `prefix`. `None` if the session is closed
    /// or the prefix overlaps one already asked for (the peer would refuse it).
    pub fn subscribe_tracks(&mut self, prefix: Namespace) -> Option<StreamId> {
        let clash = self.outgoing.values().any(|request| match request {
            Outgoing::Tracks { prefix: held, .. } => overlaps(held, &prefix),
            Outgoing::Track { .. } => false,
        });
        if self.closed || clash {
            return None;
        }
        let message = Message::SubscribeTracks(SubscribeTracks {
            request_id: self.take_request_id(),
            prefix: prefix.clone(),
            params: Params::default(),
        });
        let stream = self.request(&message)?;
        self.outgoing.insert(
            stream,
            Outgoing::Tracks {
                prefix,
                answered: false,
            },
        );
        Some(stream)
    }

    /// Announce a track to the peer. Returns the alias its objects must carry; they may be
    /// sent at once, before the peer answers. `None` if the session is closed.
    pub fn publish(&mut self, track: FullTrackName) -> Option<u64> {
        if self.closed {
            return None;
        }
        let alias = self.next_alias;
        self.next_alias = self.next_alias.saturating_add(1);
        let message = Message::Publish(Publish {
            request_id: self.take_request_id(),
            track,
            track_alias: alias,
            params: Params {
                forward: Some(true),
                ..Params::default()
            },
            track_properties: Vec::new(),
        });
        let stream = self.request(&message)?;
        self.outgoing.insert(
            stream,
            Outgoing::Track {
                alias,
                answered: false,
            },
        );
        Some(alias)
    }

    /// Tell the peer a track we announced has ended.
    pub fn end_track(&mut self, alias: u64) {
        let Some(stream) = self.outgoing.iter().find_map(|(stream, request)| {
            matches!(request, Outgoing::Track { alias: a, .. } if *a == alias).then_some(*stream)
        }) else {
            return;
        };
        self.outgoing.remove(&stream);
        let done = Message::PublishDone(PublishDone {
            status: TRACK_ENDED,
            stream_count: 0,
            reason: String::new(),
        });
        self.send_message(stream, &done, true);
    }

    /// Accept the peer's request for tracks: it now expects a PUBLISH for each one.
    pub fn accept_tracks(&mut self, request: StreamId) {
        let Some(Incoming::TracksPending { prefix }) = self.incoming.get(&request) else {
            return;
        };
        let prefix = prefix.clone();
        self.incoming.insert(request, Incoming::Tracks { prefix });
        let ok = Message::RequestOk(RequestOk {
            params: Params::default(),
            track_properties: Vec::new(),
        });
        self.send_message(request, &ok, false);
    }

    /// Refuse a request the peer made that has not been answered yet.
    pub fn refuse(&mut self, request: StreamId, code: u64, reason: &str) {
        if !matches!(
            self.incoming.get(&request),
            Some(Incoming::TracksPending { .. })
        ) {
            return;
        }
        self.reject(request, code, reason);
    }

    /// Give up a request for tracks we made.
    pub fn unsubscribe_tracks(&mut self, request: StreamId) {
        if matches!(self.outgoing.get(&request), Some(Outgoing::Tracks { .. })) {
            self.outgoing.remove(&request);
            self.actions.push_back(Action::Reset { stream: request });
        }
    }

    // --- What the connection reports ---

    /// Bytes arrived on a stream. For a stream the peer opened, `kind` says which sort it is
    /// (it is ignored for our own streams). `fin` means the peer finished its direction.
    pub fn on_data(&mut self, stream: StreamId, kind: StreamKind, bytes: &[u8], fin: bool) {
        if self.closed {
            return;
        }
        let kind = match stream {
            StreamId::Ours(_) => StreamKind::Bidi,
            StreamId::Theirs(_) => kind,
        };
        let inbound = self.inbound.entry(stream).or_insert(Inbound {
            kind,
            buffer: Vec::new(),
            uni: None,
            finished: false,
        });
        if inbound.finished {
            return self.close(
                CloseCode::ProtocolViolation,
                "data after the end of a stream",
            );
        }
        inbound.finished = fin;
        // Object streams are not read yet: nothing of theirs is kept.
        if matches!(inbound.uni, Some(UniStream::Subgroup | UniStream::Padding)) {
            return;
        }
        if inbound.buffer.len().saturating_add(bytes.len()) > MAX_BUFFERED {
            return self.close(CloseCode::ProtocolViolation, "message too long");
        }
        inbound.buffer.extend_from_slice(bytes);
        let kind = inbound.kind;
        self.drain(stream, kind);
    }

    /// The peer reset a stream, or asked us to stop sending on it.
    pub fn on_reset(&mut self, stream: StreamId) {
        if self.closed {
            return;
        }
        self.inbound.remove(&stream);
        if Some(stream) == self.their_control {
            return self.close(CloseCode::ProtocolViolation, "control stream closed");
        }
        self.abandoned(stream);
    }

    // --- Inside ---

    fn open(&mut self, kind: StreamKind) -> StreamId {
        let stream = StreamId::Ours(self.next_stream);
        self.next_stream = self.next_stream.saturating_add(1);
        self.actions.push_back(Action::Open { stream, kind });
        stream
    }

    fn send(&mut self, stream: StreamId, bytes: Vec<u8>, fin: bool) {
        self.actions.push_back(Action::Send { stream, bytes, fin });
    }

    fn send_message(&mut self, stream: StreamId, message: &Message, fin: bool) {
        let mut bytes = Vec::new();
        if message.encode(&mut bytes).is_err() {
            return self.close(CloseCode::InternalError, "a message does not encode");
        }
        self.send(stream, bytes, fin);
    }

    /// Open a request stream carrying `message`.
    fn request(&mut self, message: &Message) -> Option<StreamId> {
        let mut bytes = Vec::new();
        message.encode(&mut bytes).ok()?;
        let stream = self.open(StreamKind::Bidi);
        self.send(stream, bytes, false);
        Some(stream)
    }

    fn take_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(2);
        id
    }

    fn close(&mut self, code: CloseCode, reason: &'static str) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.inbound.clear();
        self.actions.push_back(Action::Close { code, reason });
    }

    fn fail(&mut self, error: Error) {
        match error {
            Error::NeedMore => {}
            Error::Violation(reason) => self.close(CloseCode::ProtocolViolation, reason),
            Error::KeyValueFormatting(reason) => self.close(CloseCode::KeyValueFormatting, reason),
        }
    }

    fn reject(&mut self, request: StreamId, code: u64, reason: &str) {
        self.incoming.insert(request, Incoming::Done);
        let error = Message::RequestError(RequestError {
            code,
            retry_after: None,
            reason: reason.to_owned(),
            redirect: None,
        });
        self.send_message(request, &error, true);
    }

    /// Read every whole message buffered for `stream`.
    fn drain(&mut self, stream: StreamId, kind: StreamKind) {
        // The first bytes of a unidirectional stream say what it is.
        if kind == StreamKind::Uni && !self.classify(stream) {
            return;
        }
        // Requests wait until the peer has introduced itself (section 6.3).
        if self.theirs.is_none() && Some(stream) != self.their_control {
            return;
        }
        loop {
            if self.closed {
                return;
            }
            let Some(inbound) = self.inbound.get_mut(&stream) else {
                return;
            };
            let buffer = core::mem::take(&mut inbound.buffer);
            let mut reader = Reader::new(&buffer);
            let message = match Frame::decode(&mut reader).and_then(Message::decode) {
                Ok(message) => message,
                Err(Error::NeedMore) => {
                    let finished = inbound.finished;
                    let empty = buffer.is_empty();
                    inbound.buffer = buffer;
                    if finished {
                        self.finished(stream, empty);
                    }
                    return;
                }
                Err(error) => return self.fail(error),
            };
            let used = buffer.len().saturating_sub(reader.remaining());
            if let Some(inbound) = self.inbound.get_mut(&stream) {
                inbound.buffer = buffer.get(used..).unwrap_or_default().to_vec();
            }
            self.on_message(stream, message);
        }
    }

    /// Work out what a unidirectional stream is. `false` if that is not known yet, or the
    /// stream is not one this part of the session reads.
    fn classify(&mut self, stream: StreamId) -> bool {
        let Some(inbound) = self.inbound.get_mut(&stream) else {
            return false;
        };
        if inbound.uni.is_none() {
            let first = match Reader::new(&inbound.buffer).vi64() {
                Ok(first) => first,
                Err(error) => {
                    self.fail(error);
                    return false;
                }
            };
            match uni_stream(first) {
                Ok(sort) => inbound.uni = Some(sort),
                Err(error) => {
                    self.fail(error);
                    return false;
                }
            }
        }
        match inbound.uni {
            Some(UniStream::Control) => {
                if self.their_control.is_some_and(|control| control != stream) {
                    self.close(CloseCode::ProtocolViolation, "a second control stream");
                    return false;
                }
                self.their_control = Some(stream);
                true
            }
            _ => {
                inbound.buffer = Vec::new();
                false
            }
        }
    }

    /// The peer finished its direction of `stream`, with nothing left to read on it (or, if
    /// not `clean`, part of a message).
    fn finished(&mut self, stream: StreamId, clean: bool) {
        if Some(stream) == self.their_control {
            return self.close(CloseCode::ProtocolViolation, "control stream closed");
        }
        if !clean {
            return self.close(
                CloseCode::ProtocolViolation,
                "stream ended inside a message",
            );
        }
        self.inbound.remove(&stream);
        match stream {
            // The peer will send nothing more on a request it made. For a request for
            // tracks that is allowed and changes nothing; a track it published is over.
            StreamId::Theirs(_) => {
                if let Some(Incoming::Track { alias }) = self.incoming.get(&stream) {
                    let alias = *alias;
                    self.end_their_track(stream, alias);
                }
            }
            // The peer will send nothing more on a request we made: unanswered, it failed.
            StreamId::Ours(_) => {
                let unanswered = match self.outgoing.get(&stream) {
                    Some(Outgoing::Tracks { answered, .. } | Outgoing::Track { answered, .. }) => {
                        !answered
                    }
                    None => false,
                };
                if unanswered {
                    self.abandoned(stream);
                }
            }
        }
    }

    /// The peer gave up on `stream`.
    fn abandoned(&mut self, stream: StreamId) {
        match self.incoming.remove(&stream) {
            Some(Incoming::Tracks { .. } | Incoming::TracksPending { .. }) => {
                self.events
                    .push_back(Event::TracksUnwanted { request: stream });
            }
            Some(Incoming::Track { alias }) => {
                self.their_aliases.remove(&alias);
                self.events.push_back(Event::TrackEnded { alias });
            }
            Some(Incoming::Done) | None => {}
        }
        match self.outgoing.remove(&stream) {
            Some(Outgoing::Tracks { .. }) => self.events.push_back(Event::TracksRefused {
                request: stream,
                code: 0,
            }),
            Some(Outgoing::Track { alias, .. }) => {
                self.events
                    .push_back(Event::TrackRefused { alias, code: 0 });
            }
            None => {}
        }
    }

    fn end_their_track(&mut self, stream: StreamId, alias: u64) {
        self.incoming.insert(stream, Incoming::Done);
        self.their_aliases.remove(&alias);
        self.events.push_back(Event::TrackEnded { alias });
    }

    fn on_message(&mut self, stream: StreamId, message: Message) {
        if Some(stream) == self.their_control {
            return self.on_control(message);
        }
        match stream {
            StreamId::Theirs(_) => self.on_their_request(stream, message),
            StreamId::Ours(_) => self.on_response(stream, message),
        }
    }

    fn on_control(&mut self, message: Message) {
        match message {
            Message::Setup(setup) if self.theirs.is_none() => {
                let palaver_version = negotiated_palaver_version(&self.ours, &setup);
                self.theirs = Some(setup);
                self.events.push_back(Event::Ready { palaver_version });
                // Requests that arrived before the SETUP can be read now.
                let waiting: Vec<StreamId> = self
                    .inbound
                    .iter()
                    .filter(|(_, inbound)| inbound.kind == StreamKind::Bidi)
                    .map(|(stream, _)| *stream)
                    .collect();
                for stream in waiting {
                    self.drain(stream, StreamKind::Bidi);
                }
            }
            Message::Setup(_) => self.close(CloseCode::ProtocolViolation, "a second SETUP"),
            // The peer is going away. Nothing to migrate to between khefs: the plan decides
            // who connects to whom, and the peer closing is seen as a link going down.
            Message::GoAway(_) => {}
            _ => self.close(
                CloseCode::ProtocolViolation,
                "a request on the control stream",
            ),
        }
    }

    /// Check a request id the peer used: its parity, and that it is not a repeat.
    fn their_request_id_is_valid(&mut self, id: u64) -> bool {
        let their_parity = match self.role {
            Role::Client => 1,
            Role::Server => 0,
        };
        let below_window = self.seen_request_ids.len() >= REQUEST_ID_WINDOW
            && self
                .seen_request_ids
                .first()
                .is_some_and(|oldest| id < *oldest);
        if id % 2 != their_parity || below_window || !self.seen_request_ids.insert(id) {
            self.close(CloseCode::InvalidRequestId, "bad or repeated request id");
            return false;
        }
        while self.seen_request_ids.len() > REQUEST_ID_WINDOW {
            self.seen_request_ids.pop_first();
        }
        true
    }

    fn on_their_request(&mut self, stream: StreamId, message: Message) {
        let Some(held) = self.incoming.get(&stream).cloned() else {
            return self.on_new_request(stream, message);
        };
        match (held, message) {
            // The publisher ends a track it announced.
            (Incoming::Track { alias }, Message::PublishDone(_)) => {
                self.end_their_track(stream, alias);
            }
            // Changing a request in place is not supported. The answer goes back on the
            // stream; the request itself stands.
            (
                Incoming::Tracks { .. } | Incoming::TracksPending { .. },
                Message::RequestUpdate(update),
            ) => {
                if self.their_request_id_is_valid(update.request_id) {
                    let error = Message::RequestError(RequestError {
                        code: NOT_SUPPORTED,
                        retry_after: None,
                        reason: "REQUEST_UPDATE is not supported".to_owned(),
                        redirect: None,
                    });
                    self.send_message(stream, &error, false);
                }
            }
            (_, Message::GoAway(_)) => {}
            _ => self.close(
                CloseCode::ProtocolViolation,
                "unexpected message on a request stream",
            ),
        }
    }

    fn on_new_request(&mut self, stream: StreamId, message: Message) {
        match message {
            Message::SubscribeTracks(request) => {
                if !self.their_request_id_is_valid(request.request_id) {
                    return;
                }
                let clash = self.incoming.values().any(|held| match held {
                    Incoming::Tracks { prefix } | Incoming::TracksPending { prefix } => {
                        overlaps(prefix, &request.prefix)
                    }
                    _ => false,
                });
                if clash {
                    return self.reject(stream, PREFIX_OVERLAP, "prefix overlaps another request");
                }
                self.incoming.insert(
                    stream,
                    Incoming::TracksPending {
                        prefix: request.prefix.clone(),
                    },
                );
                self.events.push_back(Event::TracksWanted {
                    request: stream,
                    prefix: request.prefix,
                });
            }
            Message::Publish(publish) => {
                if !self.their_request_id_is_valid(publish.request_id) {
                    return;
                }
                if !self.their_aliases.insert(publish.track_alias) {
                    return self
                        .close(CloseCode::DuplicateTrackAlias, "track alias already in use");
                }
                // Only tracks we asked for are taken.
                let wanted = self.outgoing.values().any(|request| match request {
                    Outgoing::Tracks { prefix, .. } => {
                        prefix.is_prefix_of(publish.track.namespace())
                    }
                    Outgoing::Track { .. } => false,
                });
                if !wanted {
                    self.their_aliases.remove(&publish.track_alias);
                    return self.reject(stream, UNINTERESTED, "track was not asked for");
                }
                self.incoming.insert(
                    stream,
                    Incoming::Track {
                        alias: publish.track_alias,
                    },
                );
                let ok = Message::RequestOk(RequestOk {
                    params: Params::default(),
                    track_properties: Vec::new(),
                });
                self.send_message(stream, &ok, false);
                self.events.push_back(Event::TrackOffered {
                    track: publish.track,
                    alias: publish.track_alias,
                });
            }
            Message::Subscribe(request) => {
                if self.their_request_id_is_valid(request.request_id) {
                    self.reject(stream, NOT_SUPPORTED, "SUBSCRIBE is not supported yet");
                }
            }
            _ => self.close(
                CloseCode::ProtocolViolation,
                "a request stream must begin with a request",
            ),
        }
    }

    fn on_response(&mut self, stream: StreamId, message: Message) {
        let Some(request) = self.outgoing.get_mut(&stream) else {
            return; // a request we gave up; whatever was on its way is of no interest
        };
        let first = match request {
            Outgoing::Tracks { answered, .. } | Outgoing::Track { answered, .. } => {
                !core::mem::replace(answered, true)
            }
        };
        let event = match (request.clone(), message, first) {
            (Outgoing::Tracks { .. }, Message::RequestOk(_), true) => {
                Event::TracksAccepted { request: stream }
            }
            (Outgoing::Tracks { .. }, Message::RequestError(error), true) => {
                self.outgoing.remove(&stream);
                Event::TracksRefused {
                    request: stream,
                    code: error.code,
                }
            }
            (Outgoing::Track { alias, .. }, Message::RequestOk(_), true) => {
                Event::TrackAccepted { alias }
            }
            (Outgoing::Track { alias, .. }, Message::RequestError(error), true) => {
                self.outgoing.remove(&stream);
                Event::TrackRefused {
                    alias,
                    code: error.code,
                }
            }
            // After accepting a track the subscriber may try to change the subscription.
            (Outgoing::Track { .. }, Message::RequestUpdate(update), false) => {
                if self.their_request_id_is_valid(update.request_id) {
                    let error = Message::RequestError(RequestError {
                        code: NOT_SUPPORTED,
                        retry_after: None,
                        reason: "REQUEST_UPDATE is not supported".to_owned(),
                        redirect: None,
                    });
                    self.send_message(stream, &error, false);
                }
                return;
            }
            (_, Message::GoAway(_), false) => return,
            _ => {
                return self.close(
                    CloseCode::ProtocolViolation,
                    "unexpected response on a request stream",
                );
            }
        };
        self.events.push_back(event);
    }
}
