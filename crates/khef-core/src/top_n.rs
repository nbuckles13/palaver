//! Top-N audio selection (design section 8).
//!
//! A khef keeps one [`TopN`] for each overlay link and one for its own client. Each holds a
//! selected set of at most N tracks. On every audio packet the khef asks whether to forward
//! it; the answer is yes exactly when the packet's track is selected after this rule runs:
//!
//! 1. Every candidate track has a current level: the level on its latest packet, held for a
//!    short time and then decayed to zero, so a track that stops sending cannot keep a slot.
//! 2. Rank the candidates by current level. Each selected track that is among the N loudest
//!    and at or above the minimum level has its "last ranked in" time refreshed.
//! 3. If the packet's track is not selected, is among the N loudest and is at or above the
//!    minimum level, it replaces the quietest selected track whose time is older than the
//!    hold (ties: the one refreshed longest ago). If no selected track is that old, the
//!    packet is not forwarded.
//!
//! Free slots are filled by the first tracks seen. Slots are never emptied, so when everyone
//! is silent the set does not change. The rule runs only on packet arrival: no timers.
//!
//! Fast in, slow out: a new speaker gets in on their first speech packet as long as some
//! holder has not ranked in for the hold time; a holder leaves only when someone needs the
//! slot.

use std::collections::BTreeMap;

/// Time in microseconds on the caller's clock. Only differences matter.
pub type Micros = u64;

/// The constants of the rule. They are part of the Palaver profile: every khef in a meeting
/// uses the same values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopNConfig {
    /// How many tracks are selected.
    pub n: usize,
    /// How long after last ranking in a selected track is protected from replacement.
    pub hold: Micros,
    /// Levels below this never cause a switch and never refresh protection.
    pub min_level: u8,
    /// A level is taken at face value for this long after its packet (two packet intervals).
    pub decay_start: Micros,
    /// A level has decayed to zero this long after its packet.
    pub decay_end: Micros,
}

impl Default for TopNConfig {
    /// The values chosen in spike 4: N = 3, 250 ms hold, decay from 40 ms to 100 ms.
    fn default() -> Self {
        Self {
            n: 3,
            hold: 250_000,
            min_level: 30,
            decay_start: 40_000,
            decay_end: 100_000,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Heard {
    level: u8,
    at: Micros,
}

#[derive(Debug, Clone)]
struct Slot<K> {
    track: K,
    /// When the track last ranked among the N loudest at or above the minimum level. `None`
    /// for a track that filled a free slot without speaking: it is replaceable at once.
    ranked_in: Option<Micros>,
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
    pub fn new(config: TopNConfig) -> Self {
        Self {
            config,
            selected: Vec::new(),
            heard: BTreeMap::new(),
            switches: 0,
        }
    }

    /// The level of a track now: as sent, then decaying linearly to zero.
    fn level_now(&self, heard: Heard, now: Micros) -> u8 {
        let age = now.saturating_sub(heard.at);
        if age <= self.config.decay_start {
            return heard.level;
        }
        if age >= self.config.decay_end {
            return 0;
        }
        let span = self
            .config
            .decay_end
            .saturating_sub(self.config.decay_start);
        let left = self.config.decay_end.saturating_sub(age);
        let scaled = u64::from(heard.level)
            .saturating_mul(left)
            .checked_div(span)
            .unwrap_or(0);
        u8::try_from(scaled).unwrap_or(heard.level)
    }

    fn level_of(&self, track: &K, now: Micros) -> u8 {
        self.heard.get(track).map_or(0, |h| self.level_now(*h, now))
    }

    /// Whether `track` is among the N loudest candidates. Ties go to the smaller key, so the
    /// order is total and the same on every run.
    fn in_loudest(&self, track: &K, now: Micros) -> bool {
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
    pub fn on_packet(&mut self, track: K, level: u8, now: Micros) -> bool {
        self.heard.insert(track.clone(), Heard { level, at: now });

        // Step 2: refresh protection of selected tracks that currently rank in.
        let refresh: Vec<bool> = self
            .selected
            .iter()
            .map(|slot| {
                self.level_of(&slot.track, now) >= self.config.min_level
                    && self.in_loudest(&slot.track, now)
            })
            .collect();
        for (slot, ranked) in self.selected.iter_mut().zip(refresh) {
            if ranked {
                slot.ranked_in = Some(now);
            }
        }

        if self.is_selected(&track) {
            return true;
        }
        let speaking = level >= self.config.min_level;
        if self.selected.len() < self.config.n {
            self.selected.push(Slot {
                track,
                ranked_in: speaking.then_some(now),
            });
            return true;
        }

        // Step 3: a newcomer needs to be speaking, to rank in, and to find a replaceable slot.
        if !speaking || !self.in_loudest(&track, now) {
            return false;
        }
        let victim = self
            .selected
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.ranked_in
                    .is_none_or(|at| now.saturating_sub(at) >= self.config.hold)
            })
            .min_by_key(|(_, slot)| (self.level_of(&slot.track, now), slot.ranked_in))
            .map(|(index, _)| index);
        match victim.and_then(|index| self.selected.get_mut(index)) {
            Some(slot) => {
                *slot = Slot {
                    track,
                    ranked_in: Some(now),
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
    pub fn switches(&self) -> u64 {
        self.switches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Micros = 1000;
    const SPEECH: u8 = 80;
    const QUIET: u8 = 10;

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
            assert!(t.on_packet(track, SPEECH, 0));
        }
        t
    }

    /// Every 20 ms from `from` to `to`, each listed track sends a packet at `level`.
    fn keep_sending(t: &mut TopN<u32>, tracks: &[u32], level: u8, from: Micros, to: Micros) {
        let mut now = from;
        while now <= to {
            for track in tracks {
                t.on_packet(*track, level, now);
            }
            now += 20 * MS;
        }
    }

    #[test]
    fn free_slots_go_to_the_first_tracks_seen_even_if_silent() {
        let mut t = top();
        assert!(t.on_packet(7, QUIET, 0));
        assert!(t.on_packet(8, SPEECH, 0));
        assert!(t.on_packet(9, QUIET, 0));
        assert!(!t.on_packet(10, QUIET, 0));
        assert_eq!(selected(&t), [7, 8, 9]);
    }

    #[test]
    fn a_new_speaker_gets_in_on_the_first_packet_when_a_holder_is_stale() {
        let mut t = three_talkers();
        // Holders fall silent; five seconds later someone new speaks.
        keep_sending(&mut t, &[1, 2, 3], QUIET, 20 * MS, 5000 * MS);
        assert!(t.on_packet(4, SPEECH, 5010 * MS));
        assert_eq!(t.switches(), 1);
        assert!(selected(&t).contains(&4));
    }

    #[test]
    fn a_silent_holder_that_filled_a_free_slot_is_replaceable_at_once() {
        let mut t = top();
        for track in 1..=3 {
            t.on_packet(track, QUIET, 0);
        }
        assert!(t.on_packet(4, SPEECH, MS));
        assert_eq!(selected(&t).len(), 3);
    }

    #[test]
    fn the_hold_protects_recent_speakers_then_expires() {
        let mut t = three_talkers();
        keep_sending(&mut t, &[1, 2, 3], SPEECH, 20 * MS, 1000 * MS);
        // All three stop. A fourth voice arrives 100 ms later: still inside the hold.
        assert!(!t.on_packet(4, SPEECH, 1100 * MS));
        // ...and gets in once a holder has not ranked in for 250 ms.
        assert!(!t.on_packet(4, SPEECH, 1240 * MS));
        assert!(t.on_packet(4, SPEECH, 1260 * MS));
    }

    #[test]
    fn a_pausing_speaker_keeps_the_slot_when_nobody_else_needs_it() {
        let mut t = three_talkers();
        keep_sending(&mut t, &[1], QUIET, 20 * MS, 10_000 * MS);
        assert_eq!(selected(&t), [1, 2, 3]);
        assert_eq!(t.switches(), 0);
        assert!(t.on_packet(1, SPEECH, 10_020 * MS));
    }

    #[test]
    fn below_the_minimum_level_never_switches_and_never_refreshes() {
        let mut t = three_talkers();
        let just_under = TopNConfig::default().min_level - 1;
        // A noisy outsider below the minimum, louder than the silent holders, never gets in.
        keep_sending(&mut t, &[1, 2, 3], QUIET, 20 * MS, 2000 * MS);
        keep_sending(&mut t, &[9], just_under, 20 * MS, 2000 * MS);
        assert_eq!(selected(&t), [1, 2, 3]);
        // A noisy holder below the minimum is not protected by its noise.
        let mut t = three_talkers();
        keep_sending(&mut t, &[1], just_under, 20 * MS, 2000 * MS);
        keep_sending(&mut t, &[2, 3], QUIET, 20 * MS, 2000 * MS);
        for (i, newcomer) in [4, 5, 6].into_iter().enumerate() {
            assert!(
                t.on_packet(newcomer, SPEECH, (2010 + i as u64) * MS),
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
        keep_sending(&mut t, &[2], 25, 20 * MS, 1000 * MS);
        keep_sending(&mut t, &[3], SPEECH, 20 * MS, 1000 * MS);
        assert!(t.on_packet(4, SPEECH, 1005 * MS));
        assert_eq!(selected(&t), [2, 3, 4]);
    }

    #[test]
    fn a_fourth_voice_waits_while_three_are_talking_unless_it_is_louder_for_the_hold() {
        let mut t = three_talkers();
        // Equal levels: the incumbents keep ranking in, so the fourth never enters.
        let mut now = 20 * MS;
        while now <= 2000 * MS {
            for track in 1..=3 {
                t.on_packet(track, SPEECH, now);
            }
            assert!(!t.on_packet(4, SPEECH, now + MS));
            now += 20 * MS;
        }
        // Clearly louder than track 3 for longer than the hold: it displaces track 3.
        let start = now;
        let mut entered_at = None;
        while now <= start + 600 * MS {
            t.on_packet(1, SPEECH, now);
            t.on_packet(2, SPEECH, now);
            t.on_packet(3, 60, now);
            if t.on_packet(4, 100, now + MS) && entered_at.is_none() {
                entered_at = Some(now - start);
            }
            now += 20 * MS;
        }
        let waited = entered_at.expect("the louder voice never got in");
        assert!(
            (240 * MS..=300 * MS).contains(&waited),
            "entered after {} ms",
            waited / MS
        );
        assert_eq!(selected(&t), [1, 2, 4]);
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
                t.on_packet(newcomer, SPEECH, (3010 + i as u64) * MS),
                "newcomer {newcomer}"
            );
        }
        assert_eq!(selected(&t), [4, 5, 6]);
    }

    #[test]
    fn a_level_is_held_then_decays_linearly_to_zero() {
        let t = top();
        let heard = Heard { level: 90, at: 0 };
        assert_eq!(t.level_now(heard, 0), 90);
        assert_eq!(t.level_now(heard, 40 * MS), 90);
        assert_eq!(t.level_now(heard, 70 * MS), 45);
        assert_eq!(t.level_now(heard, 100 * MS), 0);
        assert_eq!(t.level_now(heard, 10_000 * MS), 0);
        // A packet stamped later than "now" (reordering) is taken at face value.
        assert_eq!(t.level_now(Heard { level: 90, at: 500 }, 0), 90);
    }

    #[test]
    fn removing_a_track_frees_its_slot() {
        let mut t = three_talkers();
        t.remove(&2);
        assert_eq!(selected(&t), [1, 3]);
        assert!(t.on_packet(9, QUIET, MS));
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
                forwarded += u32::from(t.on_packet(track, level, step * 5 * MS));
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
        assert!(!t.on_packet(1, SPEECH, 0));
        assert_eq!(t.selected().count(), 0);
    }
}
