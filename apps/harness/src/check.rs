//! Trying the microphone: how loud it is, on a meter, and whether what it gives can be
//! used as it is.
//!
//! [`Ear`] is the judging, and plain logic: it is given frames and says what it finds.
//! [`MicCheck`] runs one on the default microphone before a meeting; in a meeting the
//! sound thread runs one on what it captures, before any cleaning of our own.

use palaver_media::Channels;
use palaver_media::level::audio_level;
use palaver_media::pcm::to_i16;
use palaver_sound::Microphone;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// How many frames the ear judges by: the last five seconds.
const WINDOW: usize = 250;
/// How many it wants before it says anything: one second.
const ENOUGH: usize = 50;
/// Speech is judged by the level of the twelfth loudest frame: about the loudest quarter
/// second, which one click or knock does not reach.
const LOUDEST_FEW: usize = 12;
/// Below this nothing has been said yet. On the scale of [`palaver_media::level`], 67 is
/// -60 dB: the room, not a voice.
pub const SPEECH_FLOOR: u8 = 67;
/// A voice below this is faint: 87 is -40 dB. A headset at an ordinary level gave -35 dB;
/// the same one behind the system's own cleaning gave -44 to -63 dB.
pub const FAINT_BELOW: u8 = 87;
/// With this share of its frames exactly silent, in tenths, a microphone that also gives
/// sound is being gated before it gets here. No microphone is that silent by itself.
const GATED_TENTHS: usize = 1;

/// What an [`Ear`] makes of a microphone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Finding {
    /// Not enough has been heard to say.
    #[default]
    TooSoon,
    /// Every sample is exactly nought: muted or switched off somewhere before here.
    NoSignal,
    /// Sound with stretches of exact silence in it: something before here is cleaning
    /// it, and cutting it off between sounds.
    Processed,
    /// Sound, and nothing as loud as a voice yet.
    NothingSaid,
    /// A voice, and a faint one.
    Faint,
    /// A voice at a level that can be worked with.
    Good,
}

impl Finding {
    /// As a number, to be kept where threads share it.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::TooSoon => 0,
            Self::NoSignal => 1,
            Self::Processed => 2,
            Self::NothingSaid => 3,
            Self::Faint => 4,
            Self::Good => 5,
        }
    }

    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::NoSignal,
            2 => Self::Processed,
            3 => Self::NothingSaid,
            4 => Self::Faint,
            5 => Self::Good,
            _ => Self::TooSoon,
        }
    }
}

/// Judges a microphone by the last few seconds of what it gave.
#[derive(Debug, Clone, Default)]
pub struct Ear {
    /// The level of each frame, or `None` for one that was exactly silent.
    heard: VecDeque<Option<u8>>,
    silent: usize,
    /// Set once sound and exact silence have been heard together, and kept through
    /// silence: a gated microphone gives nothing at all while nobody speaks.
    gated: bool,
}

impl Ear {
    /// Hear one frame, as the device gave it.
    pub fn hear(&mut self, frame: &[f32]) {
        // Dead silence is the sign looked for: under half a step of 16 bits (-96 dB), as
        // it was measured. A gate leaves that; a microphone open to a room never does.
        // Not exact noughts: what a gate leaves may be dither, or a filter's tail.
        let silent = frame.iter().all(|sample| to_i16(*sample) == 0);
        self.heard.push_back((!silent).then(|| audio_level(frame)));
        self.silent = self.silent.saturating_add(usize::from(silent));
        if self.heard.len() > WINDOW && self.heard.pop_front() == Some(None) {
            self.silent = self.silent.saturating_sub(1);
        }
        if self.heard.len() < ENOUGH {
            return;
        }
        let much_silence = self.silent.saturating_mul(10) >= self.heard.len() * GATED_TENTHS;
        if self.silent < self.heard.len() {
            // Some sound: gated if there is much exact silence with it, and not if not.
            self.gated = much_silence;
        }
    }

    #[must_use]
    pub fn finding(&self) -> Finding {
        if self.heard.len() < ENOUGH {
            return Finding::TooSoon;
        }
        if self.gated {
            return Finding::Processed;
        }
        if self.silent == self.heard.len() {
            return Finding::NoSignal;
        }
        let mut levels: Vec<u8> = self.heard.iter().flatten().copied().collect();
        levels.sort_unstable_by(|a, b| b.cmp(a));
        match levels.get(LOUDEST_FEW.saturating_sub(1)).copied() {
            Some(level) if level >= FAINT_BELOW => Finding::Good,
            Some(level) if level >= SPEECH_FLOOR => Finding::Faint,
            _ => Finding::NothingSaid,
        }
    }
}

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
    finding: Arc<AtomicU8>,
    found: Arc<Mutex<Found>>,
    thread: Option<JoinHandle<()>>,
}

impl MicCheck {
    #[must_use]
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let loudest = Arc::new(AtomicU8::new(0));
        let found = Arc::new(Mutex::new(Found::Opening));
        let finding = Arc::new(AtomicU8::new(Finding::TooSoon.code()));
        let (stopping, level, finding_out, judged) = (
            Arc::clone(&stop),
            Arc::clone(&loudest),
            Arc::clone(&found),
            Arc::clone(&finding),
        );
        let thread = std::thread::spawn(move || {
            let say = |what: Found| {
                if let Ok(mut found) = finding_out.lock() {
                    *found = what;
                }
            };
            let microphone = match Microphone::open(Channels::Mono) {
                Ok(microphone) => microphone,
                Err(error) => return say(Found::Failed(error.to_string())),
            };
            say(Found::Listening(microphone.device().name.clone()));
            let mut frame = vec![0.0; Channels::Mono.frame_len()];
            let mut ear = Ear::default();
            while !stopping.load(Ordering::Relaxed) {
                while microphone.pop(&mut frame) {
                    level.fetch_max(audio_level(&frame), Ordering::Relaxed);
                    ear.hear(&frame);
                }
                judged.store(ear.finding().code(), Ordering::Relaxed);
                std::thread::park_timeout(Duration::from_millis(20));
            }
        });
        Self {
            stop,
            loudest,
            finding,
            found,
            thread: Some(thread),
        }
    }

    /// The loudest frame since the last reading, on the scale of [`palaver_media::level`].
    #[must_use]
    pub fn loudest(&self) -> u8 {
        self.loudest.swap(0, Ordering::Relaxed)
    }

    /// What the last few seconds say of the microphone.
    #[must_use]
    pub fn finding(&self) -> Finding {
        Finding::from_code(self.finding.load(Ordering::Relaxed))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of a steady tone at `amplitude`; nought gives exact silence.
    fn frame(amplitude: f32) -> Vec<f32> {
        (0..Channels::Mono.frame_len())
            .map(|n| if n % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    /// An ear that has heard each of `stretches`: so many frames at an amplitude.
    fn heard(stretches: &[(usize, f32)]) -> Ear {
        let mut ear = Ear::default();
        for (frames, amplitude) in stretches {
            for _ in 0..*frames {
                ear.hear(&frame(*amplitude));
            }
        }
        ear
    }

    /// The hiss of a room, a faint voice and an ordinary one: -70, -50 and -30 dB.
    const ROOM: f32 = 0.0003;
    const FAINT: f32 = 0.003;
    const VOICE: f32 = 0.03;

    #[test]
    fn it_says_nothing_until_it_has_heard_a_second() {
        assert_eq!(heard(&[(49, VOICE)]).finding(), Finding::TooSoon);
        assert_eq!(heard(&[(50, VOICE)]).finding(), Finding::Good);
    }

    #[test]
    fn a_room_with_nobody_speaking_is_waiting_for_a_voice() {
        assert_eq!(heard(&[(100, ROOM)]).finding(), Finding::NothingSaid);
        // A knock is not a voice: it takes a quarter second of sound.
        assert_eq!(
            heard(&[(100, ROOM), (5, VOICE), (50, ROOM)]).finding(),
            Finding::NothingSaid
        );
    }

    #[test]
    fn a_voice_is_good_or_faint_by_its_level() {
        assert_eq!(heard(&[(100, ROOM), (20, VOICE)]).finding(), Finding::Good);
        assert_eq!(heard(&[(100, ROOM), (20, FAINT)]).finding(), Finding::Faint);
        // And it is forgotten five seconds on.
        assert_eq!(
            heard(&[(20, VOICE), (250, ROOM)]).finding(),
            Finding::NothingSaid
        );
    }

    #[test]
    fn nothing_but_exact_silence_is_no_signal() {
        assert_eq!(heard(&[(100, 0.0)]).finding(), Finding::NoSignal);
        // A few silent frames as the device starts are not a sign of anything.
        assert_eq!(heard(&[(3, 0.0), (100, VOICE)]).finding(), Finding::Good);
    }

    #[test]
    fn silence_is_what_would_round_to_nothing_in_16_bits_and_not_only_exact_noughts() {
        // -100 dB: what a gate may leave behind.
        let residue = 0.00001;
        assert_eq!(heard(&[(100, residue)]).finding(), Finding::NoSignal);
        let gated = [(30, residue), (20, FAINT), (30, residue), (20, FAINT)];
        assert_eq!(heard(&gated).finding(), Finding::Processed);
        // The same faint voice over a room's hiss is only faint.
        let open = [(30, ROOM), (20, FAINT), (30, ROOM), (20, FAINT)];
        assert_eq!(heard(&open).finding(), Finding::Faint);
    }

    #[test]
    fn sound_with_exact_silence_between_is_a_microphone_already_processed() {
        let gated = [(30, 0.0), (20, FAINT), (30, 0.0), (20, VOICE)];
        assert_eq!(heard(&gated).finding(), Finding::Processed);
        // It stays so through the silence of nobody speaking, however long.
        let mut ear = heard(&gated);
        for _ in 0..600 {
            ear.hear(&frame(0.0));
        }
        assert_eq!(ear.finding(), Finding::Processed);
        // And is taken back once the microphone gives a room's sound again.
        for _ in 0..250 {
            ear.hear(&frame(ROOM));
        }
        assert_eq!(ear.finding(), Finding::NothingSaid);
    }

    #[test]
    fn a_finding_survives_being_kept_as_a_number() {
        for finding in [
            Finding::TooSoon,
            Finding::NoSignal,
            Finding::Processed,
            Finding::NothingSaid,
            Finding::Faint,
            Finding::Good,
        ] {
            assert_eq!(Finding::from_code(finding.code()), finding);
        }
    }
}
