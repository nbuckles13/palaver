//! Two participants on an in-memory network, one with a camera: what the other is shown.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test may stop where it stands"
)]

use palaver_media::picture::{Pattern, Picture, Size};
use palaver_media::video::{Settings, SoftwareDecoder, SoftwareEncoder};
use palaver_net::identity::Identity;
use palaver_net::memory::MemoryNetwork;
use palaver_sdk::{Call, Options, Participant, Pictures, Status, entry_points};
use std::time::Duration;
use tokio::sync::watch;

type Both = Pictures<SoftwareEncoder, SoftwareDecoder>;

/// A participant with its pictures; the call is kept so that the node stays up.
struct Seat {
    participant: Participant,
    pictures: Both,
    _call: Call,
}

fn seat(network: &MemoryNetwork, secret: &Identity, name: &str, camera: bool) -> Seat {
    let options = Options {
        camera: camera.then(|| b"180p".to_vec()),
        ..Options::named(name)
    };
    let (mut participant, call) =
        Participant::on_memory(network, &secret.secret(), options).unwrap();
    let (port, status) = participant.video().unwrap();
    assert!(participant.video().is_none(), "there is one door");
    Seat {
        participant,
        pictures: Pictures::new(port, status),
        _call: call,
    }
}

/// Wait until the plan `status` shows has `count` video tracks.
async fn until_tracks(mut status: watch::Receiver<Status>, count: usize) {
    let enough = status.wait_for(|status| status.videos.len() == count);
    if tokio::time::timeout(Duration::from_secs(10), enough)
        .await
        .is_err()
    {
        panic!("never {count} video tracks");
    }
}

#[tokio::test(start_paused = true)]
async fn a_camera_is_coded_only_while_watched_and_shown_at_the_other_end() {
    let network = MemoryNetwork::new();
    let secret = Identity::generate().unwrap();
    let mut ana = seat(&network, &secret, "Ana", true);
    let mut bo = seat(&network, &secret, "Bo", false);
    ana.participant.create_meeting().await;
    let ticket = ana.participant.ticket(&secret.secret());
    bo.participant.join(entry_points(&ticket)).await;
    // Only Ana offered a camera, and each knows whose it is.
    until_tracks(bo.participant.control().status(), 1).await;
    until_tracks(ana.participant.control().status(), 1).await;
    let track = bo.pictures.tracks().remove(0);
    assert!(!track.own);
    assert!(ana.pictures.tracks().iter().all(|track| track.own));

    let size = Size::new(320, 180).unwrap();
    let mut pattern = Pattern::new(size, 1);
    let mut shown: Vec<Picture> = Vec::new();
    // Nobody watches: nothing is coded, and no encoder is needed.
    ana.pictures.turn(|_, _| {});
    assert!(!ana.pictures.wanted());
    assert!(!ana.pictures.film(&pattern.next_picture()).unwrap());

    bo.pictures
        .watch(track.alias, SoftwareDecoder::new().unwrap());
    assert_eq!(bo.pictures.watching().collect::<Vec<_>>(), [track.alias]);
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(33)).await;
        ana.pictures.turn(|_, _| {});
        if ana.pictures.wanted() && !ana.pictures.has_camera() {
            let encoder = SoftwareEncoder::new(Settings::new(size, 300_000)).unwrap();
            ana.pictures.set_camera(encoder);
        }
        ana.pictures.film(&pattern.next_picture()).unwrap();
        bo.pictures.turn(|alias, picture| {
            assert_eq!(alias, track.alias);
            shown.push(picture);
        });
    }
    assert!(shown.len() >= 10, "only {} pictures shown", shown.len());
    assert!(shown.iter().all(|picture| picture.size() == size));

    // Bo looks away: Ana is told, and makes nothing more.
    bo.pictures.unwatch(track.alias);
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(33)).await;
        ana.pictures.turn(|_, _| {});
    }
    assert!(!ana.pictures.wanted());
    assert!(!ana.pictures.film(&pattern.next_picture()).unwrap());
}
