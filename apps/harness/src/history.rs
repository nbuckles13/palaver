//! What the details remember: the voices and links as they are now, and the last two
//! minutes of the numbers worth watching, one value a second, for drawing over time.
//!
//! Plain logic: it is told what is so and how long the meeting has run, and keeps time
//! by that alone.

use crate::show::Counted;
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
    /// Bytes sent and received on it so far.
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// One size of this machine's camera, as its coding stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filmed {
    /// What the size is called.
    pub name: String,
    /// Across and down, if the plan says.
    pub size: Option<(u32, u32)>,
    pub counted: Counted,
}

/// Someone else's picture, as its decoding stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub id: ParticipantId,
    pub name: String,
    /// How large the newest picture was, if there has been one.
    pub size: Option<(u32, u32)>,
    pub counted: Counted,
    /// Key frames asked for so far.
    pub keys: u64,
}

/// How pictures flowed one way over the last second taken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flowed {
    /// Thousands of bits a second.
    pub rate: u32,
    /// Pictures a second.
    pub pace: u32,
    /// How long each took to code, on average.
    pub cost: Duration,
}

/// One size of this machine's camera, as the details show it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SentLine {
    pub name: String,
    pub size: Option<(u32, u32)>,
    pub flowed: Flowed,
    /// Thousands of bits a second.
    pub rate_past: Past,
}

/// Someone else's picture, as the details show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PictureLine {
    pub id: ParticipantId,
    pub name: String,
    pub size: Option<(u32, u32)>,
    pub flowed: Flowed,
    pub keys: u64,
    /// Thousands of bits a second, and pictures a second.
    pub rate_past: Past,
    pub pace_past: Past,
}

/// A count of coding as it was when last taken, and what it has come to since.
#[derive(Debug, Clone, Default)]
struct Flowing {
    now: Counted,
    taken: Counted,
    flowed: Flowed,
    rate_past: Track,
    pace_past: Track,
}

impl Flowing {
    /// Take what has been counted in the last `over`.
    fn take(&mut self, over: Duration) {
        let millis = u64::try_from(over.as_millis()).unwrap_or(u64::MAX).max(1);
        // A count that starts afresh counts from nothing again.
        let frames = self.now.frames.saturating_sub(self.taken.frames);
        let bytes = self.now.bytes.saturating_sub(self.taken.bytes);
        let spent = self.now.spent.saturating_sub(self.taken.spent);
        self.taken = self.now;
        self.flowed = Flowed {
            rate: count(bytes.saturating_mul(8) / millis),
            pace: count(frames.saturating_mul(1000).saturating_add(millis / 2) / millis),
            cost: spent / count(frames).max(1),
        };
        self.rate_past.push(Some(self.flowed.rate));
        self.pace_past.push(Some(self.flowed.pace));
    }
}

/// Thousands of bits a second, for `bytes` carried in `over`.
fn kilobits(bytes: u64, over: Duration) -> u32 {
    let millis = u64::try_from(over.as_millis()).unwrap_or(u64::MAX).max(1);
    count(bytes.saturating_mul(8) / millis)
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
    /// What it carries each way now, in thousands of bits a second.
    pub up: u32,
    pub down: u32,
    /// The round trip, in milliseconds.
    pub rtt_past: Past,
    /// Packets lost in each second.
    pub lost_past: Past,
    /// What it carried each way, in thousands of bits a second.
    pub up_past: Past,
    pub down_past: Past,
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
    /// The bytes it has carried each way, now and when last taken.
    bytes: (u64, u64),
    bytes_taken: (u64, u64),
    rtt_past: Track,
    lost_past: Track,
    up_past: Track,
    down_past: Track,
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
    /// The sizes of this machine's camera, by what each is called.
    sent: BTreeMap<String, (SentLine, Flowing)>,
    /// The others' pictures.
    shown: BTreeMap<ParticipantId, (PictureLine, Flowing)>,
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
                bytes: (0, 0),
                bytes_taken: (now.bytes_sent, now.bytes_received),
                rtt_past: Track::default(),
                lost_past: Track::default(),
                up_past: Track::default(),
                down_past: Track::default(),
            });
            link.bytes = (now.bytes_sent, now.bytes_received);
            link.line.name = now.name;
            link.line.rtt = now.rtt;
            link.line.sent = now.sent;
            link.line.lost = now.lost;
        }
        if self
            .taken
            .is_none_or(|taken| at.saturating_sub(taken) >= EVERY)
        {
            let over = self.taken.map_or(EVERY, |taken| at.saturating_sub(taken));
            self.taken = Some(at);
            self.take(speaker_lead, over);
        }
    }

    /// The pictures as they stand: each size of this machine's camera, and everyone
    /// else's. Told before [`note`](Self::note), which takes their values with the rest.
    pub fn note_pictures(&mut self, filmed: Vec<Filmed>, shown: Vec<Shown>) {
        self.sent
            .retain(|name, _| filmed.iter().any(|now| now.name == *name));
        for now in filmed {
            let (line, flowing) = self.sent.entry(now.name.clone()).or_default();
            line.name = now.name;
            line.size = now.size;
            flowing.now = now.counted;
        }
        self.shown
            .retain(|id, _| shown.iter().any(|now| now.id == *id));
        for now in shown {
            let (line, flowing) = self.shown.entry(now.id.clone()).or_insert_with(|| {
                let line = PictureLine {
                    id: now.id.clone(),
                    name: String::new(),
                    size: None,
                    flowed: Flowed::default(),
                    keys: 0,
                    rate_past: Past::new(),
                    pace_past: Past::new(),
                };
                (line, Flowing::default())
            });
            line.name = now.name;
            line.size = now.size;
            line.keys = now.keys;
            flowing.now = now.counted;
        }
    }

    /// Take the value of each number over the last `over`: a second, or a little more.
    fn take(&mut self, speaker_lead: Duration, over: Duration) {
        for (line, flowing) in self.sent.values_mut() {
            flowing.take(over);
            line.flowed = flowing.flowed;
        }
        for (line, flowing) in self.shown.values_mut() {
            flowing.take(over);
            line.flowed = flowing.flowed;
        }
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
            link.line.up = kilobits(link.bytes.0.saturating_sub(link.bytes_taken.0), over);
            link.line.down = kilobits(link.bytes.1.saturating_sub(link.bytes_taken.1), over);
            link.bytes_taken = link.bytes;
            link.up_past.push(Some(link.line.up));
            link.down_past.push(Some(link.line.down));
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
                up_past: link.up_past.shown(),
                down_past: link.down_past.shown(),
                ..link.line.clone()
            })
            .collect()
    }

    /// Each size of this machine's camera, by what it is called.
    #[must_use]
    pub fn sent(&self) -> Vec<SentLine> {
        self.sent
            .values()
            .map(|(line, flowing)| SentLine {
                rate_past: flowing.rate_past.shown(),
                ..line.clone()
            })
            .collect()
    }

    /// Everyone else's picture.
    #[must_use]
    pub fn shown(&self) -> Vec<PictureLine> {
        self.shown
            .values()
            .map(|(line, flowing)| PictureLine {
                rate_past: flowing.rate_past.shown(),
                pace_past: flowing.pace_past.shown(),
                ..line.clone()
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
            bytes_sent: 0,
            bytes_received: 0,
        }
    }

    #[test]
    fn a_link_says_what_it_carries_each_way_a_second() {
        let mut history = History::default();
        let carried = |up, down| Linked {
            bytes_sent: up,
            bytes_received: down,
            ..link(10, 0)
        };
        // What it had carried before it was first seen is not this second's.
        history.note(
            seconds(0),
            ms(0),
            ms(0),
            vec![],
            vec![carried(50_000, 9_000)],
        );
        history.note(
            seconds(1),
            ms(0),
            ms(0),
            vec![],
            vec![carried(175_000, 19_000)],
        );
        let line = history.links().remove(0);
        // 125 000 bytes in a second are a million bits; 10 000 are eighty thousand.
        assert_eq!((line.up, line.down), (1000, 80));
        assert_eq!(line.up_past, [Some(0), Some(1000)]);
        assert_eq!(line.down_past, [Some(0), Some(80)]);
    }

    #[test]
    fn pictures_are_counted_a_second_at_a_time_each_way() {
        let mut history = History::default();
        let counted = |frames, bytes, millis| Counted {
            frames,
            bytes,
            spent: ms(millis),
        };
        let bo = ParticipantId::from_key_hash([4; 16]);
        let tell = |history: &mut History, at, sent: Counted, shown: Counted, keys| {
            let filmed = Filmed {
                name: String::from("360p"),
                size: Some((640, 360)),
                counted: sent,
            };
            let seen = Shown {
                id: bo.clone(),
                name: String::from("Bo"),
                size: Some((320, 180)),
                counted: shown,
                keys,
            };
            history.note_pictures(vec![filmed], vec![seen]);
            history.note(seconds(at), ms(0), ms(0), vec![], vec![]);
        };
        tell(&mut history, 0, Counted::default(), Counted::default(), 0);
        tell(
            &mut history,
            1,
            counted(30, 62_500, 60),
            counted(15, 12_500, 15),
            1,
        );
        let (sent, shown) = (history.sent().remove(0), history.shown().remove(0));
        assert_eq!((sent.name.as_str(), sent.size), ("360p", Some((640, 360))));
        let flowed = |rate, pace, cost| Flowed {
            rate,
            pace,
            cost: ms(cost),
        };
        assert_eq!(sent.flowed, flowed(500, 30, 2));
        assert_eq!(sent.rate_past, [Some(0), Some(500)]);
        assert_eq!((shown.flowed, shown.keys), (flowed(100, 15, 1), 1));
        assert_eq!(shown.pace_past, [Some(0), Some(15)]);
        // A second with nothing in it is nothing; someone who has gone is gone.
        tell(
            &mut history,
            2,
            counted(30, 62_500, 60),
            counted(15, 12_500, 15),
            1,
        );
        assert_eq!(history.sent().remove(0).flowed, Flowed::default());
        history.note_pictures(vec![], vec![]);
        assert!(history.sent().is_empty() && history.shown().is_empty());
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
