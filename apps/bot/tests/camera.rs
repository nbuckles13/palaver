//! A bot with a camera, and someone watching it, over the in-memory transport.

use palaver_bot::{Bot, FRAME, PICTURE, Script};
use palaver_media::video::{SoftwareDecoder, SoftwareEncoder};
use palaver_net::memory::MemoryNetwork;
use palaver_proto::Secret;
use palaver_sdk::{Options, Participant, Pictures};
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test(start_paused = true)]
async fn a_bots_pattern_is_sent_once_somebody_watches_it() {
    let network = MemoryNetwork::new();
    let secret = Secret::new([7; 32]);
    let mut bot = Bot::on_memory(&network, &secret, "bot 1", Script::silent(1).filmed()).unwrap();
    bot.participant().create_meeting().await;
    let (mut viewer, _call) =
        Participant::on_memory(&network, &secret, Options::named("viewer")).unwrap();
    viewer.join(vec![bot.participant().address().clone()]).await;
    let (port, mut status) = viewer.video().unwrap();
    let listed = status.wait_for(|status| status.videos.len() == 3);
    assert!(timeout(Duration::from_secs(30), listed).await.is_ok());
    let mut pictures: Pictures<SoftwareEncoder, SoftwareDecoder> = Pictures::new(port, status);
    // The bot offers its picture whole, halved and quartered; the smallest will do.
    let tracks = pictures.tracks();
    let high: Vec<u32> = tracks.iter().map(|t| t.shape.unwrap().height).collect();
    assert_eq!(high, [PICTURE.1, PICTURE.1 / 2, PICTURE.1 / 4]);
    let alias = tracks.last().unwrap().alias;

    // Half a second with nobody watching: the bot makes nothing.
    for _ in 0..25 {
        bot.tick().unwrap();
        tokio::time::sleep(FRAME).await;
    }
    assert_eq!(bot.filmed(), 0);

    pictures.watch(alias, SoftwareDecoder::new().unwrap());
    let mut shown = Vec::new();
    for _ in 0..25 {
        bot.tick().unwrap();
        tokio::time::sleep(FRAME).await;
        pictures.turn(|_, picture| shown.push(picture.size()));
    }
    assert_eq!(bot.camera_trouble(), None);
    assert!(bot.filmed() >= 5, "only {} pictures sent", bot.filmed());
    assert!(shown.len() >= 5, "only {} pictures shown", shown.len());
    assert!(
        shown
            .iter()
            .all(|size| (size.width() * 4, size.height() * 4) == PICTURE)
    );
}
