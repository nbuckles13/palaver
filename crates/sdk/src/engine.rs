//! Runs one node over a transport.
//!
//! [`palaver_node::Node`] decides everything and touches nothing: it is told what arrived
//! and when, and says what to send, whom to dial and whom to hang up on. The engine is the
//! part that touches things. It owns the node and is the only thing that calls it, from
//! one task, so the khef and the control logic are driven by one loop and nothing in them
//! needs a lock.
//!
//! Around that task are small ones that only carry bytes:
//!
//! - one per connection for each of: streams the peer opens one-way, streams it opens
//!   two-way, datagrams, and the connection's end;
//! - one per stream, reading it, writing to it, or both;
//! - one per dial.
//!
//! Each turns what it sees into a message to the one task. Messages about a connection the
//! node has since dropped are told apart by the connection's number and ignored.
//!
//! **If the same two nodes connect to each other twice** (each dialled the other at the
//! same moment), both keep the connection dialled by the node with the lower participant
//! id and close the other. Both apply the same rule, so they keep the same one.
//!
//! The engine gives a link to every connection its endpoint gives it. Who may connect is
//! the endpoint's business: wrap it in [`palaver_net::admission::Admitted`] and only
//! holders of the meeting secret get that far.

use bytes::Bytes;
use palaver_khef_core::{Action, LinkWatchConfig, Role, StreamId, StreamKind};
use palaver_moq_wire::{PalaverExtensions, Setup};
use palaver_net::identity::{Ed25519, Identity, participant_id};
use palaver_net::transport::{
    Connection, DialError, Endpoint, Peer, RecvStream, SendStream, Side, StreamError,
};
use palaver_node::{Config as NodeConfig, Node, Output, VideoTrack};
use palaver_proto::{Edge, MediaTrack, MeetingTag, ParticipantId, PlanStamp, Redacted, Timestamp};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Messages waiting for the engine's task. Those who carry bytes wait when it is full.
const INPUT_QUEUE: usize = 1024;
/// Frames from the microphone waiting for the engine. A frame that finds it full is
/// dropped: audio that is late is no use.
const AUDIO_OUT_QUEUE: usize = 16;
/// Frames to play waiting for the application; the same rule.
const AUDIO_IN_QUEUE: usize = 256;
/// Close code for the connection that loses when two nodes have connected twice.
const CLOSE_DUPLICATE: u64 = 0x100;
/// Close code when the node hangs up on a peer the plan no longer gives it.
const CLOSE_HANGUP: u64 = 0x101;
/// How long the node with the higher id waits for a planned peer to dial it before it
/// dials too. Long enough that on an open network the first dial has arrived and there
/// is nothing to do; short enough that through NATs the first dial is still trying.
/// How long a dial waits to learn where its peer is before it is given up.
const ADDRESS_PATIENCE: Duration = Duration::from_secs(10);
const ANSWER_DELAY: Duration = Duration::from_millis(200);
/// How long a node that nobody answered waits before it tries them all again. Short
/// enough that a router along the way still remembers its last try when the other end
/// starts to dial back.
const KNOCK_PAUSE: Duration = Duration::from_secs(1);

/// What the engine is told once, at start.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub meeting: MeetingTag,
    /// The name the user chose.
    pub name: String,
    /// Every media track the node can send.
    pub tracks: Vec<MediaTrack>,
    /// Where the node can be reached.
    pub addresses: Vec<SocketAddr>,
    pub link_watch: LinkWatchConfig,
}

/// A frame of someone's audio, to play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioIn {
    /// Which track: the alias the plan gives it.
    pub alias: u64,
    /// The frame counter.
    pub group: u64,
    pub level: u8,
    /// The encoded frame: a slice of the datagram as it arrived, not a copy.
    pub payload: Bytes,
}

/// What the node knows of the meeting at the moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// The plan in force, if any has been seen.
    pub plan: Option<PlanStamp>,
    /// Whether this node is the conductor.
    pub conductor: bool,
    /// Who issued the plan in force.
    pub conducts: Option<ParticipantId>,
    /// The connections the plan asks for, each a pair of nodes; and those of them
    /// sound is forwarded along, which make a tree.
    pub peerings: Vec<(ParticipantId, ParticipantId)>,
    pub audio_tree: Vec<(ParticipantId, ParticipantId)>,
    /// Everyone in the plan's roster, this node included.
    pub roster: Vec<ParticipantId>,
    /// The name each of the roster chose, in the same order. Redacted in debug output.
    pub names: Vec<Redacted<String>>,
    /// The nodes this one has a connection to.
    pub links: Vec<ParticipantId>,
    /// Whose microphone each audio alias is, by the plan.
    pub microphones: Vec<(u64, ParticipantId)>,
    /// Every video track in the plan, this node's own among them.
    pub videos: Vec<VideoTrack>,
    /// The node was told to join and nobody it was sent to has answered. It keeps
    /// trying. This is the moment to give the person who invited it a ticket with this
    /// node in it, for them to [`Engine::reach`].
    pub nobody_answers: bool,
}

/// How one connection is faring, as its transport measures it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkGauge {
    pub peer: ParticipantId,
    /// The round-trip time.
    pub rtt: Duration,
    /// Packets sent to the peer since the connection came up, and how many of them were
    /// given up as lost.
    pub sent: u64,
    pub lost: u64,
    /// Bytes sent to the peer since the connection came up, and received from it, as
    /// they are on the wire.
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// The two ends of a connection the plan names.
fn pair(edge: &Edge) -> (ParticipantId, ParticipantId) {
    (edge.0.clone(), edge.1.clone())
}

enum Command {
    Create,
    Join(Vec<Peer>),
    Addresses(Vec<SocketAddr>),
    Tracks(Vec<MediaTrack>),
    Reach(Vec<Peer>),
    Stop,
    Links(oneshot::Sender<Vec<LinkGauge>>),
    Audio {
        group: u64,
        level: u8,
        payload: Bytes,
    },
    Watch {
        alias: u64,
        watch: bool,
    },
    KeyFrame {
        alias: u64,
        lost: u64,
    },
    Video {
        alias: u64,
        group: u64,
        start: bool,
        bytes: Vec<u8>,
        fin: bool,
    },
    VideoReset {
        alias: u64,
        group: u64,
    },
}

/// Something about video, for whatever makes and shows this node's pictures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoIn {
    /// Somebody watches this node's own video track `alias` (`wanted`), or nobody does
    /// any more. Make it only while it is wanted, beginning with a key frame.
    Produce { alias: u64, wanted: bool },
    /// Begin a new group, with a key frame, on this node's own track `alias`, as soon
    /// as the encoder allows.
    KeyFrame { alias: u64 },
    /// Bytes of one group of a track this node watches, in order. The first (`start`)
    /// begin with the stream's header; `fin` when the group is whole.
    Group {
        alias: u64,
        group: u64,
        start: bool,
        bytes: Vec<u8>,
        fin: bool,
    },
    /// A group this node was being given was given up part way.
    Reset { alias: u64, group: u64 },
}

/// The engine's door for video.
///
/// What to watch and what this node's camera makes go in, and what there is to show
/// comes out. Taken once ([`Engine::video`]) and kept by whatever runs the pictures,
/// apart from the sound.
///
/// Nothing here waits or is dropped: a group with a piece missing is no use, so pieces
/// queue without limit both ways. (What happens when a link or a reader cannot keep up
/// is to come; see the design, section 7.)
pub struct VideoPort {
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::UnboundedReceiver<VideoIn>,
}

impl VideoPort {
    /// Begin to watch the video track with `alias`, or stop. Watching lasts through
    /// changes of plan.
    pub fn watch(&self, alias: u64, watch: bool) {
        let _ = self.commands.send(Command::Watch { alias, watch });
    }

    /// The decoder lost its place in group `lost` of a track being watched, and needs a
    /// group after it.
    pub fn ask_key_frame(&self, alias: u64, lost: u64) {
        let _ = self.commands.send(Command::KeyFrame { alias, lost });
    }

    /// The next bytes of a group of this node's own track: the first (`start`) begin
    /// with the stream's header, and `fin` ends the group.
    pub fn send(&self, alias: u64, group: u64, start: bool, bytes: Vec<u8>, fin: bool) {
        let _ = self.commands.send(Command::Video {
            alias,
            group,
            start,
            bytes,
            fin,
        });
    }

    /// Give up a group of this node's own part way.
    pub fn give_up(&self, alias: u64, group: u64) {
        let _ = self.commands.send(Command::VideoReset { alias, group });
    }

    /// The next thing about video. `None` once the engine has stopped.
    pub async fn next(&mut self) -> Option<VideoIn> {
        self.events.recv().await
    }

    /// The same, from a thread that is not the runtime's.
    pub fn blocking_next(&mut self) -> Option<VideoIn> {
        self.events.blocking_recv()
    }

    /// The next thing if one is waiting; never waits.
    pub fn try_next(&mut self) -> Option<VideoIn> {
        self.events.try_recv().ok()
    }
}

/// A running node.
pub struct Engine {
    me: ParticipantId,
    commands: mpsc::Sender<Command>,
    microphone: mpsc::Sender<Command>,
    status: watch::Receiver<Status>,
    audio: mpsc::Receiver<AudioIn>,
    video: Option<VideoPort>,
    task: JoinHandle<()>,
}

impl Engine {
    /// Start a node on `endpoint`, which must have been made with `identity`'s key. Must
    /// be called inside a tokio runtime. The node does nothing until it is told to
    /// [`create_meeting`](Self::create_meeting) or [`join`](Self::join).
    pub fn start<E: Endpoint>(endpoint: E, identity: Identity, config: EngineConfig) -> Self {
        let me = identity.participant_id();
        let (commands, commands_in) = mpsc::channel(16);
        let (microphone, microphone_in) = mpsc::channel(AUDIO_OUT_QUEUE);
        let (audio_out, audio) = mpsc::channel(AUDIO_IN_QUEUE);
        let (status_out, status) = watch::channel(Status::default());
        let (inputs, inputs_in) = mpsc::channel(INPUT_QUEUE);
        let (camera, camera_in) = mpsc::unbounded_channel();
        let (video_out, video_in) = mpsc::unbounded_channel();
        let endpoint = Arc::new(endpoint);

        // The first report's sequence number must be higher than any this key used before.
        let first_sequence = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(1, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let node = Node::new(
            NodeConfig {
                meeting: config.meeting,
                name: Redacted::new(config.name),
                first_sequence,
                setup: Setup {
                    implementation: Some(concat!("palaver/", env!("CARGO_PKG_VERSION")).into()),
                    palaver: Some(PalaverExtensions { version: 1 }),
                    ..Setup::default()
                },
                link_watch: config.link_watch,
                tracks: config.tracks,
                addresses: config.addresses.into_iter().map(Redacted::new).collect(),
            },
            identity,
            Ed25519,
        );

        let accepting = {
            let (endpoint, inputs) = (Arc::clone(&endpoint), inputs.clone());
            tokio::spawn(async move {
                while let Some(connection) = endpoint.accept().await {
                    if inputs.send(Input::Connected(connection)).await.is_err() {
                        return;
                    }
                }
            })
        };
        let run = Run {
            node,
            endpoint,
            links: BTreeMap::new(),
            entry_points: BTreeMap::new(),
            join_through: Vec::new(),
            nobody_answers: false,
            knock_pending: false,
            awaiting: BTreeMap::new(),
            dials: BTreeMap::new(),
            answering: BTreeSet::new(),
            answered_for: None,
            next_number: 0,
            inputs,
            started: Instant::now(),
            audio_out,
            video_out,
            status: status_out,
            scratch: Vec::new(),
        };
        let task = tokio::spawn(async move {
            run.run(commands_in, microphone_in, camera_in, inputs_in)
                .await;
            accepting.abort();
        });
        Self {
            me,
            commands,
            microphone,
            status,
            audio,
            video: Some(VideoPort {
                commands: camera,
                events: video_in,
            }),
            task,
        }
    }

    /// The door for video. There is one: `None` once it has been taken.
    pub const fn video(&mut self) -> Option<VideoPort> {
        self.video.take()
    }

    #[must_use]
    pub const fn me(&self) -> &ParticipantId {
        &self.me
    }

    /// The handle for telling the node what to do and watching what it reports. It can
    /// be cloned and kept anywhere, while the engine itself goes to whatever carries the
    /// sound.
    #[must_use]
    pub fn controller(&self) -> Controller {
        Controller {
            me: self.me.clone(),
            commands: self.commands.clone(),
            status: self.status.clone(),
        }
    }

    /// Create a meeting: this node is its conductor.
    pub async fn create_meeting(&self) {
        let _ = self.commands.send(Command::Create).await;
    }

    /// Join a meeting through one of `entry_points`, tried in order.
    pub async fn join(&self, entry_points: Vec<Peer>) {
        let _ = self.commands.send(Command::Join(entry_points)).await;
    }

    /// Dial nodes that are trying to join and cannot get through on their own: those in a
    /// ticket one of them sent back. Through routers, a connection needs both ends to
    /// send; the joiner has been sending since it began, and this is the other half.
    pub async fn reach(&self, peers: Vec<Peer>) {
        let _ = self.commands.send(Command::Reach(peers)).await;
    }

    /// Where this node can be reached has changed: it has learned its public address,
    /// say. The meeting is told in its next report.
    pub async fn set_addresses(&self, addresses: Vec<SocketAddr>) {
        let _ = self.commands.send(Command::Addresses(addresses)).await;
    }

    /// The tracks this node can send have changed: all of them, as they now are. The
    /// meeting is told in its next report, and the plan that follows lists them.
    pub async fn set_tracks(&self, tracks: Vec<MediaTrack>) {
        let _ = self.commands.send(Command::Tracks(tracks)).await;
    }

    /// Send one frame of this node's microphone. `false` if it was dropped because the
    /// engine is behind (or stopped).
    pub fn send_audio(&self, group: u64, level: u8, payload: Bytes) -> bool {
        self.microphone
            .try_send(Command::Audio {
                group,
                level,
                payload,
            })
            .is_ok()
    }

    /// What the node knows of the meeting; changes can be awaited.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// The next frame of someone's audio to play. `None` once the engine has stopped.
    pub async fn next_audio(&mut self) -> Option<AudioIn> {
        self.audio.recv().await
    }

    /// The next frame to play if one is waiting; never waits.
    pub fn try_next_audio(&mut self) -> Option<AudioIn> {
        self.audio.try_recv().ok()
    }

    /// Stop the node: close its endpoint and every connection.
    pub async fn stop(self) {
        let _ = self.commands.send(Command::Stop).await;
        drop(self.microphone);
        let _ = self.task.await;
    }
}

/// Tells a running node what to do, and watches what it reports.
///
/// Any number may exist beside the [`Engine`]. The node runs until it is told to
/// [`stop`](Controller::stop), or until the engine and every controller are gone.
#[derive(Debug, Clone)]
pub struct Controller {
    me: ParticipantId,
    commands: mpsc::Sender<Command>,
    status: watch::Receiver<Status>,
}

impl Controller {
    #[must_use]
    pub const fn me(&self) -> &ParticipantId {
        &self.me
    }

    /// Create a meeting: this node is its conductor.
    pub async fn create_meeting(&self) {
        let _ = self.commands.send(Command::Create).await;
    }

    /// Join a meeting through one of `entry_points`, tried in order.
    pub async fn join(&self, entry_points: Vec<Peer>) {
        let _ = self.commands.send(Command::Join(entry_points)).await;
    }

    /// Dial nodes that are trying to join and cannot get through on their own: those in a
    /// ticket one of them sent back. Through routers, a connection needs both ends to
    /// send; the joiner has been sending since it began, and this is the other half.
    pub async fn reach(&self, peers: Vec<Peer>) {
        let _ = self.commands.send(Command::Reach(peers)).await;
    }

    /// Where this node can be reached has changed: it has learned its public address,
    /// say. The meeting is told in its next report.
    pub async fn set_addresses(&self, addresses: Vec<SocketAddr>) {
        let _ = self.commands.send(Command::Addresses(addresses)).await;
    }

    /// The tracks this node can send have changed: all of them, as they now are. The
    /// meeting is told in its next report, and the plan that follows lists them.
    pub async fn set_tracks(&self, tracks: Vec<MediaTrack>) {
        let _ = self.commands.send(Command::Tracks(tracks)).await;
    }

    /// What the node reports: watch it for changes.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// How each connection is faring at the moment. These change all the time, so they
    /// are asked for and not watched. Empty once the node has stopped.
    pub async fn links(&self) -> Vec<LinkGauge> {
        let (answer, answered) = oneshot::channel();
        if self.commands.send(Command::Links(answer)).await.is_err() {
            return Vec::new();
        }
        answered.await.unwrap_or_default()
    }

    /// Stop the node: close its endpoint and every connection.
    pub async fn stop(&self) {
        let _ = self.commands.send(Command::Stop).await;
    }
}

/// What the carrying tasks tell the engine's task.
enum Input<C: Connection> {
    /// A connection is up, dialled by either end.
    Connected(C),
    /// One attempt to connect to `peer` is over, with a connection or without.
    DialEnded {
        peer: ParticipantId,
    },
    /// It is time to try again everyone this node was told to join through.
    Knock,
    /// It is time to dial `peer`, which should have dialled this node, if it has not.
    Answer {
        peer: ParticipantId,
    },
    /// The peer opened a two-way stream; this is how to write on it.
    Opened {
        peer: ParticipantId,
        number: u64,
        stream: u64,
        writer: mpsc::UnboundedSender<Write>,
    },
    Data {
        peer: ParticipantId,
        number: u64,
        stream: StreamId,
        kind: StreamKind,
        bytes: Bytes,
        fin: bool,
    },
    Reset {
        peer: ParticipantId,
        number: u64,
        stream: StreamId,
    },
    Datagram {
        peer: ParticipantId,
        number: u64,
        bytes: Bytes,
    },
    Closed {
        peer: ParticipantId,
        number: u64,
    },
}

/// What the engine's task tells a stream's task.
enum Write {
    Data { bytes: Bytes, fin: bool },
    Reset,
}

struct Link<C> {
    connection: C,
    /// Tells this connection from an earlier one to the same peer.
    number: u64,
    /// Streams this end opened, by the session's number for them.
    ours: BTreeMap<u64, mpsc::UnboundedSender<Write>>,
    /// Two-way streams the peer opened, by the number the engine gave them.
    theirs: BTreeMap<u64, mpsc::UnboundedSender<Write>>,
}

/// The attempts under way to connect to one peer.
#[derive(Default)]
struct Dialling {
    /// The addresses tried so far.
    tried: Vec<Redacted<SocketAddr>>,
    /// Attempts that have not ended.
    under_way: u32,
    /// The node asked for the dial, so it is told if every attempt fails.
    asked: bool,
}

struct Run<E: Endpoint> {
    node: Node<Identity, Ed25519>,
    endpoint: Arc<E>,
    links: BTreeMap<ParticipantId, Link<E::Connection>>,
    /// Where to find the nodes the meeting was joined through.
    entry_points: BTreeMap<ParticipantId, Peer>,
    /// The nodes this one was told to join through, in order.
    join_through: Vec<ParticipantId>,
    /// Nobody it was told to join through has answered yet.
    nobody_answers: bool,
    /// A [`Input::Knock`] is on its way.
    knock_pending: bool,
    /// Nodes to dial once it is known where they are, and since when each has waited.
    awaiting: BTreeMap<ParticipantId, Timestamp>,
    /// Dials under way.
    dials: BTreeMap<ParticipantId, Dialling>,
    /// Planned peers that should dial this node and that it has dialled as well, or is
    /// about to. One is dropped when its link comes up or the plan drops it, so that a
    /// link lost later is answered again.
    answering: BTreeSet<ParticipantId>,
    /// The plan and the links `answering` was last worked out for.
    answered_for: Option<(Option<PlanStamp>, usize, u64)>,
    next_number: u64,
    inputs: mpsc::Sender<Input<E::Connection>>,
    started: Instant,
    audio_out: mpsc::Sender<AudioIn>,
    video_out: mpsc::UnboundedSender<VideoIn>,
    status: watch::Sender<Status>,
    scratch: Vec<u8>,
}

impl<E: Endpoint> Run<E> {
    fn now(&self) -> Timestamp {
        Timestamp::from_micros(
            u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX),
        )
    }

    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut microphone: mpsc::Receiver<Command>,
        mut camera: mpsc::UnboundedReceiver<Command>,
        mut inputs: mpsc::Receiver<Input<E::Connection>>,
    ) {
        loop {
            let now = self.now();
            let wake = self.node.next_wake(now).map(|at| {
                // Never for the present moment: that would spin without the clock moving.
                let after = at.saturating_since(now).max(Duration::from_millis(1));
                Instant::now() + after
            });
            tokio::select! {
                command = commands.recv() => match command {
                    // Told to stop, or every handle is gone.
                    Some(Command::Stop) | None => break,
                    Some(command) => self.command(command),
                },
                Some(frame) = microphone.recv() => self.command(frame),
                Some(video) = camera.recv() => self.command(video),
                Some(input) = inputs.recv() => self.input(input),
                () = sleep_until(wake) => {
                    let now = self.now();
                    self.node.poll(now);
                }
            }
            self.carry_out();
        }
        self.endpoint.close();
    }

    fn command(&mut self, command: Command) {
        let now = self.now();
        match command {
            Command::Create => self.node.create_meeting(now),
            // Taken before it gets here: it ends the loop.
            Command::Stop => {}
            Command::Links(answer) => {
                let gauges = self
                    .links
                    .iter()
                    .map(|(peer, link)| {
                        let path = link.connection.path();
                        LinkGauge {
                            peer: peer.clone(),
                            rtt: path.rtt,
                            sent: path.sent,
                            lost: path.lost,
                            bytes_sent: path.bytes_sent,
                            bytes_received: path.bytes_received,
                        }
                    })
                    .collect();
                // Whoever asked may have gone.
                let _ = answer.send(gauges);
            }
            Command::Tracks(tracks) => self.node.set_tracks(tracks, now),
            Command::Addresses(addresses) => self
                .node
                .set_addresses(addresses.into_iter().map(Redacted::new).collect(), now),
            Command::Join(peers) => {
                let mut ids = Vec::new();
                for peer in peers {
                    let id = participant_id(&peer.key);
                    ids.push(id.clone());
                    self.entry_points.insert(id, peer);
                }
                self.join_through.clone_from(&ids);
                self.nobody_answers = false;
                self.node.join(ids, now);
            }
            Command::Reach(peers) => {
                for target in peers {
                    let peer = participant_id(&target.key);
                    if peer != *self.node.me() && !self.links.contains_key(&peer) {
                        // Remembered, so that a later dial the plan asks for knows where.
                        self.entry_points.insert(peer.clone(), target.clone());
                        self.attempt(peer, target, false);
                    }
                }
            }
            Command::Audio {
                group,
                level,
                payload,
            } => {
                self.scratch.clear();
                let Some(out) =
                    self.node
                        .local_audio(group, level, &payload, &mut self.scratch, now)
                else {
                    return;
                };
                let datagram = Bytes::copy_from_slice(&self.scratch);
                for peer in &out.forward_to {
                    self.send_datagram(peer, datagram.clone());
                }
            }
            Command::Watch { alias, watch: true } => self.node.watch(alias, now),
            Command::Watch {
                alias,
                watch: false,
            } => self.node.unwatch(alias, now),
            Command::KeyFrame { alias, lost } => self.node.ask_key_frame(alias, lost, now),
            Command::Video {
                alias,
                group,
                start,
                bytes,
                fin,
            } => self.node.local_video(alias, group, start, &bytes, fin, now),
            Command::VideoReset { alias, group } => self.node.local_video_reset(alias, group, now),
        }
    }

    fn input(&mut self, input: Input<E::Connection>) {
        let now = self.now();
        match input {
            Input::Connected(connection) => self.connected(connection),
            Input::DialEnded { peer } => self.dial_ended(&peer),
            Input::Answer { peer } => self.answer(peer),
            Input::Knock => {
                self.knock_pending = false;
                if self.outside() {
                    for peer in self.join_through.clone() {
                        if let Some(target) = self.whereabouts(&peer) {
                            self.attempt(peer, target, false);
                        }
                    }
                }
            }
            Input::Opened {
                peer,
                number,
                stream,
                writer,
            } => {
                if let Some(link) = self.link(&peer, number) {
                    link.theirs.insert(stream, writer);
                }
            }
            Input::Data {
                peer,
                number,
                stream,
                kind,
                bytes,
                fin,
            } => {
                if self.link(&peer, number).is_some() {
                    self.node.on_data(&peer, stream, kind, &bytes, fin, now);
                }
            }
            Input::Reset {
                peer,
                number,
                stream,
            } => {
                if self.link(&peer, number).is_some() {
                    self.node.on_reset(&peer, stream, now);
                }
            }
            Input::Datagram {
                peer,
                number,
                bytes,
            } => {
                if self.link(&peer, number).is_none() {
                    return;
                }
                let Some(out) = self.node.on_datagram(&peer, &bytes, now) else {
                    return;
                };
                // The same bytes go on to each neighbour: nothing is copied or rewritten.
                for next in &out.forward_to {
                    self.send_datagram(next, bytes.clone());
                }
                if out.for_client && out.payload.end <= bytes.len() {
                    // A full queue drops the frame: the application is not playing.
                    let _ = self.audio_out.try_send(AudioIn {
                        alias: out.alias,
                        group: out.group,
                        level: out.level,
                        payload: bytes.slice(out.payload),
                    });
                }
            }
            Input::Closed { peer, number } => {
                if self.link(&peer, number).is_some() {
                    self.links.remove(&peer);
                    self.node.link_down(&peer, now);
                }
            }
        }
    }

    /// The link to `peer`, if it is still the connection with this number.
    fn link(&mut self, peer: &ParticipantId, number: u64) -> Option<&mut Link<E::Connection>> {
        self.links
            .get_mut(peer)
            .filter(|link| link.number == number)
    }

    fn send_datagram(&self, peer: &ParticipantId, datagram: Bytes) {
        if let Some(link) = self.links.get(peer) {
            // Dropped if the path cannot take it; that is what a datagram is.
            let _ = link.connection.send_datagram(datagram);
        }
    }

    /// A connection is up. Give it to the node, unless there is already one to that peer
    /// and that one is the one to keep.
    fn connected(&mut self, connection: E::Connection) {
        let now = self.now();
        let peer = participant_id(&connection.peer());
        let me = self.node.me().clone();
        if peer == me {
            connection.close(CLOSE_DUPLICATE, "connected to itself");
            return;
        }
        let dialler = |side: Side| match side {
            Side::Dialled => me.clone(),
            Side::Accepted => peer.clone(),
        };
        if let Some(existing) = self.links.get(&peer) {
            // Keep the one dialled by the lower id. Both ends work this out alike.
            let (held, new) = (
                dialler(existing.connection.side()),
                dialler(connection.side()),
            );
            if new >= held {
                connection.close(CLOSE_DUPLICATE, "already connected");
                // If this was a dial of ours, it is answered all the same.
                self.node.link_up(
                    peer,
                    role_of(connection.side()),
                    connection.path().rtt,
                    connection.path().remote,
                    now,
                );
                return;
            }
            if let Some(old) = self.links.remove(&peer) {
                old.connection.close(CLOSE_DUPLICATE, "already connected");
            }
            self.node.link_down(&peer, now);
        }

        let number = self.next_number;
        self.next_number = self.next_number.saturating_add(1);
        carry(&connection, peer.clone(), number, &self.inputs);
        let path = connection.path();
        let role = role_of(connection.side());
        self.links.insert(
            peer.clone(),
            Link {
                connection,
                number,
                ours: BTreeMap::new(),
                theirs: BTreeMap::new(),
            },
        );
        self.node.link_up(peer, role, path.rtt, path.remote, now);
    }

    /// Do everything the node has asked for, then say what it now knows.
    fn carry_out(&mut self) {
        self.dial_the_awaited();
        self.try_new_addresses();
        while let Some(output) = self.node.poll_output() {
            match output {
                Output::Dial { peer } => self.dial(peer),
                Output::Hangup { peer } => {
                    if let Some(link) = self.links.remove(&peer) {
                        link.connection.close(CLOSE_HANGUP, "not in the plan");
                    }
                }
                Output::Datagram { peer, bytes } => self.send_datagram(&peer, Bytes::from(bytes)),
                Output::Stream { peer, action } => self.stream(peer, action),
                // Whoever runs the pictures may have gone; then there is nobody to tell.
                Output::Produce { alias, wanted } => {
                    let _ = self.video_out.send(VideoIn::Produce { alias, wanted });
                }
                Output::KeyFrame { alias } => {
                    let _ = self.video_out.send(VideoIn::KeyFrame { alias });
                }
                Output::Video {
                    alias,
                    group,
                    start,
                    bytes,
                    fin,
                } => {
                    let _ = self.video_out.send(VideoIn::Group {
                        alias,
                        group,
                        start,
                        bytes,
                        fin,
                    });
                }
                Output::VideoReset { alias, group } => {
                    let _ = self.video_out.send(VideoIn::Reset { alias, group });
                }
            }
        }
        self.answer_dials();
        self.knock();
        let status = Status {
            nobody_answers: self.nobody_answers,
            plan: self.node.plan().map(|plan| plan.stamp),
            conductor: self.node.is_conductor(),
            conducts: self.node.plan().map(|plan| plan.conductor.clone()),
            peerings: self
                .node
                .plan()
                .map_or_else(Vec::new, |plan| plan.peerings.iter().map(pair).collect()),
            audio_tree: self.node.plan().map_or_else(Vec::new, |plan| {
                plan.audio_overlay.iter().map(pair).collect()
            }),
            roster: self.node.plan().map_or_else(Vec::new, |plan| {
                plan.roster
                    .iter()
                    .map(|member| participant_id(&member.key))
                    .collect()
            }),
            names: self.node.plan().map_or_else(Vec::new, |plan| {
                plan.roster
                    .iter()
                    .map(|member| member.name.clone())
                    .collect()
            }),
            links: self.links.keys().cloned().collect(),
            microphones: self.node.plan().map_or_else(Vec::new, |plan| {
                let mut microphones: Vec<(u64, ParticipantId)> = plan
                    .roster
                    .iter()
                    .flat_map(|member| {
                        let id = participant_id(&member.key);
                        member
                            .tracks
                            .iter()
                            .filter(|assigned| assigned.track.audio)
                            .map(move |assigned| (assigned.alias, id.clone()))
                    })
                    .collect();
                microphones.sort();
                microphones
            }),
            videos: self.node.video_tracks().to_vec(),
        };
        self.status.send_if_modified(|held| {
            if *held == status {
                return false;
            }
            *held = status;
            true
        });
    }

    /// Where to find `peer`: its key, and every address known for it. An entry point is
    /// known from the ticket; any other node from its report, which names its key and
    /// lists its addresses, and from the reports of its neighbours, which say where they
    /// see it. `None` until enough is known to dial.
    fn whereabouts(&self, peer: &ParticipantId) -> Option<Peer> {
        let entry = self.entry_points.get(peer);
        let report = self.node.report_of(peer);
        let mut target = match (entry, report) {
            (Some(entry), _) => entry.clone(),
            (None, Some(report)) => Peer::by_key(report.key),
            (None, None) => return None,
        };
        if let Some(report) = report {
            // The report is the newer word, so its addresses are tried first in a tie.
            let mut addresses = report.addresses.clone();
            for address in target.addresses {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
            target.addresses = addresses;
        }
        // Where others see it: behind a NAT, the only address that leads to it.
        for address in self.node.observed_addresses_of(peer) {
            let address = Redacted::new(address);
            if !target.addresses.contains(&address) {
                target.addresses.push(address);
            }
        }
        if target.addresses.is_empty() && self.endpoint.dials_by_address() {
            return None;
        }
        Some(target)
    }

    fn dial(&mut self, peer: ParticipantId) {
        if let Some(target) = self.whereabouts(&peer) {
            self.attempt(peer, target, true);
        } else {
            // The plan names it, so word of where it is is on its way here.
            let now = self.now();
            self.awaiting.entry(peer).or_insert(now);
        }
    }

    /// Try to connect to `peer` at the addresses in `target` not yet tried. `asked` if
    /// the node asked for the dial: it is then told if every attempt fails.
    fn attempt(&mut self, peer: ParticipantId, mut target: Peer, asked: bool) {
        let dialling = self.dials.entry(peer.clone()).or_default();
        dialling.asked |= asked;
        target
            .addresses
            .retain(|address| !dialling.tried.contains(address));
        if dialling.under_way > 0 && target.addresses.is_empty() {
            // Everything known is being tried already.
            return;
        }
        dialling.tried.extend(target.addresses.iter().cloned());
        dialling.under_way = dialling.under_way.saturating_add(1);
        let (endpoint, inputs) = (Arc::clone(&self.endpoint), self.inputs.clone());
        tokio::spawn(async move {
            match endpoint.dial(&target).await {
                Ok(connection) => {
                    let _ = inputs.send(Input::Connected(connection)).await;
                }
                Err(DialError::Unreachable | DialError::Refused | DialError::EndpointClosed) => {}
            }
            let _ = inputs.send(Input::DialEnded { peer }).await;
        });
    }

    /// An attempt to reach `peer` is over. If it was the last and there is no link, the
    /// dial has failed.
    fn dial_ended(&mut self, peer: &ParticipantId) {
        let Some(dialling) = self.dials.get_mut(peer) else {
            return;
        };
        dialling.under_way = dialling.under_way.saturating_sub(1);
        if dialling.under_way > 0 {
            return;
        }
        let asked = dialling.asked;
        self.dials.remove(peer);
        if asked && !self.links.contains_key(peer) {
            let now = self.now();
            self.node.dial_failed(peer, now);
        }
    }

    /// A peer being dialled may have become known at more addresses since: a neighbour's
    /// report, with the address it sees the peer at, can come after the dial began.
    fn try_new_addresses(&mut self) {
        if self.dials.is_empty() {
            return;
        }
        let peers: Vec<ParticipantId> = self.dials.keys().cloned().collect();
        for peer in peers {
            if let Some(target) = self.whereabouts(&peer) {
                self.attempt(peer, target, false);
            }
        }
    }

    /// Told to join, and still with no link and no plan.
    fn outside(&self) -> bool {
        !self.join_through.is_empty() && self.links.is_empty() && self.node.plan().is_none()
    }

    /// A node that everyone it was sent to has failed to answer tries them all again,
    /// after a pause, for as long as it is outside. Each try also keeps open the way
    /// back through its own router, for a member who dials it from a ticket it sent.
    fn knock(&mut self) {
        if !self.outside() {
            self.nobody_answers = false;
            return;
        }
        // The node tries each in turn first; while any try is under way there is
        // nothing to add.
        if !self.dials.is_empty() || !self.awaiting.is_empty() || self.knock_pending {
            return;
        }
        self.nobody_answers = true;
        self.knock_pending = true;
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            tokio::time::sleep(KNOCK_PAUSE).await;
            let _ = inputs.send(Input::Knock).await;
        });
    }

    /// Planned peers with a lower id dial this node. Through NATs their packets arrive
    /// only once this node has sent some the other way, so after a moment it dials any
    /// that have not got through. Two connections that result are settled as always.
    fn answer_dials(&mut self) {
        // This runs after every message, most of them audio: look only on a change.
        let stamp = self.node.plan().map(|plan| plan.stamp);
        let state = Some((stamp, self.links.len(), self.next_number));
        if self.answered_for == state {
            return;
        }
        self.answered_for = state;
        let planned = self.node.planned_peers_now();
        let links = &self.links;
        self.answering
            .retain(|peer| planned.contains(peer) && !links.contains_key(peer));
        for peer in planned {
            if peer < *self.node.me()
                && !self.links.contains_key(&peer)
                && self.answering.insert(peer.clone())
            {
                let inputs = self.inputs.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(ANSWER_DELAY).await;
                    let _ = inputs.send(Input::Answer { peer }).await;
                });
            }
        }
    }

    fn answer(&mut self, peer: ParticipantId) {
        if self.links.contains_key(&peer) || !self.answering.contains(&peer) {
            return;
        }
        let Some(target) = self.whereabouts(&peer) else {
            // Its report has not come; ask again in a moment.
            self.answering.remove(&peer);
            self.answered_for = None;
            return;
        };
        // A failure is the dialler's to report, not this node's.
        self.attempt(peer, target, false);
    }

    /// Dial those it is now known where to find, and give up on those the plan has
    /// dropped, a connection has reached first, or nothing has been heard of for too long.
    fn dial_the_awaited(&mut self) {
        if self.awaiting.is_empty() {
            return;
        }
        let now = self.now();
        for (peer, since) in std::mem::take(&mut self.awaiting) {
            let planned = self
                .node
                .plan()
                .is_some_and(|plan| plan.member(&peer, &Ed25519).is_some());
            if self.links.contains_key(&peer) {
                // It dialled us meanwhile; the node already knows.
            } else if !planned || now.saturating_since(since) >= ADDRESS_PATIENCE {
                self.node.dial_failed(&peer, now);
            } else if self.whereabouts(&peer).is_some() {
                self.dial(peer);
            } else {
                self.awaiting.insert(peer, since);
            }
        }
    }

    fn stream(&mut self, peer: ParticipantId, action: Action) {
        let Some(link) = self.links.get_mut(&peer) else {
            return;
        };
        match action {
            Action::Open {
                stream: StreamId::Ours(n),
                kind,
            } => {
                let (writer, queue) = mpsc::unbounded_channel();
                link.ours.insert(n, writer);
                let connection = link.connection.clone();
                let tag = Tag {
                    peer,
                    number: link.number,
                    stream: StreamId::Ours(n),
                    kind,
                    inputs: self.inputs.clone(),
                };
                tokio::spawn(async move {
                    match kind {
                        StreamKind::Uni => {
                            if let Ok(send) = connection.open_uni().await {
                                let none: Option<<E::Connection as Connection>::Recv> = None;
                                run_stream(Some(send), none, Some(queue), tag).await;
                            }
                        }
                        StreamKind::Bidi => {
                            if let Ok((send, recv)) = connection.open_bidi().await {
                                run_stream(Some(send), Some(recv), Some(queue), tag).await;
                            }
                        }
                    }
                });
            }
            // The peer's streams are opened by the peer.
            Action::Open { .. } => {}
            Action::Send { stream, bytes, fin } => {
                let (writers, n) = match stream {
                    StreamId::Ours(n) => (&mut link.ours, n),
                    StreamId::Theirs(n) => (&mut link.theirs, n),
                };
                if let Some(writer) = writers.get(&n) {
                    let _ = writer.send(Write::Data {
                        bytes: Bytes::from(bytes),
                        fin,
                    });
                }
                // A finished stream needs no more remembering.
                if fin {
                    writers.remove(&n);
                }
            }
            Action::Reset { stream } => {
                let writer = match stream {
                    StreamId::Ours(n) => link.ours.remove(&n),
                    StreamId::Theirs(n) => link.theirs.remove(&n),
                };
                if let Some(writer) = writer {
                    let _ = writer.send(Write::Reset);
                }
            }
            // The session met something the protocol forbids. The connection is done.
            Action::Close { code, reason } => {
                let now = self.now();
                if let Some(link) = self.links.remove(&peer) {
                    link.connection.close(code as u64, reason);
                }
                self.node.link_down(&peer, now);
            }
        }
    }
}

const fn role_of(side: Side) -> Role {
    match side {
        Side::Dialled => Role::Client,
        Side::Accepted => Role::Server,
    }
}

async fn sleep_until(wake: Option<Instant>) {
    match wake {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Start the tasks that carry what arrives on `connection` to the engine's task.
fn carry<C: Connection>(
    connection: &C,
    peer: ParticipantId,
    number: u64,
    inputs: &mpsc::Sender<Input<C>>,
) {
    // Streams the peer opens are numbered here: one-way ones even, two-way ones odd.
    {
        let (connection, peer, inputs) = (connection.clone(), peer.clone(), inputs.clone());
        tokio::spawn(async move {
            let mut count = 0u64;
            while let Ok(recv) = connection.accept_uni().await {
                let tag = Tag {
                    peer: peer.clone(),
                    number,
                    stream: StreamId::Theirs(count.saturating_mul(2)),
                    kind: StreamKind::Uni,
                    inputs: inputs.clone(),
                };
                count = count.saturating_add(1);
                let none: Option<C::Send> = None;
                tokio::spawn(run_stream(none, Some(recv), None, tag));
            }
        });
    }
    {
        let (connection, peer, inputs) = (connection.clone(), peer.clone(), inputs.clone());
        tokio::spawn(async move {
            let mut count = 0u64;
            while let Ok((send, recv)) = connection.accept_bidi().await {
                let stream = count.saturating_mul(2).saturating_add(1);
                count = count.saturating_add(1);
                let (writer, queue) = mpsc::unbounded_channel();
                // The engine must know how to answer before it sees the question.
                let opened = Input::Opened {
                    peer: peer.clone(),
                    number,
                    stream,
                    writer,
                };
                if inputs.send(opened).await.is_err() {
                    return;
                }
                let tag = Tag {
                    peer: peer.clone(),
                    number,
                    stream: StreamId::Theirs(stream),
                    kind: StreamKind::Bidi,
                    inputs: inputs.clone(),
                };
                tokio::spawn(run_stream(Some(send), Some(recv), Some(queue), tag));
            }
        });
    }
    {
        let (connection, peer, inputs) = (connection.clone(), peer.clone(), inputs.clone());
        tokio::spawn(async move {
            while let Ok(bytes) = connection.read_datagram().await {
                let datagram = Input::Datagram {
                    peer: peer.clone(),
                    number,
                    bytes,
                };
                if inputs.send(datagram).await.is_err() {
                    return;
                }
            }
        });
    }
    let (connection, inputs) = (connection.clone(), inputs.clone());
    tokio::spawn(async move {
        connection.closed().await;
        let _ = inputs.send(Input::Closed { peer, number }).await;
    });
}

/// Which stream a stream's task is carrying, and where to report.
struct Tag<C: Connection> {
    peer: ParticipantId,
    number: u64,
    stream: StreamId,
    kind: StreamKind,
    inputs: mpsc::Sender<Input<C>>,
}

/// Carry one stream: write what the engine queues for it, and report what arrives on it.
/// Either half may be absent.
async fn run_stream<S: SendStream, R: RecvStream, C: Connection>(
    mut send: Option<S>,
    mut recv: Option<R>,
    mut queue: Option<mpsc::UnboundedReceiver<Write>>,
    tag: Tag<C>,
) {
    loop {
        if send.is_none() {
            queue = None;
        }
        if queue.is_none() && recv.is_none() {
            return;
        }
        tokio::select! {
            write = next_write(&mut queue), if queue.is_some() => match write {
                Some(Write::Data { bytes, fin }) => {
                    let Some(stream) = send.as_mut() else { continue };
                    if !bytes.is_empty() && stream.write(bytes).await.is_err() {
                        // The peer stopped the stream, or the connection is gone.
                        send = None;
                        continue;
                    }
                    if fin {
                        stream.finish();
                        send = None;
                    }
                }
                Some(Write::Reset) => {
                    if let Some(mut stream) = send.take() {
                        stream.reset(0);
                    }
                    if let Some(mut stream) = recv.take() {
                        stream.stop(0);
                    }
                    return;
                }
                // The engine has forgotten the stream.
                None => queue = None,
            },
            read = next_read(&mut recv), if recv.is_some() => {
                let input = match read {
                    Ok(Some(bytes)) => Input::Data {
                        peer: tag.peer.clone(),
                        number: tag.number,
                        stream: tag.stream,
                        kind: tag.kind,
                        bytes,
                        fin: false,
                    },
                    Ok(None) => {
                        recv = None;
                        Input::Data {
                            peer: tag.peer.clone(),
                            number: tag.number,
                            stream: tag.stream,
                            kind: tag.kind,
                            bytes: Bytes::new(),
                            fin: true,
                        }
                    }
                    Err(StreamError::Reset(_)) => {
                        recv = None;
                        Input::Reset {
                            peer: tag.peer.clone(),
                            number: tag.number,
                            stream: tag.stream,
                        }
                    }
                    // The connection is gone; its own task says so.
                    Err(_) => return,
                };
                if tag.inputs.send(input).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn next_write(queue: &mut Option<mpsc::UnboundedReceiver<Write>>) -> Option<Write> {
    match queue {
        Some(queue) => queue.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_read<R: RecvStream>(recv: &mut Option<R>) -> Result<Option<Bytes>, StreamError> {
    match recv {
        Some(recv) => recv.read().await,
        None => std::future::pending().await,
    }
}
