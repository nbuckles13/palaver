//! The event loop: virtual time, an ordered queue, and a trace fingerprint.

use crate::Rng;
use palaver_proto::Timestamp;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::Duration;

/// Index of a node in the world.
pub type NodeId = u32;

/// One scheduled delivery. Ordered by time, then by the order it was scheduled, so ties are
/// broken the same way on every run.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Scheduled<E: Ord> {
    at: Timestamp,
    seq: u64,
    to: NodeId,
    event: E,
}

/// The simulated world: a clock, the event queue, the root random source and a running
/// fingerprint of everything delivered.
#[derive(Debug)]
pub struct World<E: Ord> {
    now: Timestamp,
    seq: u64,
    queue: BinaryHeap<Reverse<Scheduled<E>>>,
    rng: Rng,
    trace: u64,
    delivered: u64,
}

impl<E: Ord> World<E> {
    pub fn new(seed: u64) -> Self {
        Self {
            now: Timestamp::ZERO,
            seq: 0,
            queue: BinaryHeap::new(),
            rng: Rng::new(seed),
            trace: seed,
            delivered: 0,
        }
    }

    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// The root random source; derive per-node and per-link streams from it.
    pub fn rng(&self) -> &Rng {
        &self.rng
    }

    /// Schedule `event` for node `to`, `delay` after the current time.
    pub fn schedule(&mut self, delay: Duration, to: NodeId, event: E) {
        self.seq += 1;
        self.queue.push(Reverse(Scheduled {
            at: self.now.saturating_add(delay),
            seq: self.seq,
            to,
            event,
        }));
    }

    /// When the next event is due, if there is one.
    pub fn next_at(&self) -> Option<Timestamp> {
        self.queue.peek().map(|Reverse(next)| next.at)
    }

    /// Deliver the next event, advancing the clock to its time. `None` when the queue is empty.
    pub fn step(&mut self) -> Option<(NodeId, E)> {
        let Reverse(next) = self.queue.pop()?;
        self.now = next.at;
        self.delivered += 1;
        self.note(next.at.as_micros() ^ (u64::from(next.to) << 48));
        Some((next.to, next.event))
    }

    /// Move the clock forward to `time` if it is later than now. Events already due stay due.
    pub fn advance_to(&mut self, time: Timestamp) {
        self.now = self.now.max(time);
    }

    /// Mix a value into the trace fingerprint. Handlers call this with what they observed, so
    /// two runs have equal fingerprints only if they behaved identically.
    pub fn note(&mut self, value: u64) {
        self.trace = (self.trace ^ value)
            .wrapping_mul(0x0000_0100_0000_01b3)
            .rotate_left(23);
    }

    /// Fingerprint of the run so far.
    pub fn trace(&self) -> u64 {
        self.trace
    }

    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy protocol with random fan-out, delay and loss, enough to make the trace depend on
    /// every random draw and on tie-breaking.
    fn gossip_run(seed: u64) -> (u64, u64, Timestamp) {
        const NODES: u32 = 20;
        let mut world: World<u32> = World::new(seed);
        let mut rngs: Vec<Rng> = (0..NODES)
            .map(|n| world.rng().derive(u64::from(n)))
            .collect();
        for n in 0..NODES {
            world.schedule(Duration::ZERO, n, 6);
        }
        while let Some((node, ttl)) = world.step() {
            world.note(u64::from(ttl));
            let Some(rng) = rngs.get_mut(node as usize) else {
                continue;
            };
            if ttl == 0 {
                continue;
            }
            for _ in 0..2 {
                if rng.chance(100_000) {
                    continue; // lost
                }
                let peer = rng.below(u64::from(NODES)) as u32;
                let delay = 5_000 + rng.below(3) * 10_000; // few distinct delays: many ties
                world.schedule(Duration::from_micros(delay), peer, ttl - 1);
            }
        }
        (world.trace(), world.delivered(), world.now())
    }

    #[test]
    fn events_are_delivered_in_time_order_with_ties_in_schedule_order() {
        let mut w: World<&str> = World::new(1);
        w.schedule(Duration::from_micros(20), 0, "c");
        w.schedule(Duration::from_micros(10), 0, "a");
        w.schedule(Duration::from_micros(10), 1, "b");
        let order: Vec<&str> = std::iter::from_fn(|| w.step().map(|(_, e)| e)).collect();
        assert_eq!(order, ["a", "b", "c"]);
        assert_eq!(w.now(), Timestamp::from_micros(20));
    }

    // The determinism guard: the simulator's own promise, checked on the simulator itself.
    #[test]
    fn same_seed_gives_an_identical_trace() {
        let (a, b) = (gossip_run(42), gossip_run(42));
        assert!(
            a.1 > 500,
            "scenario too small to prove anything: {} events",
            a.1
        );
        assert_eq!(a, b);
    }

    #[test]
    fn a_different_seed_gives_a_different_trace() {
        assert_ne!(gossip_run(42).0, gossip_run(43).0);
    }
}
