//! The meeting behind the window.
//!
//! The window draws and takes clicks; it must never wait. So the node, its network and
//! its sound live on a thread of their own, which takes [`Order`]s and keeps a [`View`]
//! up to date for the window to draw.
//!
//! What a session stands on is behind two traits: a [`World`] makes the node and starts
//! its sound, and a [`Sound`] is the sound once started. The window's session uses the
//! real ones (QUIC, the system's devices); the tests give it an in-memory network and
//! sound that only says it is there, so that everything the session decides is tested
//! without a network or a sound card.

use crate::live::{Live, Reading, Tuning};
use palaver_media::Channels;
use palaver_media::jitter::Stats as JitterStats;
use palaver_media::process::Settings;
use palaver_net::quic::QuicOptions;
use palaver_net::reach::{DEFAULT_STUN_SERVERS, Network};
use palaver_proto::SECRET_BYTES;
use palaver_proto::{ParticipantId, Secret, Ticket};
use palaver_sdk::{Call, LinkGauge, Options, Participant, Status, entry_points};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// How often the network thread checks how late it is running.
const PULSE: Duration = Duration::from_millis(20);
/// The worst of each gauge over this many refreshes is what is shown: five seconds.
const REMEMBERED: usize = 25;

/// How often the view is brought up to date.
const REFRESH: Duration = Duration::from_millis(200);

/// What the window asks for.
#[derive(Debug, Clone)]
pub enum Order {
    /// Start a meeting, or join the one `ticket` is for.
    Start {
        name: String,
        ticket: Option<String>,
        /// Whether a STUN server may be asked for this node's public address.
        ask_outside: bool,
    },
    /// Someone sent this ticket back because they could not get in: dial them.
    Reach(String),
    Mute(bool),
    Leave,
}

/// Where things stand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Stage {
    #[default]
    Out,
    Starting,
    In,
}

/// One of the people in the meeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub me: bool,
    /// Whether this node has a connection of its own to them.
    pub linked: bool,
}

/// The largest of the last few values given: a gauge that is read for its worst.
#[derive(Debug, Clone)]
pub struct Worst<T> {
    recent: VecDeque<T>,
    kept: usize,
}

impl<T: Ord + Copy + Default> Worst<T> {
    #[must_use]
    pub const fn of_last(kept: usize) -> Self {
        Self {
            recent: VecDeque::new(),
            kept,
        }
    }

    /// Take the newest value and give the largest of those remembered.
    pub fn after(&mut self, value: T) -> T {
        self.recent.push_back(value);
        while self.recent.len() > self.kept.max(1) {
            self.recent.pop_front();
        }
        self.recent.iter().copied().max().unwrap_or_default()
    }
}

/// How one voice being played is faring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceLine {
    pub name: String,
    /// The playout delay its jitter buffer is aiming for.
    pub delay: Duration,
    pub stats: JitterStats,
}

/// One connection to another participant, as the details show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkLine {
    pub name: String,
    pub rtt: Duration,
    /// Packets sent on it, and how many of them were lost.
    pub sent: u64,
    pub lost: u64,
}

/// The numbers behind how the meeting sounds, each the worst of the last five seconds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    /// The longest the sound thread went between two turns.
    pub sound_gap: Duration,
    /// The most frames the microphone had waiting at one turn.
    pub sound_burst: usize,
    /// How late the network thread's timers have run.
    pub network_late: Duration,
    /// The most sound that waited for the speaker.
    pub speaker_lead: Duration,
    pub voices: Vec<VoiceLine>,
    /// The connections this node has, as they are now.
    pub links: Vec<LinkLine>,
}

/// The gauges as they are remembered between refreshes.
struct Gauges {
    sound_gap: Worst<Duration>,
    sound_burst: Worst<usize>,
    network_late: Worst<Duration>,
    speaker_lead: Worst<Duration>,
    /// The latest the pulse has been since the last refresh.
    late_now: Duration,
}

impl Gauges {
    const fn new() -> Self {
        Self {
            sound_gap: Worst::of_last(REMEMBERED),
            sound_burst: Worst::of_last(REMEMBERED),
            network_late: Worst::of_last(REMEMBERED),
            speaker_lead: Worst::of_last(REMEMBERED),
            late_now: Duration::ZERO,
        }
    }
}

/// What the window draws.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub stage: Stage,
    /// Why the last thing asked for did not happen.
    pub error: Option<String>,
    /// Something worth saying that is not an error.
    pub notice: Option<String>,
    /// The invitation to hand to someone else. It holds the meeting's secret.
    pub ticket: Option<String>,
    /// The ticket to send back to whoever invited this node, when nobody answers it.
    pub reply: Option<String>,
    pub network: Option<&'static str>,
    pub people: Vec<Person>,
    pub links: usize,
    pub sound: Reading,
    pub microphone: String,
    pub speaker: String,
    pub muted: bool,
    /// When this node got in.
    pub since: Option<Instant>,
    pub details: Details,
}

/// The sound of a meeting, once started.
pub trait Sound {
    /// The names of the microphone and the speaker.
    fn devices(&self) -> (String, String);
    fn set_muted(&self, muted: bool);
    /// Whether it is still carrying sound.
    fn running(&self) -> bool;
    fn reading(&self) -> Reading;
    /// Stop, and give the call back to be stopped; `None` if it was lost.
    fn stop(self) -> Option<Call>;
}

/// The sound could not be started.
pub struct NoSound {
    /// What to tell the user.
    pub why: String,
    /// The call, unused, to be stopped; `None` if it was lost.
    pub call: Option<Call>,
}

/// What a session stands on: where nodes come from, and how their sound is started.
pub trait World {
    type Sound: Sound;

    /// A node for the meeting with `secret`, and its call.
    ///
    /// # Errors
    ///
    /// With what to tell the user, when the node cannot be started.
    fn node(
        &mut self,
        secret: &Secret<[u8; SECRET_BYTES]>,
        options: Options,
    ) -> Result<(Participant, Call), String>;

    /// Start carrying `call` on a microphone and a speaker.
    ///
    /// # Errors
    ///
    /// When the sound cannot start.
    fn sound(&mut self, call: Call, playout: Channels) -> Result<Self::Sound, Box<NoSound>>;
}

impl Sound for Live {
    fn devices(&self) -> (String, String) {
        (self.microphone().name.clone(), self.speaker().name.clone())
    }

    fn set_muted(&self, muted: bool) {
        Self::set_muted(self, muted);
    }

    fn running(&self) -> bool {
        Self::running(self)
    }

    fn reading(&self) -> Reading {
        self.meter().read()
    }

    fn stop(self) -> Option<Call> {
        Self::stop(self)
    }
}

/// The real thing: nodes over QUIC, sound on the system's devices.
#[derive(Debug, Clone, Copy, Default)]
pub struct Real {
    pub tuning: Tuning,
}

impl World for Real {
    type Sound = Live;

    fn node(
        &mut self,
        secret: &Secret<[u8; SECRET_BYTES]>,
        options: Options,
    ) -> Result<(Participant, Call), String> {
        let options = Options {
            jitter: self.tuning.applied(options.jitter),
            ..options
        };
        Participant::on_quic(QuicOptions::default(), Vec::new(), secret, options)
            .map_err(|error| format!("Could not start: {error:?}"))
    }

    fn sound(&mut self, call: Call, playout: Channels) -> Result<Live, Box<NoSound>> {
        let cleaning = Some(Settings::default());
        Live::start(call, cleaning, playout, self.tuning.lead_ms).map_err(|failed| {
            Box::new(NoSound {
                why: format!("Sound: {}", failed.error),
                call: failed.call,
            })
        })
    }
}

/// The thread, and the two ways to it.
pub struct Session {
    orders: UnboundedSender<Order>,
    view: Arc<Mutex<View>>,
}

const fn name_of(network: Network) -> &'static str {
    match network {
        Network::Open(_) => "open to the internet",
        Network::BehindRouter(_) => "behind a router",
        Network::PerDestination => "behind a router that cannot be punched",
        Network::Unknown => "unknown",
    }
}

impl Session {
    /// Start the thread. It ends when the session is dropped.
    #[must_use]
    pub fn start(tuning: Tuning) -> Self {
        let (orders, taken) = unbounded_channel();
        let view = Arc::new(Mutex::new(View::default()));
        let shown = Arc::clone(&view);
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                set(&shown, |view| {
                    view.error = Some(String::from("could not start the network thread"));
                });
                return;
            };
            runtime.block_on(run(taken, &shown, Real { tuning }));
        });
        Self { orders, view }
    }

    /// A session over ways that already exist: whoever holds the other ends takes the
    /// orders and sets the view. For tests of the window.
    #[must_use]
    pub const fn over(orders: UnboundedSender<Order>, view: Arc<Mutex<View>>) -> Self {
        Self { orders, view }
    }

    pub fn order(&self, order: Order) {
        let _ = self.orders.send(order);
    }

    /// Where things stand now.
    #[must_use]
    pub fn view(&self) -> View {
        self.view
            .lock()
            .map(|view| view.clone())
            .unwrap_or_default()
    }
}

fn set(view: &Mutex<View>, change: impl FnOnce(&mut View)) {
    if let Ok(mut view) = view.lock() {
        change(&mut view);
    }
}

/// Take orders until there is nobody left to give them, keeping `view` up to date.
pub async fn run<W: World>(mut orders: UnboundedReceiver<Order>, view: &Mutex<View>, mut world: W) {
    while let Some(order) = orders.recv().await {
        if let Order::Start {
            name,
            ticket,
            ask_outside,
        } = order
        {
            set(view, |view| {
                *view = View {
                    stage: Stage::Starting,
                    ..View::default()
                };
            });
            let asked = Asked {
                name: &name,
                ticket: ticket.as_deref(),
                ask_outside,
            };
            let ended = meeting(asked, &mut orders, view, &mut world).await;
            set(view, |view| {
                *view = View {
                    error: ended.err(),
                    ..View::default()
                };
            });
        }
    }
}

/// Bring the view up to date with what the node and the sound report.
fn refresh(
    view: &Mutex<View>,
    participant: &Participant,
    invitation: Option<&Ticket>,
    sound: Reading,
    status: &Status,
    links: Vec<LinkGauge>,
    gauges: &mut Gauges,
) {
    let name_of = |id: &ParticipantId| {
        status
            .roster
            .iter()
            .position(|member| member == id)
            .and_then(|place| status.names.get(place))
            .map_or_else(|| String::from("someone"), |name| name.reveal().clone())
    };
    let links = links
        .into_iter()
        .map(|link| LinkLine {
            name: name_of(&link.peer),
            rtt: link.rtt,
            sent: link.sent,
            lost: link.lost,
        })
        .collect();
    // Whose voice each alias is, by the plan.
    let voices = sound
        .gauges
        .iter()
        .map(|gauge| {
            let name = status
                .microphones
                .iter()
                .find(|(alias, _)| *alias == gauge.alias)
                .map_or_else(|| String::from("someone"), |(_, id)| name_of(id));
            VoiceLine {
                name,
                delay: gauge.delay,
                stats: gauge.stats,
            }
        })
        .collect();
    let details = Details {
        sound_gap: gauges.sound_gap.after(sound.longest_gap),
        sound_burst: gauges.sound_burst.after(sound.largest_burst),
        network_late: gauges
            .network_late
            .after(std::mem::take(&mut gauges.late_now)),
        speaker_lead: gauges.speaker_lead.after(sound.lead),
        voices,
        links,
    };
    let me = participant.control().me().clone();
    let people: Vec<Person> = status
        .roster
        .iter()
        .zip(&status.names)
        .map(|(id, name)| Person {
            name: name.reveal().clone(),
            me: *id == me,
            linked: status.links.contains(id),
        })
        .collect();
    // Once in, anyone can bring someone else in: a ticket with this node first.
    let handing_on = match (invitation, people.is_empty()) {
        (Some(invitation), false) => {
            Some(participant.reply_to(invitation).to_text().expose().clone())
        }
        _ => None,
    };
    set(view, |view| {
        if !people.is_empty() {
            view.reply = None;
        }
        if handing_on.is_some() {
            view.ticket = handing_on;
        }
        view.people = people;
        view.links = status.links.len();
        // The loudest level starts again at each reading; keep the last one heard.
        view.sound = sound;
        view.details = details;
    });
}

/// The secret of the meeting an invitation is for, or a new one for a new meeting.
fn secret_for(invitation: Option<&Ticket>) -> Result<Secret<[u8; SECRET_BYTES]>, String> {
    match invitation {
        Some(ticket) => Ok(Secret::new(*ticket.secret.expose())),
        None => Ok(palaver_net::identity::Identity::generate()
            .map_err(|_| "The system gave no random bytes for a meeting secret.")?
            .secret()),
    }
}

/// Dial whoever sent `text` back, if it is a ticket; what to say about it either way.
async fn reach(participant: &Participant, text: &str) -> String {
    match Ticket::parse(text.trim()) {
        Ok(back) => {
            participant.reach(&back).await;
            String::from("Reaching for whoever sent that back.")
        }
        Err(error) => format!("That is not a ticket: {error:?}"),
    }
}

/// What a meeting is started with.
#[derive(Debug, Clone, Copy)]
struct Asked<'a> {
    name: &'a str,
    ticket: Option<&'a str>,
    ask_outside: bool,
}

/// One meeting, from starting the node to leaving. `Err` says why it ended, if it was
/// not by leaving.
async fn meeting<W: World>(
    asked: Asked<'_>,
    orders: &mut UnboundedReceiver<Order>,
    view: &Mutex<View>,
    world: &mut W,
) -> Result<(), String> {
    let invitation = match asked.ticket {
        Some(text) => Some(
            Ticket::parse(text.trim())
                .map_err(|error| format!("That is not a ticket: {error:?}"))?,
        ),
        None => None,
    };
    let secret = secret_for(invitation.as_ref())?;
    let playout = Channels::Stereo;
    let options = Options {
        playout,
        ..Options::named(asked.name)
    };
    let (mut participant, call) = world.node(&secret, options)?;
    let live = match world.sound(call, playout) {
        Ok(live) => live,
        Err(failed) => {
            if let Some(call) = failed.call {
                call.stop().await;
            }
            return Err(failed.why);
        }
    };
    let names: Vec<String> = if asked.ask_outside {
        DEFAULT_STUN_SERVERS.map(String::from).to_vec()
    } else {
        Vec::new()
    };

    let mut network = None;
    let mut handing_on = None;
    if let Some(invitation) = &invitation {
        participant.join(entry_points(invitation)).await;
    } else {
        // An invitation is about to be made: now is when the public address matters.
        network = Some(name_of(participant.find_network_asking(&names).await));
        participant.create_meeting().await;
        handing_on = Some(participant.ticket(&secret).to_text().expose().clone());
    }
    set(view, |view| {
        view.stage = Stage::In;
        view.since = Some(Instant::now());
        (view.microphone, view.speaker) = live.devices();
        view.network = network;
        view.ticket = handing_on;
    });

    let mut beat = tokio::time::interval(REFRESH);
    let mut pulse = tokio::time::interval(PULSE);
    pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut gauges = Gauges::new();
    let mut replied = false;
    let ended = loop {
        tokio::select! {
            _ = beat.tick() => {}
            due = pulse.tick() => {
                // How long after it was due this thread got to it.
                let late = tokio::time::Instant::now().saturating_duration_since(due);
                gauges.late_now = gauges.late_now.max(late);
                continue;
            }
            order = orders.recv() => match order {
                Some(Order::Reach(text)) => {
                    let said = reach(&participant, &text).await;
                    set(view, |view| view.notice = Some(said));
                }
                Some(Order::Mute(muted)) => {
                    live.set_muted(muted);
                    set(view, |view| view.muted = muted);
                }
                Some(Order::Start { .. }) => {}
                Some(Order::Leave) | None => break Ok(()),
            },
        }
        if !live.running() {
            break Err(String::from("The sound stopped."));
        }
        let status = participant.control().status().borrow().clone();
        if let (true, false, Some(invitation)) = (status.nobody_answers, replied, &invitation) {
            // It cannot get in on its own. Its public address is wanted now, for the
            // ticket that goes back to whoever invited it.
            replied = true;
            let found = name_of(participant.find_network_asking(&names).await);
            let reply = participant.reply_to(invitation).to_text().expose().clone();
            set(view, |view| {
                view.network = Some(found);
                view.reply = Some(reply);
            });
        }
        refresh(
            view,
            &participant,
            invitation.as_ref(),
            live.reading(),
            &status,
            participant.control().links().await,
            &mut gauges,
        );
    };
    if let Some(call) = live.stop() {
        call.stop().await;
    }
    participant.stop().await;
    ended
}
