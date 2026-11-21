//! A [`Voice`] on the system's devices, on a thread of its own.
//!
//! The thread opens the speaker and the microphone (a device's stream has to stay on
//! the thread that opened it), then turns the voice each time the speaker has taken
//! sound. It ends when told to, or when the codec fails, and gives the call back.

use crate::voice::{Turn, Voice};
use palaver_media::Channels;
use palaver_media::process::Settings;
use palaver_sdk::Call;
use palaver_sound::{Device, Microphone, SoundError, Speaker};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

/// How much sound is kept waiting for the speaker.
pub const AHEAD_MS: u32 = 40;
/// How long a turn waits for the speaker before looking at the microphone anyway.
const PATIENCE: Duration = Duration::from_millis(20);

/// What the thread has been doing, for whoever shows it.
#[derive(Debug, Default)]
pub struct Meter {
    loudest: AtomicU8,
    voices: AtomicUsize,
    captured: AtomicU64,
    sent: AtomicU64,
    played: AtomicU64,
    starved: AtomicU64,
    dropped: AtomicU64,
}

/// A reading of the [`Meter`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reading {
    /// The loudest frame from the microphone since the last reading, after cleaning.
    pub loudest: u8,
    /// How many voices are being played.
    pub voices: usize,
    /// Frames the microphone has given, frames of those sent on, and frames played.
    pub captured: u64,
    pub sent: u64,
    pub played: u64,
    /// Samples the speaker wanted and did not get, and samples it was too slow to take.
    pub starved: u64,
    pub dropped: u64,
}

impl Meter {
    fn note(&self, turn: Turn, starved_and_dropped: (u64, u64)) {
        self.loudest.fetch_max(turn.loudest, Ordering::Relaxed);
        if turn.played > 0 {
            self.voices.store(turn.voices, Ordering::Relaxed);
        }
        self.captured
            .fetch_add(turn.captured as u64, Ordering::Relaxed);
        self.sent.fetch_add(turn.sent as u64, Ordering::Relaxed);
        self.played.fetch_add(turn.played as u64, Ordering::Relaxed);
        self.starved.store(starved_and_dropped.0, Ordering::Relaxed);
        self.dropped.store(starved_and_dropped.1, Ordering::Relaxed);
    }

    /// What has happened; the loudest level starts again from here.
    pub fn read(&self) -> Reading {
        Reading {
            loudest: self.loudest.swap(0, Ordering::Relaxed),
            voices: self.voices.load(Ordering::Relaxed),
            captured: self.captured.load(Ordering::Relaxed),
            sent: self.sent.load(Ordering::Relaxed),
            played: self.played.load(Ordering::Relaxed),
            starved: self.starved.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }
}

/// The devices could not be opened.
pub struct Failed {
    pub error: SoundError,
    /// The call, unused, to be stopped; `None` only if the sound thread panicked.
    pub call: Option<Call>,
}

/// A voice running on the system's devices.
pub struct Live {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Call>,
    meter: Arc<Meter>,
    microphone: Device,
    speaker: Device,
}

impl Live {
    /// Open the default speaker and microphone and start carrying `call` on them.
    /// `call` must send one channel and play `playout`.
    ///
    /// # Errors
    ///
    /// When either device cannot be opened; the call comes back with the reason.
    pub fn start(
        call: Call,
        cleaning: Option<Settings>,
        playout: Channels,
    ) -> Result<Self, Box<Failed>> {
        let stop = Arc::new(AtomicBool::new(false));
        let meter = Arc::new(Meter::default());
        let (opened, devices) = mpsc::channel();
        let (stopping, metering) = (Arc::clone(&stop), Arc::clone(&meter));
        let thread = std::thread::spawn(move || {
            // The speaker first: what the microphone gathers while the other opens
            // would be sent late.
            let both = Speaker::open(playout)
                .and_then(|speaker| Microphone::open(Channels::Mono).map(|mic| (mic, speaker)));
            let (microphone, speaker) = match both {
                Ok(both) => both,
                Err(error) => {
                    let _ = opened.send(Err(error));
                    return call;
                }
            };
            let _ = opened.send(Ok((microphone.device().clone(), speaker.device().clone())));
            let rate = u64::from(speaker.device().format.rate);
            let ahead = usize::try_from(rate.saturating_mul(u64::from(AHEAD_MS)) / 1000)
                .unwrap_or(usize::MAX);
            let mut voice = Voice::new(call, microphone, speaker, cleaning, playout, ahead);
            while !stopping.load(Ordering::Relaxed) {
                // Room or no room, the microphone is looked at as often as this.
                let _ = voice.speaker().wait_for_room(ahead, PATIENCE);
                let Ok(turn) = voice.turn() else { break };
                metering.note(turn, voice.speaker().starved_and_dropped());
            }
            voice.into_call()
        });
        match devices.recv() {
            Ok(Ok((microphone, speaker))) => Ok(Self {
                stop,
                thread,
                meter,
                microphone,
                speaker,
            }),
            Ok(Err(error)) => Err(Box::new(Failed {
                error,
                call: thread.join().ok(),
            })),
            // The thread ended without a word, which takes a panic in a device's code.
            Err(_) => Err(Box::new(Failed {
                error: SoundError::Stream(String::from("the sound thread ended")),
                call: thread.join().ok(),
            })),
        }
    }

    #[must_use]
    pub const fn microphone(&self) -> &Device {
        &self.microphone
    }

    #[must_use]
    pub const fn speaker(&self) -> &Device {
        &self.speaker
    }

    #[must_use]
    pub fn meter(&self) -> &Meter {
        &self.meter
    }

    /// Whether the thread is still carrying sound; it stops by itself if the codec fails.
    #[must_use]
    pub fn running(&self) -> bool {
        !self.thread.is_finished()
    }

    /// Stop carrying sound and give the call back. `None` if the thread panicked.
    #[must_use]
    pub fn stop(self) -> Option<Call> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().ok()
    }
}
