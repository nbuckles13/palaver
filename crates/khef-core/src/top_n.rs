//! Top-N audio selection (design section 8).
//!
//! A khef keeps one [`TopN`] for each overlay link and one for its own client. Each holds a
//! selected set of at most N tracks. On every audio packet the khef asks whether to forward
//! it; the answer is yes exactly when the packet's track is selected after this rule runs:
//!
//! 1. Every candidate track has a current level: the level on its latest packet, held for a
//!    short time and then decayed to zero, so a track that stops sending cannot keep a slot.
//! 2. Rank the candidates by current level.
//! 3. If the packet's track is not selected, is among the N loudest and is at or above the
//!    minimum level, it replaces the lowest-ranked selected track that is not itself among
//!    the N loudest and is past its hold. If there is none, the packet is not forwarded.
//!
//! **The hold** runs from the moment a track enters the set: for that long it cannot be
//! replaced. It is for the listener: a track that came and went within a few packets would
//! be heard as a fragment of sound, and a slot flapping between voices of similar loudness
//! as a run of them. This holds for every track that enters, loud or quiet, into a free slot
//! or another's: even a quiet one may carry noise a listener would hear start and stop.
//! The hold is all that slows a switch. Once it has passed, a track that no longer ranks
//! goes the instant someone who does needs the slot.
//!
//! Because the hold is counted from entry, khefs along a path do not add to each other's:
//! a track enters the set at each of them at the same moment (give or take the link delay),
//! so its hold ends at each of them together.
//!
//! Free slots are filled by the first tracks seen. Slots are never emptied, so when everyone
//! is silent the set does not change. The rule runs only on packet arrival: no timers.

use palaver_proto::Timestamp;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::time::Duration;

/// The constants of the rule. They are part of the Palaver profile: every khef in a meeting
/// uses the same values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopNConfig {
    /// How many tracks are selected.
    pub n: usize,
    /// How long after entering the set a track cannot be replaced.
    pub hold: Duration,
    /// Levels below this never cause a switch. On the scale of design section 8, 80 is
    /// -47 dBFS: over a quiet room, under speech.
    pub min_level: u8,
    /// A level is taken at face value for this long after its packet (two packet intervals).
    pub decay_start: Duration,
    /// A level has decayed to zero this long after its packet.
    pub decay_end: Duration,
}

impl Default for TopNConfig {
    /// The values chosen in spike 4: N = 3, 250 ms hold, decay from 40 ms to 100 ms.
    fn default() -> Self {
        Self {
            n: 3,
            hold: Duration::from_millis(250),
            min_level: 80,
            decay_start: Duration::from_millis(40),
            decay_end: Duration::from_millis(100),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Heard {
    level: u8,
    at: Timestamp,
}

#[derive(Debug, Clone)]
struct Slot<K> {
    track: K,
    /// When the track entered the set.
    entered: Timestamp,
}

/// The selected set for one destination (an overlay link, or the local client).
#[derive(Debug, Clone)]
pub struct TopN<K> {
    config: TopNConfig,
    selected: Vec<Slot<K>>,
    heard: BTreeMap<K, Heard>,
    switches: u64,
}

impl<K: Ord + Clone> TopN<K> {
    #[must_use]
    pub const fn new(config: TopNConfig) -> Self {
        Self {
            config,
            selected: Vec::new(),
            heard: BTreeMap::new(),
            switches: 0,
        }
    }

    /// The level of a track now: as sent, then decaying linearly to zero.
    fn level_now(&self, heard: Heard, now: Timestamp) -> u8 {
        let age = now.saturating_since(heard.at);
        if age <= self.config.decay_start {
            return heard.level;
        }
        if age >= self.config.decay_end {
            return 0;
        }
        let micros = |span: Duration| u64::try_from(span.as_micros()).unwrap_or(u64::MAX);
        let span = micros(
            self.config
                .decay_end
                .saturating_sub(self.config.decay_start),
        );
        let left = micros(self.config.decay_end.saturating_sub(age));
        let scaled = u64::from(heard.level)
            .saturating_mul(left)
            .checked_div(span)
            .unwrap_or(0);
        u8::try_from(scaled).unwrap_or(heard.level)
    }

    fn level_of(&self, track: &K, now: Timestamp) -> u8 {
        self.heard.get(track).map_or(0, |h| self.level_now(*h, now))
    }

    /// Whether `track` is among the N loudest candidates. Ties go to the smaller key, so the
    /// order is total and the same on every run.
    fn in_loudest(&self, track: &K, now: Timestamp) -> bool {
        let level = self.level_of(track, now);
        let louder = self
            .heard
            .iter()
            .filter(|(other, heard)| {
                let other_level = self.level_now(**heard, now);
                other_level > level || (other_level == level && *other < track)
            })
            .count();
        louder < self.config.n
    }

    fn is_selected(&self, track: &K) -> bool {
        self.selected.iter().any(|slot| &slot.track == track)
    }

    /// Record a packet of `track` carrying `level` at time `now`, and say whether to forward
    /// it.
    pub fn on_packet(&mut self, track: K, level: u8, now: Timestamp) -> bool {
        self.heard.insert(track.clone(), Heard { level, at: now });

        if self.is_selected(&track) {
            return true;
        }
        if self.selected.len() < self.config.n {
            self.selected.push(Slot {
                track,
                entered: now,
            });
            return true;
        }

        // Step 3: a newcomer needs to be speaking, to rank in, and to find a replaceable slot.
        if level < self.config.min_level || !self.in_loudest(&track, now) {
            return false;
        }
        // The one to go is past its hold and no longer ranks. Taking a slot from a track
        // that still ranks would only have it take one back a moment later.
        let victim = self
            .selected
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                let held = now.saturating_since(slot.entered) < self.config.hold;
                !held && !self.in_loudest(&slot.track, now)
            })
            .min_by_key(|(_, slot)| (self.level_of(&slot.track, now), Reverse(&slot.track)))
            .map(|(index, _)| index);
        match victim.and_then(|index| self.selected.get_mut(index)) {
            Some(slot) => {
                *slot = Slot {
                    track,
                    entered: now,
                };
                self.switches = self.switches.saturating_add(1);
                true
            }
            None => false,
        }
    }

    /// Forget a track that has ended. Its slot, if it had one, becomes free.
    pub fn remove(&mut self, track: &K) {
        self.heard.remove(track);
        self.selected.retain(|slot| &slot.track != track);
    }

    /// The selected tracks, in no particular order.
    pub fn selected(&self) -> impl Iterator<Item = &K> {
        self.selected.iter().map(|slot| &slot.track)
    }

    /// How many times a newcomer has replaced a selected track.
    #[must_use]
    pub const fn switches(&self) -> u64 {
        self.switches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests keep time as plain microseconds and convert at the call.
    const MS: u64 = 1000;

    fn feed(t: &mut TopN<u32>, track: u32, level: u8, micros: u64) -> bool {
        t.on_packet(track, level, Timestamp::from_micros(micros))
    }
    // On the scale of design section 8: ordinary speech, and a quiet room.
    const SPEECH: u8 = 100;
    const QUIET: u8 = 60;

    fn top() -> TopN<u32> {
        TopN::new(TopNConfig::default())
    }

    fn selected(t: &TopN<u32>) -> Vec<u32> {
        let mut v: Vec<u32> = t.selected().copied().collect();
        v.sort_unstable();
        v
    }

    /// Tracks 1, 2 and 3 speak at time 0 and hold the three slots.
    fn three_talkers() -> TopN<u32> {
        let mut t = top();
        for track in 1..=3 {
            assert!(feed(&mut t, track, SPEECH, 0));
        }
        t
    }

    /// Every 20 ms from `from` to `to`, each listed track sends a packet at `level`.
    fn keep_sending(t: &mut TopN<u32>, tracks: &[u32], level: u8, from: u64, to: u64) {
        let mut now = from;
        while now <= to {
            for track in tracks {
                feed(t, *track, level, now);
            }
            now += 20 * MS;
        }
    }

    #[test]
    fn free_slots_go_to_the_first_tracks_seen_even_if_silent() {
        let mut t = top();
        assert!(feed(&mut t, 7, QUIET, 0));
        assert!(feed(&mut t, 8, SPEECH, 0));
        assert!(feed(&mut t, 9, QUIET, 0));
        assert!(!feed(&mut t, 10, QUIET, 0));
        assert_eq!(selected(&t), [7, 8, 9]);
    }

    #[test]
    fn a_new_speaker_gets_in_on_the_first_packet_when_a_holder_is_stale() {
        let mut t = three_talkers();
        // Holders fall silent; five seconds later someone new speaks.
        keep_sending(&mut t, &[1, 2, 3], QUIET, 20 * MS, 5000 * MS);
        assert!(feed(&mut t, 4, SPEECH, 5010 * MS));
        assert_eq!(t.switches(), 1);
        assert!(selected(&t).contains(&4));
    }

    #[test]
    fn a_quiet_track_that_filled_a_slot_stays_for_the_hold_too() {
        let mut t = top();
        for track in 1..=3 {
            feed(&mut t, track, QUIET, 0);
        }
        // Quiet is not nothing: a listener would hear its noise start and stop.
        assert!(!feed(&mut t, 4, SPEECH, MS));
        assert!(!feed(&mut t, 4, SPEECH, 240 * MS));
        assert!(feed(&mut t, 4, SPEECH, 250 * MS));
    }

    #[test]
    fn when_more_than_n_start_together_the_first_n_heard_stay_for_the_hold() {
        let mut t = top();
        // The quietest is heard first and fills a slot. The third loudest waits out its
        // hold rather than cut it off after one packet.
        let mut admitted = None;
        for ms in (0..=400).step_by(20) {
            for (track, level) in [(4, 85), (1, 110), (2, 105)] {
                feed(&mut t, track, level, ms * MS);
            }
            if feed(&mut t, 3, 100, ms * MS) && admitted.is_none() {
                admitted = Some(ms);
            }
        }
        assert_eq!(admitted, Some(260));
        assert_eq!(selected(&t), [1, 2, 3]);
    }

    #[test]
    fn a_track_that_has_just_entered_cannot_be_replaced_until_its_hold_is_over() {
        let mut t = three_talkers();
        // A louder voice arrives right behind them. All three entered at time 0, so none
        // can go before 250 ms.
        let mut admitted = None;
        for ms in (0..=400).step_by(20) {
            for track in 1..=3 {
                feed(&mut t, track, SPEECH, ms * MS);
            }
            if feed(&mut t, 4, 110, (ms + 10) * MS) && admitted.is_none() {
                admitted = Some(ms + 10);
            }
        }
        assert_eq!(admitted, Some(250));
        assert_eq!(selected(&t), [1, 2, 4]);
    }

    #[test]
    fn past_its_hold_a_track_that_no_longer_ranks_goes_at_once() {
        let mut t = three_talkers();
        keep_sending(&mut t, &[1, 2, 3], SPEECH, 20 * MS, 1000 * MS);
        // All three are still talking. A louder voice gets in on its first packet.
        assert!(feed(&mut t, 4, 110, 1010 * MS));
        assert_eq!(selected(&t), [1, 2, 4]);
    }

    #[test]
    fn two_voices_trading_places_switch_no_more_than_once_a_hold() {
        let mut t = top();
        // 1 and 2 talk steadily. 3 and 4 are close, and which is louder flips every packet.
        for frame in 0..100u64 {
            let now = frame * 20 * MS;
            feed(&mut t, 1, 110, now);
            feed(&mut t, 2, 110, now);
            let (three, four) = if frame % 2 == 0 { (100, 90) } else { (90, 100) };
            feed(&mut t, 3, three, now);
            feed(&mut t, 4, four, now);
        }
        // Two seconds, 250 ms hold. Without the hold it would be a switch on every packet.
        assert!((1..=8).contains(&t.switches()), "{}", t.switches());
    }

    #[test]
    fn a_new_speaker_does_not_wait_for_speakers_who_have_stopped() {
        let mut t = three_talkers();
        keep_sending(&mut t, &[1, 2, 3], SPEECH, 20 * MS, 1000 * MS);
        // All three stop at 1000 ms. For two packet intervals their levels still stand, and
        // an equal voice does not outrank them...
        assert!(!feed(&mut t, 4, SPEECH, 1030 * MS));
        // ...then they decay, and the new voice is in.
        assert!(feed(&mut t, 4, SPEECH, 1050 * MS));
    }

    #[test]
    fn a_khef_does_not_add_its_own_hold_to_the_one_before_it() {
        // What a khef further along the tree sees when the khef before it gives 5's slot to
        // 4 at 400 ms, and then 4's to 3 when 4's hold ends at 660 ms.
        let mut t = top();
        for ms in (0..400).step_by(20) {
            for (track, level) in [(1, 110), (2, 105), (5, 90)] {
                feed(&mut t, track, level, ms * MS);
            }
        }
        for ms in (400..660).step_by(20) {
            for (track, level) in [(1, 110), (2, 105), (4, 95)] {
                assert!(feed(&mut t, track, level, ms * MS));
            }
        }
        // 4 entered here when it entered there, so its hold is over here too.
        for (track, level) in [(1, 110), (2, 105), (3, 100)] {
            assert!(feed(&mut t, track, level, 660 * MS));
        }
        assert_eq!(selected(&t), [1, 2, 3]);
    }

    #[test]
    fn a_pausing_speaker_keeps_the_slot_when_nobody_else_needs_it() {
        let mut t = three_talkers();
        keep_sending(&mut t, &[1], QUIET, 20 * MS, 10_000 * MS);
        assert_eq!(selected(&t), [1, 2, 3]);
        assert_eq!(t.switches(), 0);
        assert!(feed(&mut t, 1, SPEECH, 10_020 * MS));
    }

    #[test]
    fn below_the_minimum_level_never_switches_in_and_does_not_keep_a_slot() {
        let mut t = three_talkers();
        let just_under = TopNConfig::default().min_level - 1;
        // A noisy outsider below the minimum, louder than the silent holders, never gets in.
        keep_sending(&mut t, &[1, 2, 3], QUIET, 20 * MS, 2000 * MS);
        keep_sending(&mut t, &[9], just_under, 20 * MS, 2000 * MS);
        assert_eq!(selected(&t), [1, 2, 3]);
        // A noisy holder below the minimum does not keep its slot from those who speak.
        let mut t = three_talkers();
        keep_sending(&mut t, &[1], just_under, 20 * MS, 2000 * MS);
        keep_sending(&mut t, &[2, 3], QUIET, 20 * MS, 2000 * MS);
        for (i, newcomer) in [4, 5, 6].into_iter().enumerate() {
            assert!(
                feed(&mut t, newcomer, SPEECH, (2010 + i as u64) * MS),
                "newcomer {newcomer}"
            );
        }
        assert_eq!(selected(&t), [4, 5, 6]);
    }

    #[test]
    fn the_quietest_replaceable_holder_goes_first() {
        let mut t = three_talkers();
        // 1 is silent, 2 murmurs below the minimum, 3 keeps talking.
        keep_sending(&mut t, &[1], QUIET, 20 * MS, 1000 * MS);
        keep_sending(&mut t, &[2], 75, 20 * MS, 1000 * MS);
        keep_sending(&mut t, &[3], SPEECH, 20 * MS, 1000 * MS);
        assert!(feed(&mut t, 4, SPEECH, 1005 * MS));
        assert_eq!(selected(&t), [2, 3, 4]);
    }

    #[test]
    fn a_fourth_voice_no_louder_than_the_three_talking_stays_out() {
        let mut t = three_talkers();
        let mut now = 20 * MS;
        while now <= 2000 * MS {
            for track in 1..=3 {
                feed(&mut t, track, SPEECH, now);
            }
            assert!(!feed(&mut t, 4, SPEECH, now + MS));
            now += 20 * MS;
        }
        assert_eq!(t.switches(), 0);
    }

    #[test]
    fn a_holder_that_stops_sending_decays_and_frees_its_slot() {
        let mut t = three_talkers();
        keep_sending(&mut t, &[1, 2, 3], SPEECH, 20 * MS, 1000 * MS);
        // Track 3 crashes mid-speech: no silent packets, nothing at all. 1 and 2 go quiet.
        keep_sending(&mut t, &[1, 2], QUIET, 1020 * MS, 3000 * MS);
        // Three newcomers in turn all get in; without decay the dead track would block one.
        for (i, newcomer) in [4, 5, 6].into_iter().enumerate() {
            assert!(
                feed(&mut t, newcomer, SPEECH, (3010 + i as u64) * MS),
                "newcomer {newcomer}"
            );
        }
        assert_eq!(selected(&t), [4, 5, 6]);
    }

    #[test]
    fn a_level_is_held_then_decays_linearly_to_zero() {
        let t = top();
        let heard = Heard {
            level: 90,
            at: Timestamp::ZERO,
        };
        assert_eq!(t.level_now(heard, Timestamp::from_micros(0)), 90);
        assert_eq!(t.level_now(heard, Timestamp::from_micros(40 * MS)), 90);
        assert_eq!(t.level_now(heard, Timestamp::from_micros(70 * MS)), 45);
        assert_eq!(t.level_now(heard, Timestamp::from_micros(100 * MS)), 0);
        assert_eq!(t.level_now(heard, Timestamp::from_micros(10_000 * MS)), 0);
        // A packet stamped later than "now" (reordering) is taken at face value.
        assert_eq!(
            t.level_now(
                Heard {
                    level: 90,
                    at: Timestamp::from_micros(500)
                },
                Timestamp::ZERO
            ),
            90
        );
    }

    #[test]
    fn removing_a_track_frees_its_slot() {
        let mut t = three_talkers();
        t.remove(&2);
        assert_eq!(selected(&t), [1, 3]);
        assert!(feed(&mut t, 9, QUIET, MS));
        assert_eq!(selected(&t), [1, 3, 9]);
    }

    #[test]
    fn never_more_than_n_selected_and_the_same_input_gives_the_same_result() {
        let run = || {
            let mut t = top();
            let mut x = 0x2545_f491_4f6c_dd1du64;
            let mut forwarded = 0u32;
            for step in 0..20_000u64 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let track = (x % 12) as u32;
                let level = ((x >> 8) % 128) as u8;
                forwarded += u32::from(feed(&mut t, track, level, step * 5 * MS));
                assert!(t.selected().count() <= 3);
            }
            (forwarded, selected(&t), t.switches())
        };
        let (a, b) = (run(), run());
        assert!(
            a.0 > 1000 && a.2 > 10,
            "the scenario exercised too little: {a:?}"
        );
        assert_eq!(a, b);
    }

    #[test]
    fn n_of_zero_forwards_nothing() {
        let mut t: TopN<u32> = TopN::new(TopNConfig {
            n: 0,
            ..TopNConfig::default()
        });
        assert!(!feed(&mut t, 1, SPEECH, 0));
        assert_eq!(t.selected().count(), 0);
    }
}
