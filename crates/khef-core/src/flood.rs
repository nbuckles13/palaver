//! The control flood: the one forwarding rule that needs no plan (design section 5).
//!
//! Control tracks (reports and the plan) carry one object per group, and only the latest
//! group of a track matters. Every khef:
//!
//! - keeps the latest group of every control track;
//! - passes a newer group on to every neighbour except the one it came from;
//! - gives a neighbour that connects the latest group of every track, which is how a joiner
//!   gets the plan and every report without asking for history.
//!
//! "Newer" is by group id alone, so a copy that arrives over a second path, or an old object
//! still going round, is dropped and the flood ends. The caller must check an object's
//! signature before offering it: an unchecked object with a high group id would shut out the
//! real ones that follow.
//!
//! State is bounded. A track may have a lifetime (a report is of no use a few seconds after
//! it was sent); past it the object is dropped but its group id is remembered, so the old
//! object cannot come back in from a neighbour that still holds it. The number of tracks is
//! capped; when the cap is reached the track that expired longest ago makes room.

use palaver_proto::Timestamp;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// What to do with an object that was offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer<N> {
    /// It is the newest of its track: send it to these neighbours, and hand it to the local
    /// client unless it came from there.
    Fresh { forward_to: Vec<N> },
    /// Not newer than the group already seen. Drop it.
    Stale,
    /// A track not seen before, and no room for another. Drop it.
    Full,
}

/// The latest object of one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached<K> {
    pub track: K,
    pub group: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct Held<N> {
    group: u64,
    /// `None` once the object has outlived its track's lifetime.
    payload: Option<Vec<u8>>,
    /// The neighbour it came from; `None` if from the local client, or that neighbour left.
    from: Option<N>,
    /// When the object stops being of use; `None` for never.
    expires: Option<Timestamp>,
}

impl<N> Held<N> {
    fn expired(&self, now: Timestamp) -> bool {
        self.expires.is_some_and(|expires| now >= expires)
    }
}

/// One khef's control flood state. `K` names a track, `N` a neighbour.
#[derive(Debug, Clone)]
pub struct Flood<K, N> {
    lifetime: fn(&K) -> Option<Duration>,
    max_tracks: usize,
    tracks: BTreeMap<K, Held<N>>,
    neighbours: BTreeSet<N>,
    stale: u64,
    refused: u64,
}

impl<K: Ord + Clone, N: Ord + Clone> Flood<K, N> {
    /// `lifetime` says how long an object of a track is kept after it arrives (`None`: until
    /// replaced). `max_tracks` caps how many tracks are remembered; at least one.
    pub fn new(lifetime: fn(&K) -> Option<Duration>, max_tracks: usize) -> Self {
        Self {
            lifetime,
            max_tracks: max_tracks.max(1),
            tracks: BTreeMap::new(),
            neighbours: BTreeSet::new(),
            stale: 0,
            refused: 0,
        }
    }

    /// An object arrived: from neighbour `from`, or from the local client if `None`. Its
    /// signature has been checked. If it is the newest of its track it is kept and the
    /// neighbours to send it to are returned.
    pub fn offer(
        &mut self,
        from: Option<&N>,
        track: &K,
        group: u64,
        payload: &[u8],
        now: Timestamp,
    ) -> Offer<N> {
        match self.tracks.get(track).map(|held| held.group) {
            Some(held) if group <= held => {
                self.stale = self.stale.saturating_add(1);
                return Offer::Stale;
            }
            Some(_) => {}
            None if self.tracks.len() >= self.max_tracks && !self.make_room(now) => {
                self.refused = self.refused.saturating_add(1);
                return Offer::Full;
            }
            None => {}
        }
        let expires = (self.lifetime)(track).map(|lifetime| now.saturating_add(lifetime));
        self.tracks.insert(
            track.clone(),
            Held {
                group,
                payload: Some(payload.to_vec()),
                from: from.cloned(),
                expires,
            },
        );
        Offer::Fresh {
            forward_to: self
                .neighbours
                .iter()
                .filter(|neighbour| Some(*neighbour) != from)
                .cloned()
                .collect(),
        }
    }

    /// Forget the track whose object expired longest ago. `false` if none has expired.
    fn make_room(&mut self, now: Timestamp) -> bool {
        let oldest = self
            .tracks
            .iter()
            .filter(|(_, held)| held.expired(now))
            .min_by_key(|(_, held)| held.expires)
            .map(|(track, _)| track.clone());
        match oldest {
            Some(track) => self.tracks.remove(&track).is_some(),
            None => false,
        }
    }

    /// Drop objects that have outlived their lifetime, keeping their group ids.
    fn expire(&mut self, now: Timestamp) {
        for held in self.tracks.values_mut() {
            if held.expired(now) {
                held.payload = None;
            }
        }
    }

    /// A neighbour asked for the control tracks. Returns what to send it now: the latest
    /// object of every track, except those it sent itself. Newer ones follow from
    /// [`Flood::offer`].
    pub fn connect(&mut self, neighbour: N, now: Timestamp) -> Vec<Cached<K>> {
        self.expire(now);
        let out = self
            .tracks
            .iter()
            .filter(|(_, held)| held.from.as_ref() != Some(&neighbour))
            .filter_map(|(track, held)| {
                Some(Cached {
                    track: track.clone(),
                    group: held.group,
                    payload: held.payload.clone()?,
                })
            })
            .collect();
        self.neighbours.insert(neighbour);
        out
    }

    /// A neighbour is gone. If it comes back it is treated as new and sent everything.
    pub fn disconnect(&mut self, neighbour: &N) {
        self.neighbours.remove(neighbour);
        for held in self.tracks.values_mut() {
            if held.from.as_ref() == Some(neighbour) {
                held.from = None;
            }
        }
    }

    /// The latest object of a track, if one is held and still of use.
    pub fn latest(&self, track: &K, now: Timestamp) -> Option<(u64, &[u8])> {
        let held = self.tracks.get(track).filter(|held| !held.expired(now))?;
        Some((held.group, held.payload.as_deref()?))
    }

    /// How many tracks are remembered, expired ones included; never more than the cap.
    pub fn tracks(&self) -> usize {
        self.tracks.len()
    }

    /// How many offers were dropped as not newer.
    pub fn stale(&self) -> u64 {
        self.stale
    }

    /// How many offers were dropped for lack of room.
    pub fn refused(&self) -> u64 {
        self.refused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT_LIFETIME: Duration = Duration::from_secs(3);

    /// Tracks are strings: "plan" lives until replaced, anything else is a report.
    fn lifetime(track: &&'static str) -> Option<Duration> {
        (*track != "plan").then_some(REPORT_LIFETIME)
    }

    fn flood() -> Flood<&'static str, u8> {
        Flood::new(lifetime, 100)
    }

    fn at(ms: u64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    fn fresh(to: &[u8]) -> Offer<u8> {
        Offer::Fresh {
            forward_to: to.to_vec(),
        }
    }

    fn cached(track: &'static str, group: u64, payload: &[u8]) -> Cached<&'static str> {
        Cached {
            track,
            group,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn a_new_object_goes_to_every_neighbour_but_the_one_it_came_from() {
        let mut f = flood();
        for n in [1, 2, 3] {
            assert_eq!(f.connect(n, at(0)), []);
        }
        assert_eq!(f.offer(Some(&2), &"plan", 5, b"p5", at(0)), fresh(&[1, 3]));
        // From the local client it goes to all of them.
        assert_eq!(f.offer(None, &"r-me", 1, b"r1", at(0)), fresh(&[1, 2, 3]));
    }

    #[test]
    fn a_copy_or_an_older_group_is_dropped() {
        let mut f = flood();
        f.connect(1, at(0));
        f.connect(2, at(0));
        assert_eq!(f.offer(Some(&1), &"plan", 5, b"p5", at(0)), fresh(&[2]));
        // The same object over the other path, then an older one.
        assert_eq!(f.offer(Some(&2), &"plan", 5, b"p5", at(1)), Offer::Stale);
        assert_eq!(f.offer(Some(&2), &"plan", 4, b"p4", at(2)), Offer::Stale);
        assert_eq!(f.offer(Some(&2), &"plan", 6, b"p6", at(3)), fresh(&[1]));
        assert_eq!(f.latest(&"plan", at(3)), Some((6, &b"p6"[..])));
        assert_eq!(f.stale(), 2);
    }

    #[test]
    fn a_neighbour_that_connects_gets_the_latest_of_every_track() {
        let mut f = flood();
        f.connect(1, at(0));
        f.offer(Some(&1), &"plan", 5, b"p5", at(0));
        f.offer(Some(&1), &"r-a", 10, b"a10", at(0));
        f.offer(Some(&1), &"r-a", 11, b"a11", at(10));
        f.offer(None, &"r-me", 3, b"m3", at(10));
        assert_eq!(
            f.connect(2, at(20)),
            [
                cached("plan", 5, b"p5"),
                cached("r-a", 11, b"a11"),
                cached("r-me", 3, b"m3"),
            ]
        );
        // And what follows.
        assert_eq!(f.offer(None, &"r-me", 4, b"m4", at(30)), fresh(&[1, 2]));
    }

    #[test]
    fn a_neighbour_is_not_sent_back_what_it_sent() {
        let mut f = flood();
        f.connect(1, at(0));
        f.offer(Some(&1), &"plan", 5, b"p5", at(0));
        f.offer(None, &"r-me", 3, b"m3", at(0));
        // It asks again on the same connection: only what it did not send.
        assert_eq!(f.connect(1, at(1)), [cached("r-me", 3, b"m3")]);
        // After it leaves and comes back it is new, and gets everything.
        f.disconnect(&1);
        assert_eq!(
            f.connect(1, at(2)),
            [cached("plan", 5, b"p5"), cached("r-me", 3, b"m3")]
        );
    }

    #[test]
    fn nothing_is_forwarded_to_a_neighbour_that_left() {
        let mut f = flood();
        f.connect(1, at(0));
        f.connect(2, at(0));
        f.disconnect(&2);
        assert_eq!(f.offer(None, &"plan", 1, b"p", at(0)), fresh(&[1]));
    }

    #[test]
    fn a_report_is_dropped_after_its_lifetime_and_the_plan_is_not() {
        let mut f = flood();
        f.offer(None, &"plan", 1, b"p", at(0));
        f.offer(None, &"r-a", 7, b"a7", at(0));
        assert_eq!(f.latest(&"r-a", at(2999)), Some((7, &b"a7"[..])));
        assert_eq!(f.latest(&"r-a", at(3000)), None);
        assert_eq!(f.connect(1, at(60_000)), [cached("plan", 1, b"p")]);
    }

    #[test]
    fn an_expired_object_cannot_come_back_but_a_newer_one_can() {
        let mut f = flood();
        f.connect(1, at(0));
        f.offer(Some(&1), &"r-a", 7, b"a7", at(0));
        // Long after, a neighbour that still held it sends it again.
        f.connect(2, at(10_000));
        assert_eq!(
            f.offer(Some(&2), &"r-a", 7, b"a7", at(10_000)),
            Offer::Stale
        );
        // The node itself returns with a later report.
        assert_eq!(f.offer(Some(&2), &"r-a", 8, b"a8", at(10_001)), fresh(&[1]));
        assert_eq!(f.latest(&"r-a", at(10_001)), Some((8, &b"a8"[..])));
    }

    #[test]
    fn a_refreshed_report_lives_from_its_latest_arrival() {
        let mut f = flood();
        f.offer(None, &"r-a", 1, b"a1", at(0));
        f.offer(None, &"r-a", 2, b"a2", at(2000));
        assert_eq!(f.latest(&"r-a", at(4999)), Some((2, &b"a2"[..])));
        assert_eq!(f.latest(&"r-a", at(5000)), None);
    }

    #[test]
    fn at_the_cap_the_longest_expired_track_makes_room() {
        let mut f: Flood<&'static str, u8> = Flood::new(lifetime, 3);
        f.offer(None, &"plan", 1, b"p", at(0));
        f.offer(None, &"r-a", 1, b"a", at(0));
        f.offer(None, &"r-b", 1, b"b", at(1000));
        // Nothing has expired: no room.
        assert_eq!(f.offer(None, &"r-c", 1, b"c", at(2000)), Offer::Full);
        assert_eq!(f.refused(), 1);
        // Both reports have expired; the older one, r-a, goes.
        assert_eq!(f.offer(None, &"r-c", 1, b"c", at(5000)), fresh(&[]));
        assert_eq!(f.tracks(), 3);
        assert_eq!(f.offer(None, &"r-b", 1, b"b", at(5000)), Offer::Stale);
        // A track already known is never refused for lack of room.
        assert_eq!(f.offer(None, &"plan", 2, b"p2", at(5000)), fresh(&[]));
    }

    #[test]
    fn a_plan_floods_once_through_a_ring_of_khefs() {
        // Five khefs in a ring; each one's neighbours are the two beside it.
        const KHEFS: usize = 5;
        let mut khefs: Vec<Flood<&'static str, usize>> =
            (0..KHEFS).map(|_| Flood::new(lifetime, 100)).collect();
        for (i, khef) in khefs.iter_mut().enumerate() {
            khef.connect((i + 1) % KHEFS, at(0));
            khef.connect((i + KHEFS - 1) % KHEFS, at(0));
        }
        // (from, to) messages in flight; khef 0 publishes.
        let mut in_flight = Vec::new();
        let mut sent = 0;
        let Offer::Fresh { forward_to } = khefs[0].offer(None, &"plan", 9, b"plan", at(0)) else {
            panic!()
        };
        in_flight.extend(forward_to.into_iter().map(|to| (0, to)));
        while let Some((from, to)) = in_flight.pop() {
            sent += 1;
            if let Offer::Fresh { forward_to } =
                khefs[to].offer(Some(&from), &"plan", 9, b"plan", at(1))
            {
                in_flight.extend(forward_to.into_iter().map(|next| (to, next)));
            }
        }
        for khef in &khefs {
            assert_eq!(khef.latest(&"plan", at(1)), Some((9, &b"plan"[..])));
        }
        // One message per link, plus the one extra where the two directions meet.
        assert!(sent <= KHEFS + 1, "{sent} messages");
    }
}
