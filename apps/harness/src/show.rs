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

use crate::picture::Slot;
use palaver_media::picture::Picture;
use palaver_media::video::{
    Decode, Encode, Error, Kind, Settings, SoftwareDecoder, SoftwareEncoder,
};
use palaver_sdk::{Pictures, Status, VideoPort, VideoTrack};
use std::collections::VecDeque;
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
    pub sent: Count,
    /// Frames decoded to show.
    pub shown: Count,
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
    meter: Arc<Meter>,
}

impl<E: Encode> Encode for Timed<E> {
    fn encode(&mut self, picture: &Picture, out: &mut Vec<u8>) -> Result<Option<Kind>, Error> {
        let began = std::time::Instant::now();
        let kind = self.codec.encode(picture, out)?;
        if kind.is_some() {
            self.meter.sent.add(out.len(), began.elapsed());
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
        self.meter.shown.add(coded.len(), began.elapsed());
        Ok(picture)
    }
}

/// What the window gives a session for its pictures.
#[derive(Debug, Clone)]
pub struct Screen {
    /// Where this machine's own pictures are put, if it makes any: they are sent to
    /// whoever watches.
    pub camera: Option<Arc<Slot>>,
    /// What the meeting calls this machine's picture, and the rate to code it at, in
    /// bits a second.
    pub rendition: Vec<u8>,
    pub bitrate: u32,
    /// Where the picture of someone else is put.
    pub far: Arc<Slot>,
    pub meter: Arc<Meter>,
}

impl Screen {
    /// A screen for a camera whose pictures are `tall` high, or for none.
    #[must_use]
    pub fn new(camera: Option<Arc<Slot>>, tall: u32, bitrate: u32) -> Self {
        Self {
            camera,
            rendition: format!("{tall}p").into_bytes(),
            bitrate,
            far: Arc::new(Slot::default()),
            meter: Arc::new(Meter::default()),
        }
    }
}

/// Whose picture is shown, of the video tracks in the plan: the first that is not this
/// machine's own. (A picture for everyone is to come.)
#[must_use]
pub fn far_of(tracks: &[VideoTrack]) -> Option<&VideoTrack> {
    tracks.iter().find(|track| !track.own)
}

/// The work between the node's pictures and the window's slots, a turn at a time.
pub struct Projector {
    pictures: Pictures<Timed<SoftwareEncoder>, Timed<SoftwareDecoder>>,
    screen: Screen,
    /// The count of the last of this machine's pictures that was taken.
    seen: u64,
    /// The track being watched.
    far: Option<u64>,
}

impl Projector {
    #[must_use]
    pub fn new(port: VideoPort, status: watch::Receiver<Status>, screen: Screen) -> Self {
        Self {
            pictures: Pictures::new(port, status),
            // What was made before the meeting is not for it.
            seen: screen.camera.as_ref().map_or(0, |slot| slot.count()),
            screen,
            far: None,
        }
    }

    /// One turn: watch whoever is to be watched, show what has come, and send this
    /// machine's newest picture if there is a new one and somebody watches.
    pub fn turn(&mut self) {
        let meter = &self.screen.meter;
        let want = far_of(&self.pictures.tracks()).map(|track| track.alias);
        if want != self.far {
            if let Some(old) = self.far.take() {
                self.pictures.unwatch(old);
            }
            if let Some(new) = want {
                match SoftwareDecoder::new() {
                    Ok(codec) => {
                        let meter = Arc::clone(meter);
                        self.pictures.watch(new, Timed { codec, meter });
                    }
                    Err(error) => meter.fail(&error),
                }
            }
            self.far = want;
        }
        let far = &self.screen.far;
        self.pictures.turn(|_, picture| far.put(picture));

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
                ..Settings::new(picture.size(), self.screen.bitrate)
            };
            match SoftwareEncoder::new(settings) {
                Ok(codec) => {
                    let meter = Arc::clone(meter);
                    self.pictures.set_camera(Timed { codec, meter });
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
