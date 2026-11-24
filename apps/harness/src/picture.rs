//! Pictures on their way to the window.
//!
//! A [`Slot`] holds the newest picture of one tile. Whoever makes pictures (a camera, a
//! decoder, a pattern) puts each in; the window takes the newest when it draws, and is
//! woken when there is one. Nothing waits on anything: a picture not taken before the
//! next arrives is dropped, which is right for something to look at.
//!
//! [`Feed`] makes a moving pattern on a thread of its own, in place of a camera; and can
//! put each picture through the encoder and the decoder as well, to show what someone
//! at the far end would see and what that costs.

use palaver_media::picture::{Pattern, Picture, Size};
use palaver_media::video::{Decode, Encode, Settings, SoftwareDecoder, SoftwareEncoder};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How many pictures a second a feed makes.
pub const PER_SECOND: u32 = 30;

type Wake = Box<dyn Fn() + Send>;

/// The newest picture of one tile.
#[derive(Default)]
pub struct Slot {
    newest: Mutex<Option<Arc<Picture>>>,
    /// How many pictures have been put in.
    count: AtomicU64,
    wake: Mutex<Option<Wake>>,
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot")
            .field("count", &self.count())
            .finish_non_exhaustive()
    }
}

impl Slot {
    /// Put a picture in, in place of the one before, and wake whoever asked to be.
    pub fn put(&self, picture: Picture) {
        if let Ok(mut newest) = self.newest.lock() {
            *newest = Some(Arc::new(picture));
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        if let Ok(wake) = self.wake.lock()
            && let Some(wake) = wake.as_ref()
        {
            wake();
        }
    }

    /// The newest picture, if it is newer than the one counted `seen`; with its count,
    /// to be given back the next time.
    #[must_use]
    pub fn newer_than(&self, seen: u64) -> Option<(u64, Arc<Picture>)> {
        let count = self.count.load(Ordering::Relaxed);
        if count == seen {
            return None;
        }
        let newest = self.newest.lock().ok()?.clone()?;
        Some((count, newest))
    }

    /// How many pictures have been put in.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Have `wake` called each time a picture is put in.
    pub fn on_new(&self, wake: impl Fn() + Send + 'static) {
        if let Ok(mut held) = self.wake.lock() {
            *held = Some(Box::new(wake));
        }
    }
}

/// What coding a feed's pictures costs, over the last second of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cost {
    /// What was made, in bits a second.
    pub bitrate: u64,
    /// How long a picture took to encode, and a frame to decode, on average.
    pub encode: Duration,
    pub decode: Duration,
    /// Pictures the encoder let go to hold its rate.
    pub let_go: u32,
    /// Why coding stopped, if it did.
    pub failed: Option<String>,
}

/// Where a feed says what its coding costs, for whoever shows it.
#[derive(Debug, Default)]
pub struct Coding(Mutex<Cost>);

impl Coding {
    #[must_use]
    pub fn cost(&self) -> Cost {
        self.0.lock().map(|cost| cost.clone()).unwrap_or_default()
    }

    fn set(&self, cost: Cost) {
        if let Ok(mut held) = self.0.lock() {
            *held = cost;
        }
    }
}

/// One second's sums, on the way to a [`Cost`].
#[derive(Default)]
struct Tally {
    pictures: u32,
    bytes: u64,
    encode: Duration,
    decode: Duration,
    let_go: u32,
}

impl Tally {
    fn cost(&self) -> Cost {
        let each = |sum: Duration| sum / self.pictures.max(1);
        Cost {
            bitrate: self.bytes.saturating_mul(8),
            encode: each(self.encode),
            decode: each(self.decode),
            let_go: self.let_go,
            failed: None,
        }
    }
}

/// A moving pattern put into a slot thirty times a second, until dropped.
pub struct Feed {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Feed {
    /// Start making `speaker`'s pattern, `size` large, into `slot`.
    #[must_use]
    pub fn pattern(slot: Arc<Slot>, size: Size, speaker: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut pattern = Pattern::new(size, speaker);
            let every = Duration::from_secs(1) / PER_SECOND;
            let mut due = Instant::now();
            while !stopping.load(Ordering::Relaxed) {
                slot.put(pattern.next_picture());
                due += every;
                std::thread::park_timeout(due.saturating_duration_since(Instant::now()));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Feed {
    /// As [`Feed::pattern`], and each picture is also encoded and decoded again, as it
    /// would be for someone at the far end, and what comes back put into `back`. What
    /// that costs is said in `coding` once a second.
    #[must_use]
    pub fn coded(
        raw: Arc<Slot>,
        back: Arc<Slot>,
        coding: Arc<Coding>,
        settings: Settings,
        speaker: u32,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            // The codec is made on the thread that uses it.
            let both = SoftwareEncoder::new(settings)
                .and_then(|encoder| SoftwareDecoder::new().map(|decoder| (encoder, decoder)));
            let (mut encoder, mut decoder) = match both {
                Ok(both) => both,
                Err(error) => {
                    return coding.set(Cost {
                        failed: Some(error.to_string()),
                        ..Cost::default()
                    });
                }
            };
            let mut pattern = Pattern::new(settings.size, speaker);
            let every = Duration::from_secs(1) / settings.per_second.max(1);
            let mut due = Instant::now();
            let mut tally = Tally::default();
            let mut bytes = Vec::new();
            while !stopping.load(Ordering::Relaxed) {
                let picture = pattern.next_picture();
                let began = Instant::now();
                let kind = encoder.encode(&picture, &mut bytes);
                tally.encode += began.elapsed();
                raw.put(picture);
                tally.pictures += 1;
                let back_again = match kind {
                    Ok(Some(_)) => {
                        tally.bytes += bytes.len() as u64;
                        let began = Instant::now();
                        let picture = decoder.decode(&bytes);
                        tally.decode += began.elapsed();
                        picture
                    }
                    Ok(None) => {
                        tally.let_go += 1;
                        Ok(None)
                    }
                    Err(error) => Err(error),
                };
                match back_again {
                    Ok(Some(picture)) => back.put(picture),
                    Ok(None) => {}
                    Err(error) => {
                        return coding.set(Cost {
                            failed: Some(error.to_string()),
                            ..Cost::default()
                        });
                    }
                }
                if tally.pictures >= settings.per_second.max(1) {
                    coding.set(std::mem::take(&mut tally).cost());
                }
                due += every;
                std::thread::park_timeout(due.saturating_duration_since(Instant::now()));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Feed {
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

    fn size() -> Size {
        Size::new(16, 8).unwrap_or_else(|| unreachable!())
    }

    #[test]
    fn a_slot_gives_only_what_is_newer_and_drops_what_was_not_taken() {
        let slot = Slot::default();
        assert!(slot.newer_than(0).is_none());
        let mut pattern = Pattern::new(size(), 1);
        let (first, second) = (pattern.next_picture(), pattern.next_picture());
        slot.put(first);
        slot.put(second.clone());
        // Two were put in; the newest is the one given, once.
        let Some((seen, newest)) = slot.newer_than(0) else {
            unreachable!()
        };
        assert_eq!((seen, &*newest), (2, &second));
        assert!(slot.newer_than(seen).is_none());
    }

    #[test]
    fn whoever_asked_is_woken_for_each_picture() {
        let slot = Slot::default();
        let woken = Arc::new(AtomicU64::new(0));
        let counting = Arc::clone(&woken);
        slot.on_new(move || {
            counting.fetch_add(1, Ordering::Relaxed);
        });
        slot.put(Picture::black(size()));
        slot.put(Picture::black(size()));
        assert_eq!(woken.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_coded_feed_gives_the_picture_back() {
        let (raw, back) = (Arc::new(Slot::default()), Arc::new(Slot::default()));
        let coding = Arc::new(Coding::default());
        let (made, pictures) = std::sync::mpsc::channel();
        back.on_new(move || {
            let _ = made.send(());
        });
        // A thousand a second, so that the test does not wait on the clock.
        let settings = Settings {
            per_second: 1000,
            ..Settings::new(Size::new(64, 36).unwrap_or_else(|| unreachable!()), 200_000)
        };
        let feed = Feed::coded(
            Arc::clone(&raw),
            Arc::clone(&back),
            Arc::clone(&coding),
            settings,
            1,
        );
        for _ in 0..5 {
            assert!(pictures.recv_timeout(Duration::from_secs(10)).is_ok());
        }
        drop(feed);
        let Some((_, came_back)) = back.newer_than(0) else {
            unreachable!()
        };
        assert_eq!(came_back.size(), settings.size);
        assert_eq!(coding.cost().failed, None);
        assert!(raw.count() >= back.count());
    }

    #[test]
    fn a_seconds_sums_become_a_rate_and_what_each_picture_cost() {
        let tally = Tally {
            pictures: 30,
            bytes: 60_000,
            encode: Duration::from_millis(60),
            decode: Duration::from_millis(15),
            let_go: 2,
        };
        assert_eq!(
            tally.cost(),
            Cost {
                bitrate: 480_000,
                encode: Duration::from_millis(2),
                decode: Duration::from_micros(500),
                let_go: 2,
                failed: None,
            }
        );
        // Nothing made is nothing spent, and not a division by nothing.
        assert_eq!(Tally::default().cost(), Cost::default());
    }

    #[test]
    fn a_coded_feed_that_cannot_start_says_why() {
        let coding = Arc::new(Coding::default());
        let odd = Settings::new(Size::new(63, 36).unwrap_or_else(|| unreachable!()), 200_000);
        let feed = Feed::coded(
            Arc::new(Slot::default()),
            Arc::new(Slot::default()),
            Arc::clone(&coding),
            odd,
            1,
        );
        // Dropping waits for its thread, which has ended by itself.
        drop(feed);
        assert!(coding.cost().failed.is_some());
    }

    #[test]
    fn a_feed_makes_pictures_until_it_is_dropped() {
        let slot = Arc::new(Slot::default());
        let (made, pictures) = std::sync::mpsc::channel();
        slot.on_new(move || {
            let _ = made.send(());
        });
        let feed = Feed::pattern(Arc::clone(&slot), size(), 2);
        for _ in 0..3 {
            assert!(pictures.recv_timeout(Duration::from_secs(5)).is_ok());
        }
        // Dropping it waits for its thread to end: nothing is made after.
        drop(feed);
        let stopped = slot.count();
        assert!(stopped >= 3);
        while pictures.try_recv().is_ok() {}
        assert_eq!(slot.count(), stopped);
    }
}
