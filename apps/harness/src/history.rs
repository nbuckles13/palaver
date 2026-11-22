//! What the details remember: the voices and links as they are now, and the last two
//! minutes of the numbers worth watching, one value a second, for drawing over time.
//!
//! Plain logic: it is told what is so and how long the meeting has run, and keeps time
//! by that alone.

use crate::words::heard_after;
use palaver_media::jitter::Stats;
use palaver_proto::ParticipantId;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

/// How many values are kept of each number: two minutes of them.
pub const KEPT: usize = 120;
/// How often one is taken.
pub const EVERY: Duration = Duration::from_secs(1);

/// The values kept of one number, oldest first. `None` where there was nothing to
/// measure: a voice that was not being played.
pub type Past = Vec<Option<u32>>;

#[derive(Debug, Clone, Default)]
struct Track(VecDeque<Option<u32>>);

impl Track {
    fn push(&mut self, value: Option<u32>) {
        self.0.push_back(value);
        while self.0.len() > KEPT {
            self.0.pop_front();
        }
    }

    fn shown(&self) -> Past {
        self.0.iter().copied().collect()
    }

    /// Whether nothing kept is a value any more.
    fn empty(&self) -> bool {
        self.0.iter().all(Option::is_none)
    }
}

fn millis(span: Duration) -> u32 {
    u32::try_from(span.as_millis()).unwrap_or(u32::MAX)
}

fn count(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A voice being played, as the sound reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heard {
    pub alias: u64,
    pub name: String,
    /// The playout delay its jitter buffer is aiming for.
    pub delay: Duration,
    pub stats: Stats,
}

/// A connection, as the node reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    pub peer: ParticipantId,
    pub name: String,
    pub rtt: Duration,
    pub sent: u64,
    pub lost: u64,
}

/// How one voice is faring, as the details show it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceLine {
    pub name: String,
    /// The playout delay its jitter buffer is aiming for.
    pub delay: Duration,
    pub stats: Stats,
    /// Whether it is being played now; if not, the rest is as it last was.
    pub playing: bool,
    /// How long after arriving it was heard, in milliseconds.
    pub delay_past: Past,
    /// Frames lost or late in each second.
    pub trouble_past: Past,
}

/// One connection to another participant, as the details show it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkLine {
    pub name: String,
    pub rtt: Duration,
    /// Packets sent on it, and how many of them were lost.
    pub sent: u64,
    pub lost: u64,
    /// The round trip, in milliseconds.
    pub rtt_past: Past,
    /// Packets lost in each second.
    pub lost_past: Past,
}

#[derive(Debug, Clone)]
struct Voice {
    line: VoiceLine,
    /// The lost and late frames already counted into the past.
    counted: u64,
    delay_past: Track,
    trouble_past: Track,
}

#[derive(Debug, Clone)]
struct Link {
    line: LinkLine,
    counted: u64,
    rtt_past: Track,
    lost_past: Track,
}

/// The lost and late frames of a voice so far.
const fn trouble(stats: &Stats) -> u64 {
    stats.lost.saturating_add(stats.late)
}

/// The voices and links of a meeting, now and over the last two minutes.
#[derive(Debug, Clone, Default)]
pub struct History {
    /// How long the meeting had run when the last values were taken.
    taken: Option<Duration>,
    /// The longest wait of the sound thread since then.
    gap: Duration,
    sound_gap: Track,
    voices: BTreeMap<u64, Voice>,
    links: BTreeMap<ParticipantId, Link>,
}

impl History {
    /// What is so, `at` this long into the meeting: the sound thread's longest wait
    /// since the last telling, how much waits for the speaker, the voices being played
    /// and the connections there are.
    pub fn note(
        &mut self,
        at: Duration,
        gap: Duration,
        speaker_lead: Duration,
        heard: Vec<Heard>,
        linked: Vec<Linked>,
    ) {
        self.gap = self.gap.max(gap);
        for voice in self.voices.values_mut() {
            voice.line.playing = false;
        }
        for Heard {
            alias,
            name,
            delay,
            stats,
        } in heard
        {
            let voice = self.voices.entry(alias).or_insert_with(|| Voice {
                line: VoiceLine::default(),
                counted: trouble(&stats),
                delay_past: Track::default(),
                trouble_past: Track::default(),
            });
            voice.line.name = name;
            voice.line.delay = delay;
            voice.line.stats = stats;
            voice.line.playing = true;
        }
        // A connection that is gone is gone from the details; one that comes back
        // starts again.
        self.links
            .retain(|peer, _| linked.iter().any(|now| now.peer == *peer));
        for now in linked {
            let link = self.links.entry(now.peer).or_insert_with(|| Link {
                line: LinkLine::default(),
                counted: now.lost,
                rtt_past: Track::default(),
                lost_past: Track::default(),
            });
            link.line.name = now.name;
            link.line.rtt = now.rtt;
            link.line.sent = now.sent;
            link.line.lost = now.lost;
        }
        if self
            .taken
            .is_none_or(|taken| at.saturating_sub(taken) >= EVERY)
        {
            self.taken = Some(at);
            self.take(speaker_lead);
        }
    }

    /// Take this second's value of each number.
    fn take(&mut self, speaker_lead: Duration) {
        self.sound_gap
            .push(Some(millis(std::mem::take(&mut self.gap))));
        for voice in self.voices.values_mut() {
            if voice.line.playing {
                let total = trouble(&voice.line.stats);
                // A voice that starts afresh counts from nothing again.
                let new = total.saturating_sub(voice.counted);
                voice.counted = total;
                voice
                    .delay_past
                    .push(Some(millis(heard_after(voice.line.delay, speaker_lead))));
                voice.trouble_past.push(Some(count(new)));
            } else {
                voice.delay_past.push(None);
                voice.trouble_past.push(None);
            }
        }
        // A voice not played for as long as is remembered is forgotten.
        self.voices.retain(|_, voice| !voice.delay_past.empty());
        for link in self.links.values_mut() {
            let new = link.line.lost.saturating_sub(link.counted);
            link.counted = link.line.lost;
            link.rtt_past.push(Some(millis(link.line.rtt)));
            link.lost_past.push(Some(count(new)));
        }
    }

    /// The sound thread's longest wait in each second, in milliseconds.
    #[must_use]
    pub fn sound_gap(&self) -> Past {
        self.sound_gap.shown()
    }

    /// Every voice played in the last two minutes, by alias.
    #[must_use]
    pub fn voices(&self) -> Vec<VoiceLine> {
        self.voices
            .values()
            .map(|voice| VoiceLine {
                delay_past: voice.delay_past.shown(),
                trouble_past: voice.trouble_past.shown(),
                ..voice.line.clone()
            })
            .collect()
    }

    #[must_use]
    pub fn links(&self) -> Vec<LinkLine> {
        self.links
            .values()
            .map(|link| LinkLine {
                rtt_past: link.rtt_past.shown(),
                lost_past: link.lost_past.shown(),
                ..link.line.clone()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn bo(delay_ms: u64, lost: u64, late: u64) -> Heard {
        Heard {
            alias: 7,
            name: String::from("Bo"),
            delay: ms(delay_ms),
            stats: Stats {
                lost,
                late,
                ..Stats::default()
            },
        }
    }

    fn link(rtt_ms: u64, lost: u64) -> Linked {
        Linked {
            peer: ParticipantId::from_key_hash([4; 16]),
            name: String::from("Bo"),
            rtt: ms(rtt_ms),
            sent: 1000,
            lost,
        }
    }

    #[test]
    fn one_value_a_second_is_kept_however_often_it_is_told() {
        let mut history = History::default();
        // Told five times a second for three seconds; the longest wait in each counts.
        for tenth in 0..15u64 {
            let gap = if tenth == 7 { ms(45) } else { ms(11) };
            history.note(ms(tenth * 200), gap, ms(20), vec![], vec![]);
        }
        assert_eq!(history.sound_gap(), [Some(11), Some(11), Some(45)]);
    }

    #[test]
    fn a_voice_is_drawn_by_when_it_is_heard_and_by_what_went_wrong_each_second() {
        let mut history = History::default();
        // The speaker keeps 20 ms and works in pieces of 10: heard 30 ms after the buffer.
        history.note(seconds(0), ms(10), ms(20), vec![bo(40, 3, 1)], vec![]);
        history.note(seconds(1), ms(10), ms(20), vec![bo(40, 3, 1)], vec![]);
        history.note(seconds(2), ms(10), ms(20), vec![bo(20, 5, 2)], vec![]);
        let voices = history.voices();
        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].delay_past, [Some(70), Some(70), Some(50)]);
        // What had gone wrong before it was first seen is not laid at the first second.
        assert_eq!(voices[0].trouble_past, [Some(0), Some(0), Some(3)]);
        assert!(voices[0].playing);
    }

    #[test]
    fn a_voice_that_stops_leaves_a_gap_and_is_forgotten_after_two_minutes() {
        let mut history = History::default();
        history.note(seconds(0), ms(10), ms(20), vec![bo(40, 0, 0)], vec![]);
        history.note(seconds(1), ms(10), ms(20), vec![], vec![]);
        let quiet = history.voices();
        assert_eq!(quiet[0].delay_past, [Some(70), None]);
        // It is still listed, as it last was.
        assert!(!quiet[0].playing);
        assert_eq!((quiet[0].name.as_str(), quiet[0].delay), ("Bo", ms(40)));
        // It comes back afresh, its counters at nothing: no trouble is made of that.
        history.note(seconds(2), ms(10), ms(20), vec![bo(40, 0, 0)], vec![]);
        assert_eq!(history.voices()[0].trouble_past, [Some(0), None, Some(0)]);
        for second in 3..=(3 + KEPT as u64) {
            history.note(seconds(second), ms(10), ms(20), vec![], vec![]);
        }
        assert!(history.voices().is_empty());
    }

    #[test]
    fn a_link_is_drawn_by_its_round_trip_and_what_it_lost_each_second() {
        let mut history = History::default();
        history.note(seconds(0), ms(10), ms(20), vec![], vec![link(7, 30)]);
        history.note(seconds(1), ms(10), ms(20), vec![], vec![link(9, 34)]);
        let links = history.links();
        assert_eq!(links[0].rtt_past, [Some(7), Some(9)]);
        assert_eq!(links[0].lost_past, [Some(0), Some(4)]);
        assert_eq!((links[0].rtt, links[0].lost), (ms(9), 34));
        // Gone with its connection.
        history.note(seconds(2), ms(10), ms(20), vec![], vec![]);
        assert!(history.links().is_empty());
    }

    #[test]
    fn no_more_than_two_minutes_are_kept() {
        let mut history = History::default();
        for second in 0..300 {
            history.note(seconds(second), ms(10), ms(20), vec![], vec![]);
        }
        assert_eq!(history.sound_gap().len(), KEPT);
    }
}
