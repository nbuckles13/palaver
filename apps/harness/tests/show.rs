//! A meeting's pictures between two machines, on an in-memory network: what is put in
//! one's camera slot comes out of the other's slot for someone else's picture.

#![expect(clippy::unwrap_used, reason = "a test may stop where it stands")]

use palaver_harness::picture::Slot;
use palaver_harness::show::{Projector, Screen, Seat};
use palaver_media::picture::{Pattern, Size};
use palaver_net::memory::MemoryNetwork;
use palaver_proto::{ParticipantId, Secret};
use palaver_sdk::{Call, Options, Participant, Status};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::timeout;

/// One machine: its node, what turns its pictures, and the screen they go to.
struct Machine {
    participant: Participant,
    projector: Projector,
    screen: Screen,
    _call: Call,
}

fn machine(network: &MemoryNetwork, name: &str, camera: bool) -> Machine {
    // A camera 640 by 360 is offered at that size, halved and quartered.
    let screen = Screen::new(camera.then(|| Arc::new(Slot::default())), (640, 360));
    let options = Options {
        cameras: if camera {
            screen.offers.clone()
        } else {
            Vec::new()
        },
        ..Options::named(name)
    };
    let (mut participant, call) =
        Participant::on_memory(network, &Secret::new([7; 32]), options).unwrap();
    let (port, status) = participant.video().unwrap();
    Machine {
        projector: Projector::new(port, status, screen.clone()),
        participant,
        screen,
        _call: call,
    }
}

/// Wait until the plan `status` shows lists `count` video tracks.
async fn until_tracks(mut status: watch::Receiver<Status>, count: usize) {
    let listed = status.wait_for(|status| status.videos.len() == count);
    assert!(timeout(Duration::from_secs(30), listed).await.is_ok());
}

/// The meeting's time, a picture at a time.
struct Clock {
    pattern: Pattern,
    at: Duration,
}

impl Clock {
    const fn new() -> Self {
        Self {
            pattern: Pattern::new(Size::new(640, 360).unwrap(), 1),
            at: Duration::ZERO,
        }
    }

    /// One thirtieth of a second: each of `filming` films a picture, and then each of
    /// `watching` shows what has come.
    async fn step(&mut self, filming: &mut [&mut Machine], watching: &mut [&mut Machine]) {
        for filmed in filming.iter_mut() {
            let camera = filmed.screen.camera.clone().unwrap();
            camera.put(self.pattern.next_picture());
            filmed.projector.turn(self.at);
        }
        tokio::time::sleep(Duration::from_millis(33)).await;
        self.at += Duration::from_millis(33);
        for watcher in watching.iter_mut() {
            watcher.projector.turn(self.at);
        }
    }
}

fn id(machine: &Machine) -> ParticipantId {
    machine.participant.control().me().clone()
}

#[tokio::test(start_paused = true)]
async fn everyone_with_a_camera_is_shown_to_everyone_else() {
    let network = MemoryNetwork::new();
    let mut ana = machine(&network, "Ana", true);
    let mut bo = machine(&network, "Bo", false);
    let mut cy = machine(&network, "Cy", true);
    ana.participant.create_meeting().await;
    for joiner in [&bo, &cy] {
        joiner
            .participant
            .join(vec![ana.participant.address().clone()])
            .await;
    }
    // Two cameras at three sizes each.
    for each in [&ana, &bo, &cy] {
        until_tracks(each.participant.control().status(), 6).await;
    }
    // Bo draws its pictures 90 pixels high, so the smallest size will do.
    bo.screen.wall.set_tall(90);
    let mut clock = Clock::new();
    for _ in 0..30 {
        clock.step(&mut [&mut ana, &mut cy], &mut [&mut bo]).await;
    }
    // Bo is shown both, each in its own seat, and each is counted apart.
    for filmed in [&ana, &cy] {
        let seat = bo.screen.wall.get(&id(filmed)).unwrap();
        let (shown, picture) = seat.slot.newer_than(0).unwrap();
        assert!(shown >= 10, "only {shown} pictures shown");
        assert_eq!(picture.size(), Size::new(160, 90).unwrap());
        assert_eq!(seat.count.read().frames, shown);
        let coded = filmed.screen.meter.sent.read();
        assert!(coded.frames >= shown && coded.bytes > 0);
        assert_eq!(filmed.screen.meter.failed(), None);
    }
    assert_eq!(bo.screen.meter.failed(), None);
    // Each of the two with a camera is shown the other, and nobody is shown Bo, who
    // offered none and sent nothing.
    assert!(ana.screen.wall.get(&id(&cy)).is_some());
    assert!(ana.screen.wall.get(&id(&bo)).is_none());
    assert_eq!(bo.screen.meter.sent.read().frames, 0);
}

#[tokio::test(start_paused = true)]
async fn a_picture_drawn_larger_changes_to_a_larger_size_without_stopping() {
    let network = MemoryNetwork::new();
    let mut ana = machine(&network, "Ana", true);
    let mut bo = machine(&network, "Bo", false);
    ana.participant.create_meeting().await;
    bo.participant
        .join(vec![ana.participant.address().clone()])
        .await;
    until_tracks(bo.participant.control().status(), 3).await;
    let seat = bo.screen.wall.of(&id(&ana));
    let across = |seat: &Seat| seat.slot.newer_than(0).map(|(_, p)| p.size().width());
    let mut clock = Clock::new();

    bo.screen.wall.set_tall(90);
    for _ in 0..15 {
        clock.step(&mut [&mut ana], &mut [&mut bo]).await;
    }
    assert_eq!(across(&seat), Some(160));
    let small = bo.projector.showing()[&id(&ana)];

    // The window is dragged larger for a moment and back: nothing changes.
    bo.screen.wall.set_tall(300);
    for _ in 0..6 {
        clock.step(&mut [&mut ana], &mut [&mut bo]).await;
    }
    bo.screen.wall.set_tall(90);
    for _ in 0..30 {
        clock.step(&mut [&mut ana], &mut [&mut bo]).await;
    }
    assert_eq!(bo.projector.showing()[&id(&ana)], small);
    assert_eq!(across(&seat), Some(160));

    // It is made larger and stays so: the size changes, and a picture comes at
    // every step on the way, of the one size or the other.
    bo.screen.wall.set_tall(300);
    let mut before = seat.slot.count();
    for _ in 0..45 {
        clock.step(&mut [&mut ana], &mut [&mut bo]).await;
        let now = seat.slot.count();
        assert!(now > before, "the picture stopped at {:?}", clock.at);
        before = now;
    }
    assert_eq!(across(&seat), Some(640));
    assert_ne!(bo.projector.showing()[&id(&ana)], small);
    assert_eq!(bo.screen.meter.failed(), None);
    assert_eq!(ana.screen.meter.failed(), None);
}
