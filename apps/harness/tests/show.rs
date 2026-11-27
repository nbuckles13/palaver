//! A meeting's pictures between two machines, on an in-memory network: what is put in
//! one's camera slot comes out of the other's slot for someone else's picture.

#![expect(clippy::unwrap_used, reason = "a test may stop where it stands")]

use palaver_harness::picture::Slot;
use palaver_harness::show::{Projector, Screen};
use palaver_media::picture::{Pattern, Size};
use palaver_net::memory::MemoryNetwork;
use palaver_proto::Secret;
use palaver_sdk::{Call, Options, Participant};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// One machine: its node, what turns its pictures, and the screen they go to.
struct Machine {
    participant: Participant,
    projector: Projector,
    screen: Screen,
    _call: Call,
}

fn machine(network: &MemoryNetwork, name: &str, camera: bool) -> Machine {
    let screen = Screen::new(camera.then(|| Arc::new(Slot::default())), 180, 300_000);
    let options = Options {
        camera: camera.then(|| screen.rendition.clone()),
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

#[tokio::test(start_paused = true)]
async fn what_one_machine_films_is_shown_on_the_other() {
    let network = MemoryNetwork::new();
    let mut ana = machine(&network, "Ana", true);
    let mut bo = machine(&network, "Bo", false);
    ana.participant.create_meeting().await;
    bo.participant
        .join(vec![ana.participant.address().clone()])
        .await;
    let mut status = bo.participant.control().status();
    let listed = status.wait_for(|status| status.videos.len() == 1);
    assert!(timeout(Duration::from_secs(30), listed).await.is_ok());

    let size = Size::new(320, 180).unwrap();
    let mut pattern = Pattern::new(size, 1);
    let camera = ana.screen.camera.clone().unwrap();
    for _ in 0..30 {
        camera.put(pattern.next_picture());
        ana.projector.turn();
        tokio::time::sleep(Duration::from_millis(33)).await;
        bo.projector.turn();
    }
    // Bo is shown Ana's picture, and both count what that cost.
    let (shown, picture) = bo.screen.far.newer_than(0).unwrap();
    assert!(shown >= 10, "only {shown} pictures shown");
    assert_eq!(picture.size(), size);
    let (sent, decoded) = (ana.screen.meter.sent.read(), bo.screen.meter.shown.read());
    assert!(sent.frames >= shown && sent.bytes > 0);
    assert_eq!(decoded.frames, shown);
    assert_eq!(ana.screen.meter.failed(), None);
    assert_eq!(bo.screen.meter.failed(), None);
    // Ana has nobody to watch: Bo offered no camera.
    assert_eq!(ana.screen.far.count(), 0);
    assert_eq!(bo.screen.meter.sent.read().frames, 0);
}
