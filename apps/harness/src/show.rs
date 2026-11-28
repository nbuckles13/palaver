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
use palaver_sdk::{Offer, Pictures, Shape, Status, VideoPort, VideoTrack};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::watch;

/// How long the thread rests between two turns: what it adds, at most, to how late a
/// picture is shown.
const REST: Duration = Duration::from_millis(5);
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
    /// Frames coded to send.
    pub sent: Arc<Count>,
    failed: Mutex<Option<String>>,
}

impl Meter {
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

/// How pictures have flowed one way over the last second or so.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flow {
    /// Bits a second.
    pub bitrate: u64,
    /// Frames a second.
    pub per_second: u64,
    /// How long each frame took to code, on average.
    pub each: Duration,
}

/// Turns counts read from time to time into a [`Flow`] over the last few readings.
#[derive(Debug, Clone)]
pub struct Flowing {
    past: VecDeque<(Duration, Counted)>,
    kept: usize,
}

impl Flowing {
    /// Over the last `kept` readings.
    #[must_use]
    pub const fn over(kept: usize) -> Self {
        Self {
            past: VecDeque::new(),
            kept,
        }
    }

    /// Take the count as it stands `at` this time since some beginning.
    pub fn after(&mut self, at: Duration, now: Counted) -> Flow {
        self.past.push_back((at, now));
        while self.past.len() > self.kept.max(1).saturating_add(1) {
            self.past.pop_front();
        }
        let Some((from, then)) = self.past.front().copied() else {
            return Flow::default();
        };
        let millis = u64::try_from(at.saturating_sub(from).as_millis()).unwrap_or(u64::MAX);
        let frames = now.frames.saturating_sub(then.frames);
        if millis == 0 || frames == 0 {
            return Flow::default();
        }
        let bytes = now.bytes.saturating_sub(then.bytes);
        let spent = now.spent.saturating_sub(then.spent);
        Flow {
            bitrate: bytes.saturating_mul(8000) / millis,
            per_second: (frames.saturating_mul(1000).saturating_add(millis / 2)) / millis,
            each: spent / u32::try_from(frames).unwrap_or(u32::MAX),
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
}

type Wake = Arc<dyn Fn() + Send + Sync>;

/// The pictures of the others in a meeting, each under whose it is. Whoever decodes
/// them and whoever draws them hold the same wall.
#[derive(Default)]
pub struct Wall {
    seats: Mutex<BTreeMap<ParticipantId, Arc<Seat>>>,
    wake: Mutex<Option<Wake>>,
}

impl std::fmt::Debug for Wall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wall").finish_non_exhaustive()
    }
}

impl Wall {
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
    /// What this machine offers the meeting of its camera, if it has one.
    pub offer: Offer,
    /// Where the pictures of the others are put.
    pub wall: Arc<Wall>,
    pub meter: Arc<Meter>,
}

impl Screen {
    /// A screen for a camera whose pictures are `size` large (across and down), coded
    /// at `bitrate`, or for none.
    #[must_use]
    pub fn new(camera: Option<Arc<Slot>>, size: (u32, u32), bitrate: u32) -> Self {
        Self {
            camera,
            offer: Offer {
                name: format!("{}p", size.1).into_bytes(),
                shape: Shape {
                    width: size.0,
                    height: size.1,
                    per_second: PER_SECOND,
                    bitrate,
                },
            },
            wall: Arc::new(Wall::default()),
            meter: Arc::new(Meter::default()),
        }
    }
}

/// Whose pictures are shown, of the video tracks in the plan.
///
/// The first track of each participant but this machine's own, in the plan's order.
/// (Choosing among the sizes one participant offers is to come.)
#[must_use]
pub fn others(tracks: &[VideoTrack]) -> Vec<&VideoTrack> {
    let mut found: Vec<&VideoTrack> = Vec::new();
    for track in tracks.iter().filter(|track| !track.own) {
        if found.iter().all(|had| had.participant != track.participant) {
            found.push(track);
        }
    }
    found
}

/// The work between the node's pictures and the window's slots, a turn at a time.
pub struct Projector {
    pictures: Pictures<Timed<SoftwareEncoder>, Timed<SoftwareDecoder>>,
    screen: Screen,
    /// The count of the last of this machine's pictures that was taken.
    seen: u64,
    /// The plan's video tracks as they were last looked at.
    tracks: Vec<VideoTrack>,
    /// The tracks being watched, and where the pictures of each go.
    watching: BTreeMap<u64, Arc<Seat>>,
}

impl Projector {
    #[must_use]
    pub fn new(port: VideoPort, status: watch::Receiver<Status>, screen: Screen) -> Self {
        Self {
            pictures: Pictures::new(port, status),
            // What was made before the meeting is not for it.
            seen: screen.camera.as_ref().map_or(0, |slot| slot.count()),
            screen,
            tracks: Vec::new(),
            watching: BTreeMap::new(),
        }
    }

    /// Watch everyone the plan now has a picture of, and nobody it no longer has.
    fn watch_others(&mut self) {
        let tracks = self.pictures.tracks();
        if tracks == self.tracks {
            return;
        }
        let wanted = others(&tracks);
        let gone: Vec<u64> = self
            .watching
            .keys()
            .copied()
            .filter(|alias| wanted.iter().all(|track| track.alias != *alias))
            .collect();
        for alias in gone {
            self.pictures.unwatch(alias);
            self.watching.remove(&alias);
        }
        for track in &wanted {
            if self.watching.contains_key(&track.alias) {
                continue;
            }
            let seat = self.screen.wall.of(&track.participant);
            match SoftwareDecoder::new() {
                Ok(codec) => {
                    let count = Arc::clone(&seat.count);
                    self.pictures.watch(track.alias, Timed { codec, count });
                }
                Err(error) => self.screen.meter.fail(&error),
            }
            self.watching.insert(track.alias, seat);
        }
        let who: Vec<&ParticipantId> = wanted.iter().map(|track| &track.participant).collect();
        self.screen.wall.keep(&who);
        self.tracks = tracks;
    }

    /// One turn: watch whoever is to be watched, show what has come, and send this
    /// machine's newest picture if there is a new one and somebody watches.
    pub fn turn(&mut self) {
        self.watch_others();
        let watching = &self.watching;
        self.pictures.turn(|alias, picture| {
            if let Some(seat) = watching.get(&alias) {
                seat.slot.put(picture);
            }
        });
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
        if self.pictures.wanted() && !self.pictures.has_camera() {
            let settings = Settings {
                key_every: Some(KEY_EVERY),
                ..Settings::new(picture.size(), self.screen.offer.shape.bitrate)
            };
            match SoftwareEncoder::new(settings) {
                Ok(codec) => {
                    let count = Arc::clone(&meter.sent);
                    self.pictures.set_camera(Timed { codec, count });
                }
                Err(error) => {
                    meter.fail(&error);
                    self.screen.camera = None;
                    return;
                }
            }
        }
        if let Err(error) = self.pictures.film(&picture) {
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
            while !stopping.load(Ordering::Relaxed) {
                projector.turn();
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

    #[test]
    fn a_flow_is_what_was_counted_over_the_last_readings() {
        let mut flowing = Flowing::over(5);
        // One reading is no stretch of time to measure over.
        assert_eq!(
            flowing.after(Duration::ZERO, Counted::default()),
            Flow::default()
        );
        // Thirty frames and 60 000 bytes in a second, 90 ms spent on them.
        let flow = flowing.after(Duration::from_secs(1), counted(30, 60_000, 90));
        assert_eq!(
            flow,
            Flow {
                bitrate: 480_000,
                per_second: 30,
                each: Duration::from_millis(3),
            }
        );
        // Nothing more made: once the busy second is forgotten, nothing flows.
        for second in 2..=6 {
            flowing.after(Duration::from_secs(second), counted(30, 60_000, 90));
        }
        assert_eq!(
            flowing.after(Duration::from_secs(7), counted(30, 60_000, 90)),
            Flow::default()
        );
    }

    #[test]
    fn a_count_is_read_as_it_stands() {
        let count = Count::default();
        count.add(100, Duration::from_millis(2));
        count.add(50, Duration::from_millis(1));
        assert_eq!(count.read(), counted(2, 150, 3));
    }
}
