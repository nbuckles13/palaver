//! Trying the microphone before a meeting: how loud it is, on a meter.

use palaver_media::Channels;
use palaver_media::level::audio_level;
use palaver_sound::Microphone;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// What the check has found so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Found {
    /// Still opening the device.
    #[default]
    Opening,
    /// Listening to the microphone of this name.
    Listening(String),
    /// The microphone could not be opened, and why.
    Failed(String),
}

/// The default microphone, listened to on a thread of its own until dropped.
pub struct MicCheck {
    stop: Arc<AtomicBool>,
    loudest: Arc<AtomicU8>,
    found: Arc<Mutex<Found>>,
    thread: Option<JoinHandle<()>>,
}

impl MicCheck {
    #[must_use]
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let loudest = Arc::new(AtomicU8::new(0));
        let found = Arc::new(Mutex::new(Found::Opening));
        let (stopping, level, finding) =
            (Arc::clone(&stop), Arc::clone(&loudest), Arc::clone(&found));
        let thread = std::thread::spawn(move || {
            let say = |what: Found| {
                if let Ok(mut found) = finding.lock() {
                    *found = what;
                }
            };
            let microphone = match Microphone::open(Channels::Mono) {
                Ok(microphone) => microphone,
                Err(error) => return say(Found::Failed(error.to_string())),
            };
            say(Found::Listening(microphone.device().name.clone()));
            let mut frame = vec![0.0; Channels::Mono.frame_len()];
            while !stopping.load(Ordering::Relaxed) {
                while microphone.pop(&mut frame) {
                    level.fetch_max(audio_level(&frame), Ordering::Relaxed);
                }
                std::thread::park_timeout(Duration::from_millis(20));
            }
        });
        Self {
            stop,
            loudest,
            found,
            thread: Some(thread),
        }
    }

    /// The loudest frame since the last reading, on the scale of [`palaver_media::level`].
    #[must_use]
    pub fn loudest(&self) -> u8 {
        self.loudest.swap(0, Ordering::Relaxed)
    }

    #[must_use]
    pub fn found(&self) -> Found {
        self.found
            .lock()
            .map(|found| found.clone())
            .unwrap_or_default()
    }
}

impl Drop for MicCheck {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
