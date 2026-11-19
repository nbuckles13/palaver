//! The audio jitter buffer: turns packets that arrive late, early, out of order or not at all
//! into one decision per frame interval about what the decoder should play.
//!
//! Pure logic: the caller says when a packet arrived and when a frame is due, and gets back a
//! [`Playout`]. No audio is touched here; concealment is the decoder's job (Opus can rebuild
//! a lost frame from the next packet's FEC data, or from a later packet's DRED data, or
//! guess), and the buffer only says which of those is possible.
//!
//! Timing comes from the sequence number alone: one frame per number, and the publisher's
//! counter advances through silence (design section 8), so a gap in numbers is a gap in time.
//!
//! How it works:
//!
//! - **Playout delay.** Each arrival is compared with when it would have arrived on a
//!   network with no jitter; the delay kept is enough to cover 95% of what was seen over the
//!   last few seconds, plus a frame. It rises at once and falls slowly.
//! - **Running dry.** If nothing is buffered when a frame is due, the decoder conceals and
//!   the buffer remembers where that began. A packet for a frame it concealed in that spell
//!   is not thrown away as late: playout steps back to it, which is how the delay grows when
//!   the network gets worse.
//! - **A hole with later packets behind it** is a loss. The next packet's FEC is used if it
//!   is there, else DRED from a later one, else the decoder guesses.
//! - **Steering the delay.** The delay in force is measured directly: how long after its
//!   earliest possible arrival the frame at the playhead is being played. If that stays above
//!   the target, a frame is skipped (a quiet one if the level is known); if it stays below, a
//!   frame of filler is played without consuming anything.
//! - Everything is bounded: the buffer, the history, and how far a stream may jump before
//!   the buffer starts again from scratch.

use palaver_proto::Timestamp;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Length of one audio frame.
    pub frame: Duration,
    /// Least and most playout delay, in frames.
    pub min_delay_frames: u32,
    pub max_delay_frames: u32,
    /// How many recent arrivals the delay is estimated from.
    pub history: usize,
    /// How long the estimate must stay lower before the delay drops by a frame.
    pub decrease_after: Duration,
    /// How many frame intervals the delay must be a frame or more above target before a
    /// frame is skipped.
    pub shrink_after: u32,
    /// How many frame intervals the delay must be a frame or more below target before a frame
    /// of filler is played.
    pub grow_after: u32,
    /// A frame at or below this level (0 to 127) is preferred for skipping.
    pub quiet_level: u8,
    /// Whether packets carry FEC data for the frame before them.
    pub fec: bool,
    /// How many frames back a packet's DRED data reaches; 0 if DRED is off.
    pub dred_frames: u32,
    /// After this many concealed frames in a row the output is silence.
    pub max_conceal_frames: u32,
    /// Most frames held. Also how far ahead a packet may be before the buffer starts again.
    pub max_buffered: usize,
    /// This many late packets in a row means the stream moved; the buffer starts again.
    pub resync_after_late: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            frame: Duration::from_millis(20),
            min_delay_frames: 2,
            max_delay_frames: 25,
            history: 150,
            decrease_after: Duration::from_secs(2),
            shrink_after: 50,
            grow_after: 5,
            quiet_level: 30,
            fec: true,
            dred_frames: 25,
            max_conceal_frames: 5,
            max_buffered: 100,
            resync_after_late: 10,
        }
    }
}

/// One audio packet: one encoded frame.
///
/// `P` is whatever holds the encoded bytes. The buffer never looks inside it; it only keeps
/// it, hands it back, and clones it when a packet is needed twice (once to rebuild the frame
/// before it, once for its own frame). With a reference-counted slice of the datagram as it
/// was received, nothing is copied or allocated per packet from the socket to the decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet<P> {
    /// The frame counter: the group id of the object.
    pub seq: u64,
    pub payload: P,
    /// The `audio_level` property, if the packet carried one.
    pub level: Option<u8>,
}

/// What became of a packet handed to the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    Buffered,
    /// Its frame was already played or concealed.
    Late,
    Duplicate,
    /// The stream jumped; the buffer was emptied and starts again from this packet.
    Resynced,
}

/// What the decoder should produce for the next frame interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playout<P> {
    /// Nothing has been played yet and the buffer is still filling: output silence.
    Waiting,
    /// Decode this frame.
    Frame { seq: u64, payload: P },
    /// Frame `seq` is lost; rebuild it from the FEC data in the packet after it.
    Fec { seq: u64, next: P },
    /// Frame `seq` is lost; rebuild it from the DRED data in a packet `frames_back` frames
    /// later.
    Dred {
        seq: u64,
        later: P,
        frames_back: u32,
    },
    /// Frame `seq` is missing with nothing to rebuild it from: the decoder conceals.
    Conceal { seq: u64 },
    /// Nothing has arrived for a while: output silence (or comfort noise).
    Silence { seq: u64 },
    /// Play one frame of filler (concealment, stretching the audio) without consuming a
    /// frame: the buffer is lengthening its delay.
    Hold,
}

/// Counters over the life of the buffer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub received: u64,
    pub late: u64,
    pub duplicate: u64,
    pub resyncs: u64,
    pub played: u64,
    pub recovered_fec: u64,
    pub recovered_dred: u64,
    pub concealed: u64,
    pub silent: u64,
    /// Frames skipped to shorten the delay.
    pub skipped: u64,
    /// Frames of filler played to lengthen it.
    pub held: u64,
    /// Times playout stepped back to a packet that arrived after its frame was concealed.
    pub stepped_back: u64,
}

#[derive(Debug, Clone)]
struct Stored<P> {
    payload: P,
    level: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct JitterBuffer<P> {
    config: Config,
    buffer: BTreeMap<u64, Stored<P>>,
    /// The frame due next; `None` until playout starts.
    next: Option<u64>,
    first_arrival: Option<Timestamp>,
    /// The packet other arrivals are timed against.
    reference: Option<(u64, Timestamp)>,
    /// How much later than the reference's pace each recent packet arrived, in microseconds.
    lateness: VecDeque<i64>,
    /// The least of them: stands for the network's fixed delay.
    best_lateness: i64,
    target_frames: u32,
    /// When the target last changed, or was last confirmed as still needed.
    target_held_since: Timestamp,
    /// Where the current dry spell began, if the buffer has run dry.
    dry_since: Option<u64>,
    concealed_in_a_row: u32,
    late_in_a_row: u32,
    overfull_for: u32,
    underfull_for: u32,
    stats: Stats,
}

fn micros(span: Duration) -> i64 {
    i64::try_from(span.as_micros()).unwrap_or(i64::MAX)
}

impl<P: Clone> JitterBuffer<P> {
    pub fn new(config: Config) -> Self {
        let target_frames = config.min_delay_frames.min(config.max_delay_frames);
        Self {
            config,
            buffer: BTreeMap::new(),
            next: None,
            first_arrival: None,
            reference: None,
            lateness: VecDeque::new(),
            best_lateness: 0,
            target_frames,
            target_held_since: Timestamp::ZERO,
            dry_since: None,
            concealed_in_a_row: 0,
            late_in_a_row: 0,
            overfull_for: 0,
            underfull_for: 0,
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The playout delay the buffer is aiming for.
    pub fn target_delay(&self) -> Duration {
        self.config.frame.saturating_mul(self.target_frames)
    }

    /// How many frames are waiting to be played.
    pub fn depth(&self) -> usize {
        match self.next {
            Some(next) => self.buffer.range(next..).count(),
            None => self.buffer.len(),
        }
    }

    /// Forget everything about the stream except the counters and the delay learnt so far.
    fn restart(&mut self) {
        self.buffer.clear();
        self.next = None;
        self.first_arrival = None;
        self.reference = None;
        self.lateness.clear();
        self.dry_since = None;
        self.concealed_in_a_row = 0;
        self.late_in_a_row = 0;
        self.overfull_for = 0;
        self.underfull_for = 0;
    }

    /// When frame `seq` would arrive at its earliest: at the pace of the reference packet,
    /// as early as the best arrival in the history. In microseconds relative to the
    /// reference's arrival, so it may be negative.
    fn earliest_arrival(&self, seq: u64) -> Option<(Timestamp, i64)> {
        let (ref_seq, ref_at) = self.reference?;
        let frames = i64::try_from(seq)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(ref_seq).unwrap_or(i64::MAX));
        let offset = frames
            .saturating_mul(micros(self.config.frame))
            .saturating_add(self.best_lateness);
        Some((ref_at, offset))
    }

    /// The delay in force if frame `seq` is played at `now`: how long after its earliest
    /// possible arrival. In microseconds; negative if it is being played before it could
    /// have arrived.
    fn delay_in_force(&self, seq: u64, now: Timestamp) -> Option<i64> {
        let (ref_at, offset) = self.earliest_arrival(seq)?;
        let since_ref = if now >= ref_at {
            micros(now.saturating_since(ref_at))
        } else {
            micros(ref_at.saturating_since(now)).saturating_neg()
        };
        Some(since_ref.saturating_sub(offset))
    }

    /// Update the delay estimate with a packet that arrived at `now`.
    fn observe(&mut self, seq: u64, now: Timestamp) {
        let (ref_seq, ref_at) = *self.reference.get_or_insert((seq, now));
        // Signed: a packet may be older than the reference, or arrive ahead of its pace.
        let frames = i64::try_from(seq)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(ref_seq).unwrap_or(i64::MAX));
        let expected = frames.saturating_mul(micros(self.config.frame));
        let elapsed = if now >= ref_at {
            micros(now.saturating_since(ref_at))
        } else {
            micros(ref_at.saturating_since(now)).saturating_neg()
        };
        self.lateness.push_back(elapsed.saturating_sub(expected));
        while self.lateness.len() > self.config.history.max(1) {
            self.lateness.pop_front();
        }

        // Lateness is measured from the best arrival in the history, which stands for the
        // network's fixed delay; the 95th percentile above it is what the delay must cover.
        let mut sorted: Vec<i64> = self.lateness.iter().copied().collect();
        sorted.sort_unstable();
        let best = sorted.first().copied().unwrap_or(0);
        self.best_lateness = best;
        let index = sorted.len().saturating_mul(95) / 100;
        let index = index.min(sorted.len().saturating_sub(1));
        let spread = sorted
            .get(index)
            .copied()
            .unwrap_or(best)
            .saturating_sub(best);
        let frame = micros(self.config.frame).max(1);
        let frames = spread.saturating_add(frame.saturating_sub(1)) / frame;
        let wanted = u32::try_from(frames)
            .unwrap_or(u32::MAX)
            .saturating_add(1)
            .clamp(
                self.config.min_delay_frames,
                self.config
                    .max_delay_frames
                    .max(self.config.min_delay_frames),
            );

        if wanted >= self.target_frames {
            // Up at once; and while the estimate still calls for it, the clock restarts.
            self.target_frames = wanted;
            self.target_held_since = now;
        } else if now.saturating_since(self.target_held_since) >= self.config.decrease_after {
            self.target_frames = self.target_frames.saturating_sub(1);
            self.target_held_since = now;
        }
    }

    /// A packet arrived at `now`.
    pub fn insert(&mut self, packet: Packet<P>, now: Timestamp) -> Inserted {
        self.stats.received = self.stats.received.saturating_add(1);
        let seq = packet.seq;
        let stored = Stored {
            payload: packet.payload,
            level: packet.level,
        };

        if let Some(next) = self.next {
            // Far ahead of playout: the stream jumped (the publisher restarted, or a long
            // outage). Start again rather than conceal across it.
            let ahead_limit = u64::try_from(self.config.max_buffered).unwrap_or(u64::MAX);
            if seq > next.saturating_add(ahead_limit) {
                return self.resync(seq, stored, now);
            }
            if seq < next {
                // A frame concealed in the current dry spell: step back and play it. The
                // delay grows by what was concealed, which is what the network now needs.
                if self.dry_since.is_some_and(|dry| seq >= dry) {
                    if self.buffer.contains_key(&seq) {
                        self.stats.duplicate = self.stats.duplicate.saturating_add(1);
                        return Inserted::Duplicate;
                    }
                    self.observe(seq, now);
                    self.buffer.insert(seq, stored);
                    self.next = Some(seq);
                    self.late_in_a_row = 0;
                    self.stats.stepped_back = self.stats.stepped_back.saturating_add(1);
                    return Inserted::Buffered;
                }
                self.observe(seq, now);
                self.stats.late = self.stats.late.saturating_add(1);
                self.late_in_a_row = self.late_in_a_row.saturating_add(1);
                if self.late_in_a_row >= self.config.resync_after_late {
                    return self.resync(seq, stored, now);
                }
                return Inserted::Late;
            }
        }

        if self.buffer.contains_key(&seq) {
            self.stats.duplicate = self.stats.duplicate.saturating_add(1);
            return Inserted::Duplicate;
        }
        self.observe(seq, now);
        self.late_in_a_row = 0;
        self.first_arrival.get_or_insert(now);
        self.buffer.insert(seq, stored);
        // Bounded: before playout starts nothing drains the buffer, so the oldest goes.
        while self.buffer.len() > self.config.max_buffered.max(1) {
            self.buffer.pop_first();
        }
        Inserted::Buffered
    }

    fn resync(&mut self, seq: u64, stored: Stored<P>, now: Timestamp) -> Inserted {
        self.restart();
        self.stats.resyncs = self.stats.resyncs.saturating_add(1);
        self.observe(seq, now);
        self.first_arrival = Some(now);
        self.buffer.insert(seq, stored);
        Inserted::Resynced
    }

    /// A frame is due at `now`. Call once per frame interval.
    pub fn pull(&mut self, now: Timestamp) -> Playout<P> {
        let Some(mut next) = self.next else {
            return self.maybe_start(now);
        };

        // Steer the delay in force towards the target, a frame at a time.
        let frame = micros(self.config.frame).max(1);
        let target = i64::from(self.target_frames).saturating_mul(frame);
        let delay = self.delay_in_force(next, now).unwrap_or(target);
        if delay >= target.saturating_add(frame) {
            self.overfull_for = self.overfull_for.saturating_add(1);
        } else {
            self.overfull_for = 0;
        }
        if delay <= target.saturating_sub(frame) {
            self.underfull_for = self.underfull_for.saturating_add(1);
        } else {
            self.underfull_for = 0;
        }

        // Too little delay for a while: play filler and consume nothing. Only when there is
        // a frame to hold back; a dry buffer lengthens the delay by itself.
        if self.underfull_for >= self.config.grow_after && self.buffer.contains_key(&next) {
            self.underfull_for = 0;
            self.stats.held = self.stats.held.saturating_add(1);
            return Playout::Hold;
        }

        // Too much for long enough: skip a frame.
        let far_too_much = delay > target.saturating_add(frame.saturating_mul(10));
        if far_too_much || self.overfull_for >= self.config.shrink_after {
            let quiet = self
                .buffer
                .get(&next)
                .is_none_or(|held| held.level.is_some_and(|l| l <= self.config.quiet_level));
            // Wait for a quiet frame, but not for ever.
            let waited_long = self.overfull_for >= self.config.shrink_after.saturating_mul(2);
            if far_too_much || quiet || waited_long {
                self.buffer.remove(&next);
                next = next.saturating_add(1);
                self.next = Some(next);
                self.overfull_for = 0;
                self.stats.skipped = self.stats.skipped.saturating_add(1);
            }
        }

        // Frames before the playhead are of no more use.
        self.buffer = self.buffer.split_off(&next);
        self.next = Some(next.saturating_add(1));

        if let Some(frame) = self.buffer.remove(&next) {
            self.dry_since = None;
            self.concealed_in_a_row = 0;
            self.stats.played = self.stats.played.saturating_add(1);
            return Playout::Frame {
                seq: next,
                payload: frame.payload,
            };
        }

        let Some((&later_seq, later)) = self.buffer.first_key_value() else {
            // Nothing at all: late, or silence. Remember where the dry spell began.
            self.dry_since.get_or_insert(next);
            return self.conceal(next);
        };
        // A hole with packets behind it. After a long dry spell it is the end of a silence:
        // the sender sent nothing for these frames, so there is nothing to rebuild.
        self.dry_since = None;
        if self.concealed_in_a_row > self.config.max_conceal_frames {
            return self.conceal(next);
        }
        // Otherwise it is a loss.
        let frames_back = later_seq.saturating_sub(next);
        if self.config.fec && frames_back == 1 {
            self.concealed_in_a_row = 0;
            self.stats.recovered_fec = self.stats.recovered_fec.saturating_add(1);
            return Playout::Fec {
                seq: next,
                next: later.payload.clone(),
            };
        }
        if frames_back <= u64::from(self.config.dred_frames) {
            self.concealed_in_a_row = 0;
            self.stats.recovered_dred = self.stats.recovered_dred.saturating_add(1);
            return Playout::Dred {
                seq: next,
                later: later.payload.clone(),
                frames_back: u32::try_from(frames_back).unwrap_or(u32::MAX),
            };
        }
        self.conceal(next)
    }

    fn conceal(&mut self, seq: u64) -> Playout<P> {
        self.concealed_in_a_row = self.concealed_in_a_row.saturating_add(1);
        if self.concealed_in_a_row > self.config.max_conceal_frames {
            self.stats.silent = self.stats.silent.saturating_add(1);
            Playout::Silence { seq }
        } else {
            self.stats.concealed = self.stats.concealed.saturating_add(1);
            Playout::Conceal { seq }
        }
    }

    /// Playout has not begun: begin once the buffer has filled to the target delay.
    fn maybe_start(&mut self, now: Timestamp) -> Playout<P> {
        let (Some(first), Some((&lowest, _))) = (self.first_arrival, self.buffer.first_key_value())
        else {
            return Playout::Waiting;
        };
        let waited = now.saturating_since(first) >= self.target_delay();
        if !waited && self.buffer.len() < self.target_frames as usize {
            return Playout::Waiting;
        }
        self.next = Some(lowest);
        self.pull(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_MS: u64 = 20;

    fn at(ms: u64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn packet(seq: u64) -> Packet<Vec<u8>> {
        Packet {
            seq,
            payload: seq.to_be_bytes().to_vec(),
            level: Some(60),
        }
    }

    /// A sender emitting frame `seq` at `seq * 20 ms`, a network, and a receiver pulling
    /// every 20 ms.
    struct Call {
        buffer: JitterBuffer<Vec<u8>>,
        /// (arrival time, packet), unsorted.
        in_flight: Vec<(u64, Packet<Vec<u8>>)>,
        played: Vec<Playout<Vec<u8>>>,
        now_ms: u64,
        level: fn(u64) -> u8,
    }

    impl Call {
        fn new(config: Config) -> Self {
            Self {
                buffer: JitterBuffer::new(config),
                in_flight: vec![],
                played: vec![],
                now_ms: 0,
                level: |_| 60,
            }
        }

        /// Send frames `seqs` through a network where frame `seq` takes `delay_ms(seq)`, or
        /// is lost if that is `None`. Then run the receiver until the last of them was sent,
        /// so the run ends while packets are still arriving.
        fn run(&mut self, seqs: core::ops::Range<u64>, delay_ms: impl Fn(u64) -> Option<u64>) {
            let until_ms = seqs.end * FRAME_MS;
            for seq in seqs {
                if let Some(delay) = delay_ms(seq) {
                    let packet = Packet {
                        level: Some((self.level)(seq)),
                        ..packet(seq)
                    };
                    self.in_flight.push((seq * FRAME_MS + delay, packet));
                }
            }
            while self.now_ms < until_ms {
                // Deliver everything that has arrived by now, in arrival order.
                self.in_flight.sort_by_key(|(arrival, p)| (*arrival, p.seq));
                while self
                    .in_flight
                    .first()
                    .is_some_and(|(arrival, _)| *arrival <= self.now_ms)
                {
                    let (arrival, p) = self.in_flight.remove(0);
                    self.buffer.insert(p, at(arrival));
                }
                self.played.push(self.buffer.pull(at(self.now_ms)));
                self.now_ms += FRAME_MS;
            }
        }

        /// The frames decoded, in the order played.
        fn frames(&self) -> Vec<u64> {
            self.played
                .iter()
                .filter_map(|p| match p {
                    Playout::Frame { seq, .. } => Some(*seq),
                    _ => None,
                })
                .collect()
        }

        /// What was played in place of frames that were not there, in order.
        fn repairs(&self) -> Vec<(&'static str, u64)> {
            self.played
                .iter()
                .filter_map(|p| match p {
                    Playout::Fec { seq, .. } => Some(("fec", *seq)),
                    Playout::Dred { seq, .. } => Some(("dred", *seq)),
                    Playout::Conceal { seq } => Some(("conceal", *seq)),
                    Playout::Silence { seq } => Some(("silence", *seq)),
                    _ => None,
                })
                .collect()
        }

        /// The pull at which frame `seq` was decoded.
        fn played_at(&self, wanted: u64) -> usize {
            self.played
                .iter()
                .position(|p| matches!(p, Playout::Frame { seq, .. } if *seq == wanted))
                .unwrap()
        }

        /// How long after it was sent the newest frame played, in milliseconds.
        fn latency_ms(&self) -> u64 {
            let newest = *self.frames().last().unwrap();
            self.played_at(newest) as u64 * FRAME_MS - newest * FRAME_MS
        }
    }

    /// Whether `frames` is exactly `first..=last`, each once and in order.
    fn complete(frames: &[u64]) -> bool {
        frames.windows(2).all(|w| w[1] == w[0] + 1)
    }

    /// A network where every tenth packet takes 150 ms longer than the rest.
    fn spiky(seq: u64) -> Option<u64> {
        Some(if seq.is_multiple_of(10) { 180 } else { 30 })
    }

    fn steady(_: u64) -> Option<u64> {
        Some(30)
    }

    #[test]
    fn nothing_is_played_until_something_arrives() {
        let mut b: JitterBuffer<Vec<u8>> = JitterBuffer::new(Config::default());
        assert_eq!(b.pull(at(0)), Playout::Waiting);
        assert_eq!(b.pull(at(20)), Playout::Waiting);
        assert_eq!(b.depth(), 0);
    }

    #[test]
    fn on_a_steady_network_every_frame_plays_in_order_at_the_least_delay() {
        let mut call = Call::new(Config::default());
        call.run(0..500, steady);
        let frames = call.frames();
        assert!(complete(&frames) && frames[0] == 0 && frames.len() >= 497);
        let stats = call.buffer.stats();
        assert_eq!(
            (stats.late, stats.concealed, stats.skipped, stats.held),
            (0, 0, 0, 0)
        );
        assert_eq!(call.buffer.target_delay(), ms(40));
        // Frame 0 arrived at 30 ms and played once a second frame was behind it, at 60 ms.
        assert_eq!(call.played_at(0), 3);
        assert_eq!(call.latency_ms(), 60);
    }

    #[test]
    fn packets_out_of_order_are_played_in_order() {
        let mut call = Call::new(Config::default());
        // Every fourth packet is held back 30 ms, arriving after the one that follows it.
        call.run(0..300, |seq| {
            Some(if seq.is_multiple_of(4) { 55 } else { 25 })
        });
        let frames = call.frames();
        assert!(complete(&frames) && frames.len() >= 295, "{}", frames.len());
        assert_eq!(call.buffer.stats().late, 0);
    }

    #[test]
    fn a_lost_frame_is_rebuilt_from_the_next_packets_fec() {
        let mut call = Call::new(Config::default());
        call.run(0..100, |seq| (seq != 40).then_some(30));
        assert_eq!(call.repairs(), [("fec", 40)]);
        assert!(call.played.contains(&Playout::Fec {
            seq: 40,
            next: packet(41).payload
        }));
        assert_eq!(call.buffer.stats().recovered_fec, 1);
    }

    #[test]
    fn a_burst_of_losses_is_rebuilt_from_a_later_packets_dred() {
        // The delay is raised so that the packet after the burst is in the buffer in time.
        let config = Config {
            min_delay_frames: 6,
            ..Config::default()
        };
        let mut call = Call::new(config);
        call.run(0..100, |seq| (!(40..44).contains(&seq)).then_some(30));
        // The last of the four is next to packet 44, so FEC serves it.
        assert_eq!(
            call.repairs(),
            [("dred", 40), ("dred", 41), ("dred", 42), ("fec", 43)]
        );
        assert!(call.played.contains(&Playout::Dred {
            seq: 40,
            later: packet(44).payload,
            frames_back: 4
        }));
        assert!(call.played.contains(&Playout::Dred {
            seq: 42,
            later: packet(44).payload,
            frames_back: 2
        }));
    }

    #[test]
    fn without_fec_or_dred_a_loss_is_concealed() {
        let config = Config {
            fec: false,
            dred_frames: 0,
            ..Config::default()
        };
        let mut call = Call::new(config);
        call.run(0..100, |seq| (seq != 40).then_some(30));
        assert_eq!(call.repairs(), [("conceal", 40)]);
    }

    #[test]
    fn a_loss_too_far_from_the_next_packet_for_dred_is_concealed() {
        let config = Config {
            dred_frames: 3,
            min_delay_frames: 8,
            ..Config::default()
        };
        let mut call = Call::new(config);
        call.run(0..100, |seq| (!(40..46).contains(&seq)).then_some(30));
        // 40, 41 and 42 are 6, 5 and 4 frames before packet 46: out of reach.
        assert_eq!(
            call.repairs(),
            [
                ("conceal", 40),
                ("conceal", 41),
                ("conceal", 42),
                ("dred", 43),
                ("dred", 44),
                ("fec", 45)
            ]
        );
    }

    #[test]
    fn when_the_network_gets_worse_the_delay_grows_until_nothing_is_late() {
        let mut call = Call::new(Config::default());
        call.run(0..200, steady);
        assert_eq!(call.latency_ms(), 60);
        // From now on every tenth packet takes 150 ms longer. Give it four seconds to adapt.
        call.run(200..400, spiky);
        let adapted = call.buffer.stats();
        // 150 ms of spread is 8 frames, plus one.
        assert_eq!(call.buffer.target_delay(), ms(180));
        assert!((6..=8).contains(&adapted.held), "{adapted:?}");
        // It takes about eight spikes for them to show in the 95th percentile, then seven
        // frames of filler; the packets late in the meantime are covered by FEC.
        assert!(adapted.late <= 12, "{adapted:?}");
        assert_eq!(adapted.recovered_fec, adapted.late, "{adapted:?}");
        // After that nothing is late and nothing needs repair.
        call.run(400..1200, spiky);
        let stats = call.buffer.stats();
        assert_eq!(stats.late, adapted.late, "{stats:?}");
        assert_eq!(stats.recovered_fec, adapted.recovered_fec, "{stats:?}");
        assert_eq!(stats.concealed + stats.silent, 0, "{stats:?}");
        assert_eq!(stats.held, adapted.held, "{stats:?}");
        assert!(
            (200..=220).contains(&call.latency_ms()),
            "{}",
            call.latency_ms()
        );
    }

    #[test]
    fn packets_held_up_together_are_all_played_once_they_arrive() {
        let mut call = Call::new(Config::default());
        // The network stalls for 200 ms: frames 100 to 109 all arrive with frame 110.
        let stall = |seq: u64| {
            Some(if (100..110).contains(&seq) {
                (110 - seq) * FRAME_MS + 30
            } else {
                30
            })
        };
        call.run(0..400, stall);
        let stats = call.buffer.stats();
        assert_eq!(stats.late, 0, "{stats:?}");
        assert_eq!(stats.stepped_back, 1, "{stats:?}");
        let frames = call.frames();
        // Nothing before or during the stall was lost.
        let through_stall: Vec<u64> = frames.iter().copied().filter(|s| *s <= 110).collect();
        assert!(complete(&through_stall) && through_stall.len() == 111);
        // The listener heard a gap while nothing was arriving.
        assert!(
            (8..=11).contains(&(stats.concealed + stats.silent)),
            "{stats:?}"
        );
    }

    #[test]
    fn when_the_network_recovers_the_delay_comes_back_down_slowly() {
        let mut call = Call::new(Config::default());
        call.run(0..500, spiky);
        assert_eq!(call.buffer.target_delay(), ms(180));
        assert!(call.latency_ms() >= 200);
        let before = call.buffer.stats();
        // Steady again. The history takes 3 s to forget the spikes; then the target drops a
        // frame every 2 s, and the delay follows it down a skipped frame at a time.
        call.run(500..1000, steady);
        assert!(
            (ms(80)..=ms(140)).contains(&call.buffer.target_delay()),
            "{:?}",
            call.buffer.target_delay()
        );
        call.run(1000..2500, steady);
        assert_eq!(call.buffer.target_delay(), ms(40));
        let after = call.buffer.stats();
        let skipped = after.skipped - before.skipped;
        assert!((6..=8).contains(&skipped), "skipped {skipped}");
        assert_eq!(after.late, before.late);
        assert!(call.latency_ms() <= 80, "{}", call.latency_ms());
    }

    #[test]
    fn quiet_frames_are_the_ones_skipped() {
        let mut call = Call::new(Config::default());
        // One frame in five is quiet.
        call.level = |seq| if seq.is_multiple_of(5) { 5 } else { 90 };
        call.run(0..500, spiky);
        let first_after = call.frames().len();
        call.run(500..2500, steady);
        let frames = call.frames();
        let skipped: Vec<u64> = frames[first_after..]
            .windows(2)
            .filter(|w| w[1] != w[0] + 1)
            .map(|w| w[0] + 1)
            .collect();
        assert!(skipped.len() >= 6, "{skipped:?}");
        assert!(
            skipped.iter().all(|seq| seq.is_multiple_of(5)),
            "{skipped:?}"
        );
    }

    #[test]
    fn a_delay_far_above_the_target_is_cut_at_once() {
        let config = Config {
            max_delay_frames: 6,
            ..Config::default()
        };
        let mut call = Call::new(config);
        // A path with 600 ms of delay is replaced by one with 30 ms: thirty frames arrive
        // almost together, and the audio in hand is suddenly 600 ms behind.
        call.run(0..100, |_| Some(600));
        call.run(100..200, steady);
        let stats = call.buffer.stats();
        assert!(stats.skipped >= 12, "{stats:?}");
        // Within the limit of ten frames over the 120 ms target, straight away.
        assert!(call.latency_ms() <= 120 + 200 + 40, "{}", call.latency_ms());
        // And all the way down, given time.
        call.run(200..1500, steady);
        assert!(call.latency_ms() <= 80, "{}", call.latency_ms());
    }

    #[test]
    fn silence_from_the_sender_is_played_as_concealment_then_silence_and_speech_resumes_on_time() {
        let mut call = Call::new(Config::default());
        // Speech, two seconds with nothing sent (the counter keeps advancing), speech.
        call.run(0..300, |seq| (!(100..200).contains(&seq)).then_some(30));
        let stats = call.buffer.stats();
        assert_eq!((stats.concealed, stats.silent), (5, 95), "{stats:?}");
        assert_eq!(
            (stats.late, stats.skipped, stats.held),
            (0, 0, 0),
            "{stats:?}"
        );
        // Frame 200 played when it was due: 101 frame intervals after frame 99 did.
        assert_eq!(call.played_at(200) - call.played_at(99), 101);
        assert_eq!(call.buffer.target_delay(), ms(40));
    }

    #[test]
    fn a_duplicate_is_ignored_and_a_late_packet_is_counted() {
        let mut call = Call::new(Config::default());
        call.run(0..20, steady);
        // Frame 5 has long played; frame 17 arrived 30 ms ago and is waiting.
        assert_eq!(call.buffer.insert(packet(5), at(400)), Inserted::Late);
        assert_eq!(call.buffer.insert(packet(17), at(400)), Inserted::Duplicate);
        let stats = call.buffer.stats();
        assert_eq!((stats.late, stats.duplicate, stats.received), (1, 1, 20));
    }

    #[test]
    fn a_frame_rebuilt_as_lost_is_late_if_it_turns_up_after_all() {
        let mut call = Call::new(Config::default());
        call.run(0..100, |seq| Some(if seq == 40 { 530 } else { 30 }));
        assert_eq!(call.repairs(), [("fec", 40)]);
        assert_eq!(call.buffer.stats().late, 1);
        assert!(!call.frames().contains(&40));
    }

    #[test]
    fn a_stream_that_jumps_far_ahead_starts_again() {
        let mut call = Call::new(Config::default());
        call.run(0..50, steady);
        let far = 1_000_000;
        assert_eq!(
            call.buffer.insert(packet(far), at(1000)),
            Inserted::Resynced
        );
        assert_eq!(call.buffer.depth(), 1);
        assert_eq!(call.buffer.pull(at(1000)), Playout::Waiting);
        call.buffer.insert(packet(far + 1), at(1020));
        assert!(matches!(
            call.buffer.pull(at(1020)),
            Playout::Frame { seq, .. } if seq == far
        ));
        assert_eq!(call.buffer.stats().resyncs, 1);
    }

    #[test]
    fn a_stream_that_jumps_back_starts_again_after_a_run_of_late_packets() {
        let mut call = Call::new(Config::default());
        call.run(1000..1050, steady);
        // The publisher restarted its counter.
        let outcomes: Vec<Inserted> = (0..12)
            .map(|seq| call.buffer.insert(packet(seq), at(21_000 + seq * FRAME_MS)))
            .collect();
        assert_eq!(outcomes[..9], [Inserted::Late; 9]);
        assert_eq!(outcomes[9], Inserted::Resynced);
        assert_eq!(outcomes[10..], [Inserted::Buffered, Inserted::Buffered]);
    }

    #[test]
    fn the_buffer_is_bounded_when_nothing_is_pulled() {
        let mut b: JitterBuffer<Vec<u8>> = JitterBuffer::new(Config::default());
        for seq in 0..10_000 {
            b.insert(packet(seq), at(seq * FRAME_MS));
        }
        assert_eq!(b.depth(), 100);
        // The newest are kept.
        assert!(matches!(
            b.pull(at(200_000)),
            Playout::Frame { seq, .. } if seq >= 9_900
        ));
    }

    #[test]
    fn the_delay_never_leaves_its_limits() {
        let config = Config {
            min_delay_frames: 3,
            max_delay_frames: 6,
            ..Config::default()
        };
        let mut call = Call::new(config);
        assert_eq!(call.buffer.target_delay(), ms(60));
        // Spikes of a full second.
        call.run(0..500, |seq| {
            Some(if seq.is_multiple_of(7) { 1030 } else { 30 })
        });
        assert_eq!(call.buffer.target_delay(), ms(120));
        call.run(500..3000, steady);
        assert_eq!(call.buffer.target_delay(), ms(60));
    }

    #[test]
    fn on_a_lossy_jittery_network_most_of_what_is_lost_is_rebuilt() {
        let run = || {
            let mut call = Call::new(Config::default());
            // 5% loss and 20 to 110 ms of delay, from a fixed pattern.
            let network = |seq: u64| {
                let mixed = seq.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 40;
                (!mixed.is_multiple_of(20)).then_some(20 + mixed % 90)
            };
            call.run(0..3000, network);
            let latency = call.latency_ms();
            (call.played, call.buffer.stats(), latency)
        };
        let (played, stats, latency) = run();
        // The same arrivals give the same playout.
        assert_eq!(run().0, played);
        let lost = 3000 - stats.received;
        assert!((120..=180).contains(&lost), "{lost} lost");
        assert!(stats.late <= 15, "{stats:?}");
        assert!(
            stats.recovered_fec + stats.recovered_dred >= lost - 5,
            "{stats:?}"
        );
        assert!(stats.concealed + stats.silent <= 15, "{stats:?}");
        // 90 ms of jitter needs 5 frames plus one; the delay in force is close to that.
        assert!((100..=160).contains(&latency), "{latency} ms");
    }

    #[test]
    fn a_shared_payload_is_handed_back_without_being_copied() {
        use std::rc::Rc;
        // Stands for a reference-counted slice of a received datagram.
        let mut b: JitterBuffer<Rc<[u8]>> = JitterBuffer::new(Config::default());
        let payloads: Vec<Rc<[u8]>> = (0..6u8).map(|n| Rc::from(vec![n; 80])).collect();
        for (seq, payload) in payloads.iter().enumerate() {
            if seq == 2 {
                continue; // lost
            }
            let packet = Packet {
                seq: seq as u64,
                payload: Rc::clone(payload),
                level: None,
            };
            b.insert(packet, at(seq as u64 * FRAME_MS));
        }
        let mut out = vec![];
        for tick in 0..6 {
            out.push(b.pull(at(100 + tick * FRAME_MS)));
        }
        // Frame 3's packet came out twice, for frame 2's FEC and for itself, and both are
        // the very allocation that went in.
        let Playout::Fec { seq: 2, next } = &out[2] else {
            panic!("{:?}", out[2])
        };
        let Playout::Frame { seq: 3, payload } = &out[3] else {
            panic!("{:?}", out[3])
        };
        assert!(Rc::ptr_eq(next, &payloads[3]) && Rc::ptr_eq(payload, &payloads[3]));
    }
}
