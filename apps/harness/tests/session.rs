//! The meeting behind the window, on an in-memory network with sound that only says it
//! is there: what the window is shown as orders are given and the meeting changes.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test may stop where it stands"
)]

use palaver_harness::live::Reading;
use palaver_harness::picture::Slot;
use palaver_harness::session::{NoSound, Order, Sound, Stage, View, World, Worst, run};
use palaver_harness::show::Screen;
use palaver_media::Channels;
use palaver_net::memory::MemoryNetwork;
use palaver_proto::{SECRET_BYTES, Secret};
use palaver_sdk::{Call, Options, Participant};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// Sound that carries nothing: it keeps the call, and can be told to have stopped.
struct Quiet {
    call: Call,
    switches: Switches,
}

/// What a test can see of the sound, and do to it.
#[derive(Clone, Default)]
struct Switches {
    muted: Arc<AtomicBool>,
    broken: Arc<AtomicBool>,
}

impl Sound for Quiet {
    fn devices(&self) -> (String, String) {
        (String::from("a microphone"), String::from("a speaker"))
    }

    fn set_muted(&self, muted: bool) {
        self.switches.muted.store(muted, Ordering::Relaxed);
    }

    fn running(&self) -> bool {
        !self.switches.broken.load(Ordering::Relaxed)
    }

    fn reading(&self) -> Reading {
        Reading::default()
    }

    fn stop(self) -> Option<Call> {
        Some(self.call)
    }
}

/// Nodes on an in-memory network.
struct Memory {
    network: MemoryNetwork,
    switches: Switches,
    /// The sound cannot be started.
    no_sound: bool,
    /// Nodes made here can be reached only by those they have dialled.
    behind_nat: bool,
}

impl World for Memory {
    type Sound = Quiet;

    fn node(
        &mut self,
        secret: &Secret<[u8; SECRET_BYTES]>,
        options: Options,
    ) -> Result<(Participant, Call), String> {
        let (participant, call) = Participant::on_memory(&self.network, secret, options)
            .map_err(|error| format!("Could not start: {error:?}"))?;
        if self.behind_nat {
            self.network.put_behind_nat(participant.address().key);
        }
        Ok((participant, call))
    }

    fn sound(&mut self, call: Call, _: Channels) -> Result<Quiet, Box<NoSound>> {
        if self.no_sound {
            return Err(Box::new(NoSound {
                why: String::from("Sound: no sound device"),
                call: Some(call),
            }));
        }
        Ok(Quiet {
            call,
            switches: self.switches.clone(),
        })
    }
}

/// One person's session: the way to give it orders, and what it shows.
struct Seat {
    orders: UnboundedSender<Order>,
    view: Arc<Mutex<View>>,
    switches: Switches,
}

impl Seat {
    fn at(network: &MemoryNetwork) -> Self {
        Self::with(network, false, false)
    }

    fn with(network: &MemoryNetwork, no_sound: bool, behind_nat: bool) -> Self {
        Self::seated(network, no_sound, behind_nat, None)
    }

    /// A seat that shows pictures, and has a camera or not.
    fn with_screen(network: &MemoryNetwork, camera: bool) -> Self {
        let camera = camera.then(|| Arc::new(Slot::default()));
        let screen = Screen::new(camera, 180, 300_000);
        Self::seated(network, false, false, Some(screen))
    }

    fn seated(
        network: &MemoryNetwork,
        no_sound: bool,
        behind_nat: bool,
        screen: Option<Screen>,
    ) -> Self {
        let (orders, taken) = unbounded_channel();
        let view = Arc::new(Mutex::new(View::default()));
        let switches = Switches::default();
        let world = Memory {
            network: network.clone(),
            switches: switches.clone(),
            no_sound,
            behind_nat,
        };
        let shown = Arc::clone(&view);
        tokio::spawn(async move { run(taken, &shown, world, screen).await });
        Self {
            orders,
            view,
            switches,
        }
    }

    fn start(&self, name: &str, ticket: Option<String>) {
        self.orders
            .send(Order::Start {
                name: name.into(),
                ticket,
                ask_outside: false,
            })
            .unwrap();
    }

    fn view(&self) -> View {
        self.view.lock().unwrap().clone()
    }

    /// Wait, for up to `seconds`, until what is shown satisfies `test`.
    async fn until(&self, seconds: u64, what: &str, test: impl Fn(&View) -> bool) -> View {
        for _ in 0..seconds * 10 {
            if test(&self.view()) {
                return self.view();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("never {what}: {:?}", self.view());
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.view().people.into_iter().map(|p| p.name).collect();
        names.sort();
        names
    }
}

#[tokio::test(start_paused = true)]
async fn a_meeting_is_started_and_someone_joins_by_its_ticket() {
    let network = MemoryNetwork::new();
    let (ana, bo) = (Seat::at(&network), Seat::at(&network));
    assert_eq!(ana.view().stage, Stage::Out);

    ana.start("Ana", None);
    let alone = ana
        .until(5, "in a meeting of one", |view| view.people.len() == 1)
        .await;
    assert_eq!(alone.stage, Stage::In);
    assert_eq!(
        (alone.people[0].name.as_str(), alone.people[0].me),
        ("Ana", true)
    );
    assert_eq!(alone.microphone, "a microphone");
    assert!(alone.since.is_some() && alone.error.is_none());
    let ticket = alone.ticket.unwrap();
    assert!(ticket.starts_with("palaver:"));

    bo.start("Bo", Some(ticket));
    let joined = bo
        .until(10, "in a meeting of two", |view| view.people.len() == 2)
        .await;
    ana.until(10, "joined by Bo", |view| view.people.len() == 2)
        .await;
    assert_eq!(ana.names(), ["Ana", "Bo"]);
    assert_eq!(bo.names(), ["Ana", "Bo"]);
    // Each has one link, to the other, and knows which of the two it is.
    assert_eq!((ana.view().links, joined.links), (1, 1));
    let bo_sees = joined
        .people
        .iter()
        .find(|person| person.name == "Ana")
        .unwrap();
    assert!(bo_sees.linked && !bo_sees.me);
    // The details name the link by who is at its other end.
    let named = ana
        .until(5, "the link to Bo in the details", |view| {
            view.details.links.len() == 1
        })
        .await;
    assert_eq!(named.details.links[0].name, "Bo");
    // Once in, the joiner has a ticket of its own to bring someone else in with.
    assert!(
        joined
            .ticket
            .is_some_and(|ticket| ticket.starts_with("palaver:"))
    );
    assert!(joined.reply.is_none());
}

#[tokio::test(start_paused = true)]
async fn what_cannot_be_started_is_said_and_the_window_is_back_outside() {
    let network = MemoryNetwork::new();
    let seat = Seat::at(&network);
    seat.start("Ana", Some(String::from("not a ticket")));
    let view = seat.until(5, "refused", |view| view.error.is_some()).await;
    assert_eq!(view.stage, Stage::Out);
    assert!(view.error.unwrap().starts_with("That is not a ticket"));

    // The next thing asked for starts clean.
    seat.start("Ana", None);
    let view = seat.until(5, "in", |view| view.stage == Stage::In).await;
    assert!(view.error.is_none());

    let silent = Seat::with(&network, true, false);
    silent.start("Bo", None);
    let view = silent
        .until(5, "refused", |view| view.error.is_some())
        .await;
    assert_eq!(
        (view.stage, view.error.as_deref()),
        (Stage::Out, Some("Sound: no sound device"))
    );
}

#[tokio::test(start_paused = true)]
async fn mute_reaches_the_sound_and_leaving_ends_the_meeting() {
    let network = MemoryNetwork::new();
    let seat = Seat::at(&network);
    seat.start("Ana", None);
    seat.until(5, "in", |view| view.stage == Stage::In).await;

    seat.orders.send(Order::Mute(true)).unwrap();
    seat.until(5, "muted", |view| view.muted).await;
    assert!(seat.switches.muted.load(Ordering::Relaxed));
    seat.orders.send(Order::Mute(false)).unwrap();
    seat.until(5, "unmuted", |view| !view.muted).await;
    assert!(!seat.switches.muted.load(Ordering::Relaxed));

    seat.orders.send(Order::Leave).unwrap();
    let view = seat.until(5, "out", |view| view.stage == Stage::Out).await;
    assert!(view.error.is_none() && view.people.is_empty() && view.ticket.is_none());
}

#[tokio::test(start_paused = true)]
async fn sound_that_stops_by_itself_ends_the_meeting_and_says_so() {
    let network = MemoryNetwork::new();
    let seat = Seat::at(&network);
    seat.start("Ana", None);
    seat.until(5, "in", |view| view.stage == Stage::In).await;
    seat.switches.broken.store(true, Ordering::Relaxed);
    let view = seat.until(5, "out", |view| view.stage == Stage::Out).await;
    assert_eq!(view.error.as_deref(), Some("The sound stopped."));
}

#[tokio::test(start_paused = true)]
async fn a_joiner_nobody_answers_is_shown_a_ticket_to_send_back_and_it_gets_them_in() {
    let network = MemoryNetwork::new();
    // Ana can be reached only by those she has dialled: a firewall, a router.
    let ana = Seat::with(&network, false, true);
    let bo = Seat::at(&network);
    ana.start("Ana", None);
    let ticket = ana
        .until(5, "in", |view| view.ticket.is_some())
        .await
        .ticket
        .unwrap();

    bo.start("Bo", Some(ticket));
    let stuck = bo
        .until(30, "shown a ticket to send back", |view| {
            view.reply.is_some()
        })
        .await;
    assert_eq!(stuck.stage, Stage::In);
    assert!(stuck.people.is_empty());

    // Pasted into Ana's window, it makes her reach for Bo.
    ana.orders.send(Order::Reach(stuck.reply.unwrap())).unwrap();
    bo.until(30, "in a meeting of two", |view| view.people.len() == 2)
        .await;
    ana.until(30, "joined by Bo", |view| view.people.len() == 2)
        .await;
    assert_eq!(
        ana.view().notice.as_deref(),
        Some("Reaching for whoever sent that back.")
    );
    // The ticket to send back has done its work and is gone.
    assert!(bo.view().reply.is_none());

    // Something that is not a ticket is said to be so.
    ana.orders
        .send(Order::Reach(String::from("hello")))
        .unwrap();
    ana.until(5, "told", |view| {
        view.notice
            .as_deref()
            .is_some_and(|notice| notice.starts_with("That is not a ticket"))
    })
    .await;
}

#[test]
fn a_gauge_is_read_for_its_worst_over_the_last_few() {
    let mut worst = Worst::of_last(3);
    assert_eq!(worst.after(5), 5);
    assert_eq!(worst.after(2), 5);
    assert_eq!(worst.after(1), 5);
    // The five has been pushed out.
    assert_eq!(worst.after(1), 2);
    assert_eq!(worst.after(0), 1);
    assert_eq!(worst.after(9), 9);
}

#[tokio::test(start_paused = true)]
async fn whoever_has_a_camera_is_named_to_the_others_as_the_picture_to_show() {
    let network = MemoryNetwork::new();
    let (ana, bo) = (
        Seat::with_screen(&network, true),
        Seat::with_screen(&network, false),
    );
    ana.start("Ana", None);
    let alone = ana
        .until(5, "in a meeting of one", |view| view.people.len() == 1)
        .await;
    bo.start("Bo", alone.ticket);
    // Bo is shown Ana's picture; Ana has nobody's to show, as Bo offered no camera.
    bo.until(10, "shown Ana's picture", |view| {
        view.far.as_deref() == Some("Ana")
    })
    .await;
    let both = ana
        .until(5, "two people", |view| view.people.len() == 2)
        .await;
    assert_eq!(both.far, None);
}
