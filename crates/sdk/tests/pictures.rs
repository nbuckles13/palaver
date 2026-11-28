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
use palaver_sdk::{Call, Offer, Options, Participant, Pictures, Shape, Status, entry_points};
use std::time::Duration;
use tokio::sync::watch;

type Both = Pictures<SoftwareEncoder, SoftwareDecoder>;

/// A participant with its pictures; the call is kept so that the node stays up.
struct Seat {
    participant: Participant,
    pictures: Both,
    _call: Call,
}

/// The small size of a camera, coded at `bitrate`.
fn low(bitrate: u32) -> Offer {
    Offer {
        name: b"low".to_vec(),
        shape: Shape {
            width: 320,
            height: 180,
            per_second: 30,
            bitrate,
        },
    }
}

fn seat(network: &MemoryNetwork, secret: &Identity, name: &str, camera: bool) -> Seat {
    let options = Options {
        cameras: camera.then(|| low(150_000)).into_iter().collect(),
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
    assert!(!ana.pictures.film(b"low", &pattern.next_picture()).unwrap());

    bo.pictures
        .watch(track.alias, SoftwareDecoder::new().unwrap());
    assert_eq!(bo.pictures.watching().collect::<Vec<_>>(), [track.alias]);
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(33)).await;
        ana.pictures.turn(|_, _| {});
        ana.pictures
            .film_all(&pattern.next_picture(), |track, size| {
                let shape = track.shape.unwrap();
                SoftwareEncoder::new(Settings::new(size, shape.bitrate))
            })
            .unwrap();
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
    assert!(!ana.pictures.film(b"low", &pattern.next_picture()).unwrap());
}

#[tokio::test(start_paused = true)]
async fn what_a_camera_offers_is_said_in_full_and_can_change_while_the_meeting_lasts() {
    let network = MemoryNetwork::new();
    let secret = Identity::generate().unwrap();
    let ana = seat(&network, &secret, "Ana", true);
    let bo = seat(&network, &secret, "Bo", false);
    ana.participant.create_meeting().await;
    let ticket = ana.participant.ticket(&secret.secret());
    bo.participant.join(entry_points(&ticket)).await;
    // Bo is told what the picture is, not left to read it from the name.
    until_tracks(bo.participant.control().status(), 1).await;
    let first = bo.pictures.tracks().remove(0);
    assert_eq!(first.rendition, b"low");
    assert_eq!(first.shape, Some(low(150_000).shape));

    // Ana lowers the rate of that size and adds a larger one.
    let high = Offer {
        name: b"high".to_vec(),
        shape: Shape {
            width: 640,
            height: 360,
            per_second: 30,
            bitrate: 500_000,
        },
    };
    ana.participant
        .offer_cameras(vec![low(100_000), high.clone()])
        .await;
    until_tracks(bo.participant.control().status(), 2).await;
    let tracks = bo.pictures.tracks();
    let again = tracks
        .iter()
        .find(|track| track.rendition == b"low")
        .unwrap();
    // The same track, under the same alias, with what it now is.
    assert_eq!(again.alias, first.alias);
    assert_eq!(again.shape, Some(low(100_000).shape));
    let larger = tracks
        .iter()
        .find(|track| track.rendition == b"high")
        .unwrap();
    assert_eq!(larger.shape, Some(high.shape));

    // And then has no camera at all.
    ana.participant.offer_cameras(Vec::new()).await;
    until_tracks(bo.participant.control().status(), 0).await;
}

/// A size of a camera `across` wide, in the shape of a screen.
fn sized(name: &str, across: u32) -> Offer {
    Offer {
        name: name.as_bytes().to_vec(),
        shape: Shape {
            width: across,
            height: across * 9 / 16,
            per_second: 30,
            bitrate: across * 500,
        },
    }
}

#[tokio::test(start_paused = true)]
async fn each_size_on_offer_is_made_only_while_it_is_watched() {
    let network = MemoryNetwork::new();
    let secret = Identity::generate().unwrap();
    let mut ana = seat(&network, &secret, "Ana", false);
    let mut bo = seat(&network, &secret, "Bo", false);
    ana.participant.create_meeting().await;
    let ticket = ana.participant.ticket(&secret.secret());
    bo.participant.join(entry_points(&ticket)).await;
    until_tracks(bo.participant.control().status(), 0).await;
    // Ana's camera is 640 across, offered whole, halved and quartered; and at a size
    // it cannot be made into.
    let offers = vec![
        sized("large", 640),
        sized("middle", 320),
        sized("small", 160),
        sized("odd", 480),
    ];
    ana.participant.offer_cameras(offers).await;
    until_tracks(bo.participant.control().status(), 4).await;
    let alias_of = |pictures: &Both, name: &[u8]| {
        let tracks = pictures.tracks();
        tracks.iter().find(|t| t.rendition == name).unwrap().alias
    };
    let (large, small) = (
        alias_of(&bo.pictures, b"large"),
        alias_of(&bo.pictures, b"small"),
    );

    // Bo watches the largest and the smallest.
    for alias in [large, small] {
        bo.pictures.watch(alias, SoftwareDecoder::new().unwrap());
    }
    let mut pattern = Pattern::new(Size::new(640, 360).unwrap(), 1);
    let mut shown: Vec<(u64, Size)> = Vec::new();
    let mut made: Vec<Vec<u8>> = Vec::new();
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(33)).await;
        ana.pictures.turn(|_, _| {});
        ana.pictures
            .film_all(&pattern.next_picture(), |track, size| {
                made.push(track.rendition.clone());
                SoftwareEncoder::new(Settings::new(size, track.shape.unwrap().bitrate))
            })
            .unwrap();
        bo.pictures
            .turn(|alias, picture| shown.push((alias, picture.size())));
    }
    // Each came at the size it said, and nothing was coded for the size nobody watches.
    made.sort();
    assert_eq!(made, [b"large".to_vec(), b"small".to_vec()]);
    assert!(!ana.pictures.has_camera(b"middle"));
    for (alias, across) in [(large, 640), (small, 160)] {
        let sizes: Vec<Size> = shown
            .iter()
            .filter(|(of, _)| *of == alias)
            .map(|(_, size)| *size)
            .collect();
        assert!(
            sizes.len() >= 10,
            "only {} pictures of {across}",
            sizes.len()
        );
        assert!(sizes.iter().all(|size| size.width() == across));
    }

    // A size the camera's pictures cannot be made into is refused when it is wanted.
    let odd = alias_of(&bo.pictures, b"odd");
    bo.pictures.watch(odd, SoftwareDecoder::new().unwrap());
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(33)).await;
        ana.pictures.turn(|_, _| {});
    }
    let refused = ana.pictures.film_all(&pattern.next_picture(), |_, size| {
        SoftwareEncoder::new(Settings::new(size, 100_000))
    });
    assert_eq!(refused, Err(palaver_media::video::Error::Size));
}
