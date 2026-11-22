//! The window, clicked through with no display: what it shows for each state of the
//! meeting, and what it asks the session for when its buttons are pressed.

#![expect(clippy::unwrap_used, reason = "a test may stop where it stands")]

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use palaver_harness::check::Finding;
use palaver_harness::session::{Details, LinkLine, Order, Person, Session, Stage, View, VoiceLine};
use palaver_harness::ui::{Setup, Window};
use palaver_harness::words;
use palaver_media::jitter::Stats;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// A window with the test holding the session's other ends.
struct Desk {
    harness: Harness<'static, Window>,
    orders: UnboundedReceiver<Order>,
    view: Arc<Mutex<View>>,
}

impl Desk {
    fn with(setup: Setup) -> Self {
        let (sender, orders) = unbounded_channel();
        let view = Arc::new(Mutex::new(View::default()));
        let window = Window::over(Session::over(sender, Arc::clone(&view)), None, setup);
        let mut harness = Harness::new_ui_state(|ui, window: &mut Window| window.draw(ui), window);
        harness.step();
        Self {
            harness,
            orders,
            view,
        }
    }

    fn named(name: &str) -> Self {
        Self::with(Setup {
            name: Some(name.into()),
            ..Setup::default()
        })
    }

    fn show(&mut self, change: impl FnOnce(&mut View)) {
        change(&mut self.view.lock().unwrap());
        self.harness.step();
    }

    fn click(&mut self, label: &str) {
        self.harness.get_by_label(label).click();
        // One step for the click to land, one for what it changed to be drawn.
        self.harness.step();
        self.harness.step();
    }

    fn shows(&self, label: &str) -> bool {
        self.harness.query_by_label(label).is_some()
    }

    fn order(&mut self) -> Option<Order> {
        self.orders.try_recv().ok()
    }
}

fn in_a_meeting(view: &mut View) {
    view.stage = Stage::In;
    view.since = Some(Instant::now());
    view.ticket = Some(String::from("palaver:abc"));
    view.people = vec![
        Person {
            name: String::from("Ana"),
            me: true,
            linked: false,
        },
        Person {
            name: String::from("Bo"),
            me: false,
            linked: true,
        },
    ];
    view.links = 1;
}

#[test]
fn a_meeting_cannot_be_started_without_a_name() {
    let mut desk = Desk::with(Setup::default());
    assert!(desk.shows("A name first."));
    desk.click("Start a meeting");
    assert!(desk.order().is_none());
}

#[test]
fn starting_a_meeting_asks_the_session_for_one_under_the_name() {
    let mut desk = Desk::named(" Ana ");
    assert!(!desk.shows("A name first."));
    desk.click("Start a meeting");
    let Some(Order::Start {
        name,
        ticket,
        ask_outside,
    }) = desk.order()
    else {
        panic!("no order to start");
    };
    assert_eq!((name.as_str(), ticket, ask_outside), ("Ana", None, true));

    // With the box unticked, no public server is to be asked.
    let mut desk = Desk::named("Ana");
    desk.click("Find my public address (for people outside this network; asks a public server)");
    desk.click("Start a meeting");
    assert!(matches!(
        desk.order(),
        Some(Order::Start {
            ask_outside: false,
            ..
        })
    ));
}

#[test]
fn a_ticket_left_in_the_file_is_joined_by_and_a_new_meetings_ticket_is_left_there() {
    let path = std::env::temp_dir().join(format!("palaver-window-test-{}.txt", std::process::id()));
    std::fs::write(&path, "palaver:fromfile\n").unwrap();
    let setup = Setup {
        name: Some(String::from("Bo")),
        ticket_file: Some(path.to_string_lossy().into_owned()),
        ..Setup::default()
    };
    let mut desk = Desk::with(setup);
    desk.click("Join");
    let Some(Order::Start { name, ticket, .. }) = desk.order() else {
        panic!("no order to join");
    };
    assert_eq!(
        (name.as_str(), ticket.as_deref()),
        ("Bo", Some("palaver:fromfile"))
    );

    // In a meeting with a ticket of its own, that ticket takes the file's place.
    desk.show(in_a_meeting);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "palaver:abc\n");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn what_went_wrong_is_shown_outside_and_starting_is_said() {
    let mut desk = Desk::named("Ana");
    desk.show(|view| view.error = Some(String::from("Sound: no sound device")));
    assert!(desk.shows("Sound: no sound device"));
    desk.show(|view| {
        view.error = None;
        view.stage = Stage::Starting;
    });
    assert!(desk.shows("starting…"));
    // While starting, another start is not taken.
    desk.click("Start a meeting");
    assert!(desk.order().is_none());
}

#[test]
fn in_a_meeting_the_buttons_ask_for_what_they_say() {
    let mut desk = Desk::named("Ana");
    desk.show(in_a_meeting);
    assert!(desk.shows("1 link · direct · no server"));
    assert!(!desk.shows("Start a meeting"));

    desk.click("Mute");
    assert!(matches!(desk.order(), Some(Order::Mute(true))));
    desk.show(|view| view.muted = true);
    desk.click("Unmute");
    assert!(matches!(desk.order(), Some(Order::Mute(false))));

    desk.click("Bring someone in");
    assert!(desk.shows("The ticket is on the clipboard. Send it to them."));

    desk.click("Leave");
    assert!(matches!(desk.order(), Some(Order::Leave)));
}

#[test]
fn in_a_meeting_a_microphone_that_gives_nothing_usable_is_said_under_the_meter() {
    let mut desk = Desk::named("Ana");
    desk.show(in_a_meeting);
    let processed = words::microphone_hint(Finding::Processed, words::System::HERE);
    assert!(!desk.shows(processed));
    desk.show(|view| view.sound.microphone = Finding::Processed);
    assert!(desk.shows(processed));
    // A faint voice is not: it would come and go with each word.
    desk.show(|view| view.sound.microphone = Finding::Faint);
    assert!(!desk.shows(words::microphone_hint(Finding::Faint, words::System::HERE)));
}

#[test]
fn the_details_are_closed_until_asked_for_and_then_say_how_each_voice_is_faring() {
    let mut desk = Desk::named("Ana");
    desk.show(|view| {
        in_a_meeting(view);
        view.microphone = String::from("a microphone");
        view.speaker = String::from("a speaker");
        view.details = Details {
            sound_gap: Duration::from_millis(11),
            sound_burst: 1,
            network_late: Duration::from_millis(2),
            speaker_lead: Duration::from_millis(40),
            voices: vec![VoiceLine {
                name: String::from("Bo"),
                delay: Duration::from_millis(60),
                stats: Stats {
                    played: 500,
                    ..Stats::default()
                },
            }],
            links: vec![LinkLine {
                name: String::from("Bo"),
                rtt: Duration::from_millis(7),
                sent: 2000,
                lost: 3,
            }],
        };
    });
    let voice = "Bo: heard 110 ms after arriving (buffer 60 ms) · played 500 · rebuilt 0 · lost 0 · skipped 0 · late 0";
    assert!(!desk.shows(voice));
    desk.click("Details");
    assert!(desk.shows(voice));
    assert!(desk.shows("a microphone → a speaker"));
    assert!(desk.shows("network thread: timers up to 2 ms late"));
    assert!(desk.shows("link to Bo: round trip 7 ms · lost 3 of 2000 packets sent (0.1%)"));
    assert!(desk.shows(
        "sound thread: longest wait 11 ms · most frames at once 1 · speaker ran dry for 0 ms · fell behind by 0 ms"
    ));
}

#[test]
fn the_ticket_coming_back_has_both_its_ends() {
    let mut desk = Desk::named("Bo");
    desk.show(|view| {
        view.stage = Stage::In;
        view.reply = Some(String::from("palaver:back"));
    });
    assert!(desk.shows("Nobody can reach this machine yet."));
    desk.click("Copy the ticket to send back");
    assert!(desk.shows("The ticket to send back is on the clipboard."));

    // The other end: nothing to reach for until something is pasted.
    desk.click("Reach");
    assert!(desk.order().is_none());
    desk.show(|view| view.notice = Some(String::from("Reaching for whoever sent that back.")));
    assert!(desk.shows("Reaching for whoever sent that back."));
}
