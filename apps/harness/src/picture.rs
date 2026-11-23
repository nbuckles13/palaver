//! Pictures on their way to the window.
//!
//! A [`Slot`] holds the newest picture of one tile. Whoever makes pictures (a camera, a
//! decoder, a pattern) puts each in; the window takes the newest when it draws, and is
//! woken when there is one. Nothing waits on anything: a picture not taken before the
//! next arrives is dropped, which is right for something to look at.
//!
//! [`Feed`] makes a moving pattern on a thread of its own, in place of a camera.

use palaver_media::picture::{Pattern, Picture, Size};
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
