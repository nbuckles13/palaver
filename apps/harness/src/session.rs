//! The meeting behind the window.
//!
//! The window draws and takes clicks; it must never wait. So the node, its network and
//! its sound live on a thread of their own, which takes [`Order`]s and keeps a [`View`]
//! up to date for the window to draw.

use crate::live::{Live, Reading};
use palaver_media::Channels;
use palaver_media::process::Settings;
use palaver_net::quic::QuicOptions;
use palaver_net::reach::{DEFAULT_STUN_SERVERS, Network};
use palaver_proto::{Secret, Ticket};
use palaver_sdk::{Options, Participant, Status, entry_points};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

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
    pub fn start() -> Self {
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
            runtime.block_on(run(taken, &shown));
        });
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

async fn run(mut orders: UnboundedReceiver<Order>, view: &Mutex<View>) {
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
            let ended = meeting(&name, ticket.as_deref(), ask_outside, &mut orders, view).await;
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
    live: &Live,
    status: &Status,
) {
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
    let sound = live.meter().read();
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
    });
}

/// One meeting, from starting the node to leaving. `Err` says why it ended, if it was
/// not by leaving.
async fn meeting(
    name: &str,
    ticket: Option<&str>,
    ask_outside: bool,
    orders: &mut UnboundedReceiver<Order>,
    view: &Mutex<View>,
) -> Result<(), String> {
    let invitation = match ticket {
        Some(text) => Some(
            Ticket::parse(text.trim())
                .map_err(|error| format!("That is not a ticket: {error:?}"))?,
        ),
        None => None,
    };
    let secret = match &invitation {
        Some(ticket) => Secret::new(*ticket.secret.expose()),
        None => palaver_net::identity::Identity::generate()
            .map_err(|_| "The system gave no random bytes for a meeting secret.")?
            .secret(),
    };
    let playout = Channels::Stereo;
    let options = Options {
        playout,
        ..Options::named(name)
    };
    let (mut participant, call) =
        Participant::on_quic(QuicOptions::default(), Vec::new(), &secret, options)
            .map_err(|error| format!("Could not start: {error:?}"))?;
    let live = match Live::start(call, Some(Settings::default()), playout) {
        Ok(live) => live,
        Err(failed) => {
            if let Some(call) = failed.call {
                call.stop().await;
            }
            return Err(format!("Sound: {}", failed.error));
        }
    };
    let names: Vec<String> = if ask_outside {
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
        view.microphone.clone_from(&live.microphone().name);
        view.speaker.clone_from(&live.speaker().name);
        view.network = network;
        view.ticket = handing_on;
    });

    let mut beat = tokio::time::interval(REFRESH);
    let mut replied = false;
    let ended = loop {
        tokio::select! {
            _ = beat.tick() => {}
            order = orders.recv() => match order {
                Some(Order::Reach(text)) => {
                    let said = match Ticket::parse(text.trim()) {
                        Ok(back) => {
                            participant.reach(&back).await;
                            String::from("Reaching for whoever sent that back.")
                        }
                        Err(error) => format!("That is not a ticket: {error:?}"),
                    };
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
        refresh(view, &participant, invitation.as_ref(), &live, &status);
    };
    if let Some(call) = live.stop() {
        call.stop().await;
    }
    participant.stop().await;
    ended
}
