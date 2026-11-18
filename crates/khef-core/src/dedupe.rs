//! Duplicate suppression for tracks that arrive over more than one path.
//!
//! A khef can receive the same object twice: control tracks flood to every neighbour, and a
//! media track has two upstreams for a moment while it is being re-routed. Objects are
//! identified by (track, group, object).
//!
//! State is bounded: for each track only a window of the most recent groups is remembered.
//! Within a group, objects are expected in increasing order (as they are on a subgroup stream,
//! and trivially for one-object groups such as audio frames and control messages), so one
//! number per group is enough: the next object id not yet seen. An object below it is a
//! duplicate. A group older than the window is treated as already seen.

use std::collections::BTreeMap;

/// Remembers what has been forwarded, per track.
#[derive(Debug, Clone)]
pub struct Dedupe<K> {
    window: usize,
    /// Per track: group id -> next object id not yet seen.
    tracks: BTreeMap<K, BTreeMap<u64, u64>>,
    duplicates: u64,
}

impl<K: Ord + Clone> Dedupe<K> {
    /// `window` is how many recent groups are remembered per track; at least one.
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            tracks: BTreeMap::new(),
            duplicates: 0,
        }
    }

    /// Whether this object is new and should be forwarded. Records it if so.
    pub fn accept(&mut self, track: &K, group: u64, object: u64) -> bool {
        let fresh = self.check(track, group, object);
        if !fresh {
            self.duplicates = self.duplicates.saturating_add(1);
        }
        fresh
    }

    fn check(&mut self, track: &K, group: u64, object: u64) -> bool {
        if !self.tracks.contains_key(track) {
            self.tracks.insert(track.clone(), BTreeMap::new());
        }
        let Some(groups) = self.tracks.get_mut(track) else {
            return false;
        };
        if let Some(next) = groups.get_mut(&group) {
            if object < *next {
                return false;
            }
            *next = object.saturating_add(1);
            return true;
        }
        // A group not in the window: new if it is newer than the oldest one remembered, or if
        // the window is not full yet. Otherwise it fell out of the window and counts as seen.
        let oldest = groups.keys().next().copied();
        if groups.len() >= self.window && oldest.is_some_and(|oldest| group < oldest) {
            return false;
        }
        groups.insert(group, object.saturating_add(1));
        while groups.len() > self.window {
            groups.pop_first();
        }
        true
    }

    /// Forget a track that has ended.
    pub fn remove(&mut self, track: &K) {
        self.tracks.remove(track);
    }

    /// How many objects have been refused as duplicates.
    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    /// How many (track, group) entries are held; never more than tracks times the window.
    pub fn entries(&self) -> usize {
        self.tracks.values().map(BTreeMap::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_second_copy_of_an_object_is_refused() {
        let mut d = Dedupe::new(4);
        assert!(d.accept(&"t", 7, 0));
        assert!(!d.accept(&"t", 7, 0));
        assert!(d.accept(&"t", 7, 1));
        assert!(!d.accept(&"t", 7, 1));
        assert!(!d.accept(&"t", 7, 0));
        assert_eq!(d.duplicates(), 3);
    }

    #[test]
    fn tracks_are_independent() {
        let mut d = Dedupe::new(4);
        assert!(d.accept(&"a", 1, 0));
        assert!(d.accept(&"b", 1, 0));
        assert!(!d.accept(&"a", 1, 0));
    }

    #[test]
    fn two_interleaved_copies_of_a_stream_yield_each_object_once() {
        // The same groups arrive over two paths, the second lagging by a few objects.
        let mut d = Dedupe::new(4);
        let mut delivered = vec![];
        let objects: Vec<(u64, u64)> = (0..6).flat_map(|g| (0..5).map(move |o| (g, o))).collect();
        for (i, first) in objects.iter().enumerate() {
            if d.accept(&"v", first.0, first.1) {
                delivered.push(*first);
            }
            if let Some(second) = i.checked_sub(3).and_then(|j| objects.get(j))
                && d.accept(&"v", second.0, second.1)
            {
                delivered.push(*second);
            }
        }
        assert_eq!(delivered, objects);
    }

    #[test]
    fn a_gap_in_object_ids_is_not_a_duplicate() {
        let mut d = Dedupe::new(4);
        assert!(d.accept(&"t", 1, 0));
        assert!(d.accept(&"t", 1, 5));
        assert!(!d.accept(&"t", 1, 3)); // behind the newest seen: treated as already seen
    }

    #[test]
    fn groups_may_arrive_out_of_order_within_the_window() {
        let mut d = Dedupe::new(4);
        for group in [10, 12, 11, 13] {
            assert!(d.accept(&"ctl", group, 0), "group {group}");
        }
        for group in [10, 11, 12, 13] {
            assert!(!d.accept(&"ctl", group, 0), "group {group} again");
        }
    }

    #[test]
    fn a_group_older_than_the_window_counts_as_seen() {
        let mut d = Dedupe::new(3);
        for group in 1..=5 {
            assert!(d.accept(&"t", group, 0));
        }
        // Groups 3, 4 and 5 are remembered; 1 and 2 have fallen out.
        assert!(!d.accept(&"t", 2, 0));
        assert!(!d.accept(&"t", 1, 9));
        assert!(!d.accept(&"t", 4, 0));
        assert!(d.accept(&"t", 6, 0));
    }

    #[test]
    fn state_is_bounded_by_tracks_times_window() {
        let mut d = Dedupe::new(8);
        for group in 0..10_000u64 {
            for track in 0..5u32 {
                d.accept(&track, group, 0);
            }
        }
        assert_eq!(d.entries(), 5 * 8);
        d.remove(&0);
        assert_eq!(d.entries(), 4 * 8);
    }

    #[test]
    fn a_window_of_zero_is_treated_as_one() {
        let mut d = Dedupe::new(0);
        assert!(d.accept(&"t", 1, 0));
        assert!(!d.accept(&"t", 1, 0));
        assert!(d.accept(&"t", 2, 0));
        assert_eq!(d.entries(), 1);
    }
}
