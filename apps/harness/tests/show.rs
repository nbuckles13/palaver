//! A meeting's pictures between two machines, on an in-memory network: what is put in
//! one's camera slot comes out of the other's slot for someone else's picture.

#![expect(clippy::unwrap_used, reason = "a test may stop where it stands")]

use palaver_harness::picture::Slot;
use palaver_harness::show::{Projector, Screen};
use palaver_media::picture::{Pattern, Size};
use palaver_net::memory::MemoryNetwork;
use palaver_proto::Secret;
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
    let screen = Screen::new(
        camera.then(|| Arc::new(Slot::default())),
        (320, 180),
        300_000,
    );
    let options = Options {
        cameras: camera.then(|| screen.offer.clone()).into_iter().collect(),
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
    for each in [&ana, &bo, &cy] {
        until_tracks(each.participant.control().status(), 2).await;
    }

    let size = Size::new(320, 180).unwrap();
    let mut pattern = Pattern::new(size, 1);
    for _ in 0..30 {
        for filmed in [&mut ana, &mut cy] {
            let camera = filmed.screen.camera.clone().unwrap();
            camera.put(pattern.next_picture());
            filmed.projector.turn();
        }
        tokio::time::sleep(Duration::from_millis(33)).await;
        bo.projector.turn();
    }
    let id = |machine: &Machine| machine.participant.control().me().clone();
    // Bo is shown both, each in its own seat, and each is counted apart.
    for filmed in [&ana, &cy] {
        let seat = bo.screen.wall.get(&id(filmed)).unwrap();
        let (shown, picture) = seat.slot.newer_than(0).unwrap();
        assert!(shown >= 10, "only {shown} pictures shown");
        assert_eq!(picture.size(), size);
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
