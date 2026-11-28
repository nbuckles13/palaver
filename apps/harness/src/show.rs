//! The pictures of a meeting, between the node and the window.
//!
//! The window gives the session a [`Screen`]: the slot its own pictures are put in (a
//! camera's or a pattern's), and a slot for the picture of someone else. While a meeting
//! lasts, a [`Show`] runs a thread between those and the node: what is in the first is
//! coded and sent while somebody watches, and what the node is given of the track it
//! watches is decoded into the second.
//!
//! Coding is on a thread of its own because it takes milliseconds, which neither the
//! network nor the window has to spare. The work of each turn is in [`Projector`], which
//! never waits, so that tests can turn it by hand.

use crate::picture::{PER_SECOND, Slot};
use palaver_media::picture::Picture;
use palaver_media::video::{
    Decode, Encode, Error, Kind, Settings, SoftwareDecoder, SoftwareEncoder,
};
use palaver_proto::ParticipantId;
use palaver_sdk::{Offer, Pictures, Status, VideoPort, VideoTrack};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::watch;

/// How long the thread rests between two turns: what it adds, at most, to how late a
/// picture is shown.
const REST: Duration = Duration::from_millis(5);
/// The rate a picture is coded at when its track does not say, in bits a second.
const BITRATE: u32 = 500_000;
/// A key frame by itself this often, in pictures: ten seconds. Whoever needs one sooner
/// asks.
const KEY_EVERY: u32 = 300;

/// What coding has come to so far, one way.
#[derive(Debug, Default)]
pub struct Count {
    frames: AtomicU64,
    bytes: AtomicU64,
    nanos: AtomicU64,
}

/// A [`Count`] as it stood at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counted {
    pub frames: u64,
    pub bytes: u64,
    pub spent: Duration,
}

impl Count {
    fn add(&self, bytes: usize, spent: Duration) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        let nanos = u64::try_from(spent.as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    #[must_use]
    pub fn read(&self) -> Counted {
        Counted {
            frames: self.frames.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            spent: Duration::from_nanos(self.nanos.load(Ordering::Relaxed)),
        }
    }
}

/// What a meeting's pictures cost, for whoever shows it.
#[derive(Debug, Default)]
pub struct Meter {
    /// Frames coded to send, for each size of the camera, by what the size is called.
    sent: Mutex<BTreeMap<Vec<u8>, Arc<Count>>>,
    failed: Mutex<Option<String>>,
}

impl Meter {
    /// Where what is coded of the size called `name` is counted.
    #[must_use]
    pub fn sent_of(&self, name: &[u8]) -> Arc<Count> {
        let Ok(mut sent) = self.sent.lock() else {
            return Arc::default();
        };
        Arc::clone(sent.entry(name.to_vec()).or_default())
    }

    /// What has been coded of each size so far, by what the size is called.
    #[must_use]
    pub fn sent_by_size(&self) -> Vec<(Vec<u8>, Counted)> {
        self.sent.lock().map_or_else(
            |_| Vec::new(),
            |sent| {
                sent.iter()
                    .map(|(name, count)| (name.clone(), count.read()))
                    .collect()
            },
        )
    }

    /// What has been coded of every size together.
    #[must_use]
    pub fn sent_total(&self) -> Counted {
        self.sent_by_size()
            .into_iter()
            .fold(Counted::default(), |sum, (_, each)| Counted {
                frames: sum.frames.saturating_add(each.frames),
                bytes: sum.bytes.saturating_add(each.bytes),
                spent: sum.spent.saturating_add(each.spent),
            })
    }

    /// Why the pictures stopped, one way or both, if they did.
    #[must_use]
    pub fn failed(&self) -> Option<String> {
        self.failed.lock().ok()?.clone()
    }

    fn fail(&self, why: &Error) {
        if let Ok(mut failed) = self.failed.lock() {
            *failed = Some(why.to_string());
        }
    }
}

/// A codec that counts what it does.
pub struct Timed<C> {
    codec: C,
    count: Arc<Count>,
}

impl<E: Encode> Encode for Timed<E> {
    fn encode(&mut self, picture: &Picture, out: &mut Vec<u8>) -> Result<Option<Kind>, Error> {
        let began = std::time::Instant::now();
        let kind = self.codec.encode(picture, out)?;
        if kind.is_some() {
            self.count.add(out.len(), began.elapsed());
        }
        Ok(kind)
    }

    fn ask_for_key(&mut self) {
        self.codec.ask_for_key();
    }
}

impl<D: Decode> Decode for Timed<D> {
    fn decode(&mut self, coded: &[u8]) -> Result<Option<Picture>, Error> {
        let began = std::time::Instant::now();
        let picture = self.codec.decode(coded)?;
        self.count.add(coded.len(), began.elapsed());
        Ok(picture)
    }
}

/// One of the others' pictures: where it is put, and what decoding it has come to.
#[derive(Debug, Default)]
pub struct Seat {
    pub slot: Arc<Slot>,
    pub count: Arc<Count>,
    /// How large the newest picture is: its width above its height.
    size: AtomicU64,
    /// How many key frames have been asked for.
    keys: AtomicU64,
}

impl Seat {
    /// Put a picture in the seat's slot, and remember how large it is.
    pub fn put(&self, picture: Picture) {
        let size = picture.size();
        let both = u64::from(size.width()) << 32 | u64::from(size.height());
        self.size.store(both, Ordering::Relaxed);
        self.slot.put(picture);
    }

    /// How large the newest picture is, across and down, if there has been one.
    #[must_use]
    pub fn size(&self) -> Option<(u32, u32)> {
        let both = self.size.load(Ordering::Relaxed);
        let across = u32::try_from(both >> 32).ok()?;
        let down = u32::try_from(both & u64::from(u32::MAX)).ok()?;
        (across > 0).then_some((across, down))
    }

    /// How many key frames have been asked for of whoever sits here.
    #[must_use]
    pub fn keys(&self) -> u64 {
        self.keys.load(Ordering::Relaxed)
    }
}

type Wake = Arc<dyn Fn() + Send + Sync>;

/// The pictures of the others in a meeting, each under whose it is. Whoever decodes
/// them and whoever draws them hold the same wall.
#[derive(Default)]
pub struct Wall {
    /// How high each picture is drawn, in pixels of the screen.
    tall: AtomicU32,
    seats: Mutex<BTreeMap<ParticipantId, Arc<Seat>>>,
    wake: Mutex<Option<Wake>>,
}

impl std::fmt::Debug for Wall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wall").finish_non_exhaustive()
    }
}

impl Wall {
    /// How high each picture is drawn, in pixels of the screen: what whoever decodes
    /// chooses a size by. Nought until whoever draws has said.
    #[must_use]
    pub fn tall(&self) -> u32 {
        self.tall.load(Ordering::Relaxed)
    }

    pub fn set_tall(&self, tall: u32) {
        self.tall.store(tall, Ordering::Relaxed);
    }

    /// Have `wake` called each time a picture is put in any seat.
    pub fn on_new(&self, wake: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut held) = self.wake.lock() {
            *held = Some(Arc::new(wake));
        }
    }

    /// The seat for `who`'s picture, if there is one.
    #[must_use]
    pub fn get(&self, who: &ParticipantId) -> Option<Arc<Seat>> {
        self.seats.lock().ok()?.get(who).cloned()
    }

    /// The seat for `who`'s picture, made now if there was none.
    #[must_use]
    pub fn of(&self, who: &ParticipantId) -> Arc<Seat> {
        if let Some(seat) = self.get(who) {
            return seat;
        }
        let seat = Arc::new(Seat::default());
        if let Ok(wake) = self.wake.lock()
            && let Some(wake) = wake.clone()
        {
            seat.slot.on_new(move || wake());
        }
        if let Ok(mut seats) = self.seats.lock() {
            seats.insert(who.clone(), Arc::clone(&seat));
        }
        seat
    }

    /// Forget every seat but those of `who`.
    pub fn keep(&self, who: &[&ParticipantId]) {
        if let Ok(mut seats) = self.seats.lock() {
            seats.retain(|id, _| who.contains(&id));
        }
    }
}

/// What the window gives a session for its pictures.
#[derive(Debug, Clone)]
pub struct Screen {
    /// Where this machine's own pictures are put, if it makes any: they are sent to
    /// whoever watches.
    pub camera: Option<Arc<Slot>>,
    /// The sizes of its camera this machine offers the meeting, if it has one.
    pub offers: Vec<Offer>,
    /// Where the pictures of the others are put.
    pub wall: Arc<Wall>,
    pub meter: Arc<Meter>,
}

impl Screen {
    /// A screen for a camera whose pictures are `size` large (across and down), or for
    /// none. The camera is offered at that size, halved and quartered.
    #[must_use]
    pub fn new(camera: Option<Arc<Slot>>, size: (u32, u32)) -> Self {
        Self {
            camera,
            offers: Offer::sizes_of(size.0, size.1, PER_SECOND),
            wall: Arc::new(Wall::default()),
            meter: Arc::new(Meter::default()),
        }
    }
}

/// Everyone else with a picture, of the video tracks in the plan, each with the sizes
/// they offer, in the plan's order.
#[must_use]
pub fn others(tracks: &[VideoTrack]) -> Vec<(&ParticipantId, Vec<&VideoTrack>)> {
    let mut found: Vec<(&ParticipantId, Vec<&VideoTrack>)> = Vec::new();
    for track in tracks.iter().filter(|track| !track.own) {
        match found.iter_mut().find(|(who, _)| *who == &track.participant) {
            Some((_, sizes)) => sizes.push(track),
            None => found.push((&track.participant, vec![track])),
        }
    }
    found
}

/// Which of the sizes someone offers to watch for a picture drawn `tall` pixels high.
///
/// The smallest that is at least as high; the largest there is, if none is. A track
/// that does not say what it is is taken only when none does.
#[must_use]
pub fn pick<'a>(sizes: &[&'a VideoTrack], tall: u32) -> Option<&'a VideoTrack> {
    let high = |track: &VideoTrack| track.shape.map(|shape| shape.height);
    let said = || sizes.iter().copied().filter(|track| track.shape.is_some());
    said()
        .filter(|track| high(track) >= Some(tall))
        .min_by_key(|track| high(track))
        .or_else(|| said().max_by_key(|track| high(track)))
        .or_else(|| sizes.first().copied())
}

/// How long another size must have been the one to watch before it is changed to:
/// a window being dragged larger passes through many sizes.
const HOLD: Duration = Duration::from_millis(500);

/// Someone whose picture is watched.
struct Watched {
    seat: Arc<Seat>,
    /// The track whose pictures are shown.
    showing: Option<u64>,
    /// A track asked for in its place, which has shown nothing yet. The one before is
    /// kept until it does, so that the picture does not stop for the change.
    coming: Option<u64>,
    /// The size that would be better, and since when it has been.
    wish: Option<(u64, Duration)>,
}

/// The work between the node's pictures and the window's slots, a turn at a time.
pub struct Projector {
    pictures: Pictures<Timed<SoftwareEncoder>, Timed<SoftwareDecoder>>,
    screen: Screen,
    /// The count of the last of this machine's pictures that was taken.
    seen: u64,
    watching: BTreeMap<ParticipantId, Watched>,
}

impl Projector {
    #[must_use]
    pub fn new(port: VideoPort, status: watch::Receiver<Status>, screen: Screen) -> Self {
        Self {
            pictures: Pictures::new(port, status),
            // What was made before the meeting is not for it.
            seen: screen.camera.as_ref().map_or(0, |slot| slot.count()),
            screen,
            watching: BTreeMap::new(),
        }
    }

    /// Begin to watch `alias`, whose pictures go to `seat`.
    fn watch(&mut self, alias: u64, seat: &Seat) {
        match SoftwareDecoder::new() {
            Ok(codec) => {
                let count = Arc::clone(&seat.count);
                self.pictures.watch(alias, Timed { codec, count });
            }
            Err(error) => self.screen.meter.fail(&error),
        }
    }

    /// Watch everyone the plan has a picture of, each at the size that suits how
    /// large it is drawn at `at`, and nobody the plan no longer has.
    fn watch_others(&mut self, at: Duration) {
        let tracks = self.pictures.tracks();
        let others = others(&tracks);
        let gone: Vec<ParticipantId> = self
            .watching
            .keys()
            .filter(|who| others.iter().all(|(other, _)| other != who))
            .cloned()
            .collect();
        for who in gone {
            if let Some(watched) = self.watching.remove(&who) {
                for alias in watched.showing.into_iter().chain(watched.coming) {
                    self.pictures.unwatch(alias);
                }
            }
        }
        let tall = self.screen.wall.tall();
        for (who, sizes) in &others {
            let Some(choice) = pick(sizes, tall).map(|track| track.alias) else {
                continue;
            };
            let offered = |alias: &u64| sizes.iter().any(|track| track.alias == *alias);
            self.settle(who, choice, offered, at);
        }
        let who: Vec<&ParticipantId> = others.iter().map(|(who, _)| *who).collect();
        self.screen.wall.keep(&who);
    }

    /// Bring what is watched of `who` towards `choice`, the size that suits now.
    fn settle(
        &mut self,
        who: &ParticipantId,
        choice: u64,
        offered: impl Fn(&u64) -> bool,
        at: Duration,
    ) {
        let seat = self.screen.wall.of(who);
        let watched = self.watching.entry(who.clone()).or_insert_with(|| Watched {
            seat: Arc::clone(&seat),
            showing: None,
            coming: None,
            wish: None,
        });
        // A size that was withdrawn is watched no more.
        let mut dropped: Vec<u64> = Vec::new();
        for held in [&mut watched.showing, &mut watched.coming] {
            dropped.extend(held.take_if(|alias| !offered(alias)));
        }
        let heading = watched.coming.or(watched.showing);
        let change = if heading.is_none() {
            // Nothing is shown: there is nothing to keep steady.
            true
        } else if heading == Some(choice) {
            watched.wish = None;
            false
        } else {
            let since = match watched.wish {
                Some((wished, since)) if wished == choice => since,
                _ => at,
            };
            watched.wish = Some((choice, since));
            at.saturating_sub(since) >= HOLD
        };
        let mut begun = None;
        if change {
            watched.wish = None;
            dropped.extend(watched.coming.take());
            if watched.showing != Some(choice) {
                watched.coming = Some(choice);
                begun = Some(choice);
            }
        }
        for alias in dropped {
            self.pictures.unwatch(alias);
        }
        if let Some(alias) = begun {
            self.watch(alias, &seat);
        }
    }

    /// Show what has come. The first picture of a size that was asked for in place of
    /// another takes its place, and the other is watched no more.
    fn show(&mut self) {
        let watching = &mut self.watching;
        let mut replaced: Vec<u64> = Vec::new();
        self.pictures.turn(|alias, picture| {
            for watched in watching.values_mut() {
                if watched.coming == Some(alias) {
                    replaced.extend(watched.showing.replace(alias));
                    watched.coming = None;
                }
                if watched.showing == Some(alias) {
                    watched.seat.put(picture);
                    return;
                }
            }
        });
        for alias in replaced {
            self.pictures.unwatch(alias);
        }
        for alias in self.pictures.take_asked() {
            let asked = self
                .watching
                .values()
                .find(|watched| [watched.showing, watched.coming].contains(&Some(alias)));
            if let Some(watched) = asked {
                watched.seat.keys.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The track of each participant whose pictures are shown now.
    #[must_use]
    pub fn showing(&self) -> BTreeMap<ParticipantId, u64> {
        self.watching
            .iter()
            .filter_map(|(who, watched)| Some((who.clone(), watched.showing?)))
            .collect()
    }

    /// One turn: watch whoever is to be watched, show what has come, and send this
    /// machine's newest picture if there is a new one and somebody watches. `at` is
    /// the time since some beginning.
    pub fn turn(&mut self, at: Duration) {
        self.watch_others(at);
        self.show();
        let meter = &self.screen.meter;

        let Some((seen, picture)) = self
            .screen
            .camera
            .as_ref()
            .and_then(|slot| slot.newer_than(self.seen))
        else {
            return;
        };
        self.seen = seen;
        let filmed = self.pictures.film_all(&picture, |track, size| {
            let bitrate = track.shape.map_or(BITRATE, |shape| shape.bitrate);
            let settings = Settings {
                key_every: Some(KEY_EVERY),
                ..Settings::new(size, bitrate)
            };
            let codec = SoftwareEncoder::new(settings)?;
            let count = meter.sent_of(&track.rendition);
            Ok(Timed { codec, count })
        });
        if let Err(error) = filmed {
            meter.fail(&error);
            self.screen.camera = None;
        }
    }
}

/// A [`Projector`] turning on a thread of its own, until dropped.
pub struct Show {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Show {
    #[must_use]
    pub fn start(port: VideoPort, status: watch::Receiver<Status>, screen: Screen) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            // The codecs are made on the thread that uses them.
            let mut projector = Projector::new(port, status, screen);
            let began = std::time::Instant::now();
            while !stopping.load(Ordering::Relaxed) {
                projector.turn(began.elapsed());
                std::thread::park_timeout(REST);
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Show {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counted(frames: u64, bytes: u64, millis: u64) -> Counted {
        Counted {
            frames,
            bytes,
            spent: Duration::from_millis(millis),
        }
    }

    fn track(alias: u64, who: u8, tall: Option<u32>, own: bool) -> VideoTrack {
        VideoTrack {
            alias,
            participant: ParticipantId::from_key_hash([who; 16]),
            source: palaver_proto::Source::Main,
            rendition: alias.to_string().into_bytes(),
            shape: tall.map(|height| palaver_sdk::Shape {
                width: height * 16 / 9,
                height,
                per_second: 30,
                bitrate: 100_000,
            }),
            own,
        }
    }

    #[test]
    fn the_size_watched_is_the_smallest_that_covers_the_picture_as_drawn() {
        let offered = [
            track(1, 1, Some(720), false),
            track(2, 1, Some(180), false),
            track(3, 1, Some(360), false),
        ];
        let sizes: Vec<&VideoTrack> = offered.iter().collect();
        let picked = |tall| pick(&sizes, tall).map(|track| track.alias);
        assert_eq!(picked(0), Some(2));
        assert_eq!(picked(180), Some(2));
        assert_eq!(picked(181), Some(3));
        assert_eq!(picked(400), Some(1));
        // Drawn larger than anything on offer: the largest there is.
        assert_eq!(picked(2000), Some(1));
        // A track that does not say what it is is taken only when none does.
        let (unsaid, small) = (track(7, 1, None, false), track(8, 1, Some(90), false));
        assert_eq!(pick(&[&unsaid, &small], 500).map(|t| t.alias), Some(8));
        assert_eq!(pick(&[&unsaid], 500).map(|t| t.alias), Some(7));
        assert_eq!(pick(&[], 500), None);
    }

    #[test]
    fn the_others_are_each_listed_once_with_every_size_they_offer() {
        let tracks = [
            track(1, 1, Some(720), false),
            track(2, 9, Some(720), true),
            track(3, 2, Some(360), false),
            track(4, 1, Some(180), false),
        ];
        let listed: Vec<(u8, Vec<u64>)> = others(&tracks)
            .into_iter()
            .map(|(who, sizes)| {
                let aliases = sizes.iter().map(|track| track.alias).collect();
                (who.as_bytes()[0], aliases)
            })
            .collect();
        assert_eq!(listed, [(1, vec![1, 4]), (2, vec![3])]);
    }

    #[test]
    fn a_count_is_read_as_it_stands() {
        let count = Count::default();
        count.add(100, Duration::from_millis(2));
        count.add(50, Duration::from_millis(1));
        assert_eq!(count.read(), counted(2, 150, 3));
    }
}
