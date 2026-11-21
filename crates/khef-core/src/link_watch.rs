//! Whether the khef at the other end of a link is alive, judged from what it sends.
//!
//! Anything a khef sends on a link shows that its forwarding loop is running: forwarded
//! audio and video, control objects. When it has nothing to send it sends a filler (a `MoQ`
//! padding datagram, which the other end discards), so that silence on a link always means
//! something is wrong. No node floods a heartbeat to the meeting: each watches its own
//! links, and reports a link that dies as a change (design section 5).
//!
//! The transport's own keep-alive is deliberately not used for this. It is answered by the
//! transport's task, which can keep running while the khef itself is stuck.
//!
//! One [`LinkWatch`] per link. Sans-IO: the caller says when something was sent or heard and
//! asks what is due.

use palaver_proto::Timestamp;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkWatchConfig {
    /// Send a filler when nothing has been sent on the link for this long.
    pub filler_after: Duration,
    /// The link is dead when nothing has been heard on it for this long.
    pub dead_after: Duration,
}

impl Default for LinkWatchConfig {
    /// A filler every 200 ms on an idle link; dead after 700 ms of silence, which is three
    /// fillers missed with some room for jitter.
    fn default() -> Self {
        Self {
            filler_after: Duration::from_millis(200),
            dead_after: Duration::from_millis(700),
        }
    }
}

/// Watches one link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkWatch {
    config: LinkWatchConfig,
    last_sent: Timestamp,
    last_heard: Timestamp,
}

impl LinkWatch {
    /// The link came up at `now`.
    #[must_use]
    pub const fn new(config: LinkWatchConfig, now: Timestamp) -> Self {
        Self {
            config,
            last_sent: now,
            last_heard: now,
        }
    }

    /// Something the khef at the other end sent has arrived: an object, a datagram, a
    /// control message or a filler.
    pub fn heard(&mut self, now: Timestamp) {
        self.last_heard = self.last_heard.max(now);
    }

    /// This khef sent something on the link.
    pub fn sent(&mut self, now: Timestamp) {
        self.last_sent = self.last_sent.max(now);
    }

    /// Whether a filler should be sent now. The caller sends one and calls
    /// [`LinkWatch::sent`].
    #[must_use]
    pub fn filler_due(&self, now: Timestamp) -> bool {
        now.saturating_since(self.last_sent) >= self.config.filler_after
    }

    /// Whether the other end has been silent for too long.
    #[must_use]
    pub fn is_dead(&self, now: Timestamp) -> bool {
        now.saturating_since(self.last_heard) >= self.config.dead_after
    }

    /// When to look at this link next if nothing is sent or heard meanwhile.
    #[must_use]
    pub fn next_wake(&self) -> Timestamp {
        let filler = self.last_sent.saturating_add(self.config.filler_after);
        let dead = self.last_heard.saturating_add(self.config.dead_after);
        filler.min(dead)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    fn watch() -> LinkWatch {
        LinkWatch::new(LinkWatchConfig::default(), at(1000))
    }

    #[test]
    fn a_filler_is_due_only_when_nothing_else_was_sent() {
        let mut w = watch();
        assert!(!w.filler_due(at(1199)));
        assert!(w.filler_due(at(1200)));
        // Audio is going out every 20 ms: no filler is ever due.
        for ms in (1000..3000).step_by(20) {
            assert!(!w.filler_due(at(ms)));
            w.sent(at(ms));
        }
        assert!(!w.filler_due(at(3179)));
        assert!(w.filler_due(at(3180)));
    }

    #[test]
    fn a_link_is_dead_after_seven_hundred_milliseconds_of_silence() {
        let mut w = watch();
        assert!(!w.is_dead(at(1699)));
        assert!(w.is_dead(at(1700)));
        w.heard(at(1650));
        assert!(!w.is_dead(at(1700)));
        assert!(w.is_dead(at(2350)));
    }

    #[test]
    fn sending_does_not_keep_a_silent_link_alive() {
        let mut w = watch();
        for ms in (1000..2000).step_by(20) {
            w.sent(at(ms));
        }
        assert!(w.is_dead(at(1700)));
    }

    #[test]
    fn two_idle_ends_keep_each_other_alive_with_fillers_alone() {
        let (mut a, mut b) = (watch(), watch());
        let mut fillers = 0;
        for ms in 1000..11_000 {
            let now = at(ms);
            // Fillers take 30 ms to cross.
            if a.filler_due(now) {
                a.sent(now);
                b.heard(at(ms + 30));
                fillers += 1;
            }
            if b.filler_due(now) {
                b.sent(now);
                a.heard(at(ms + 30));
            }
            assert!(!a.is_dead(now) && !b.is_dead(now), "at {ms} ms");
        }
        assert!((49..=50).contains(&fillers), "five a second: {fillers}");
    }

    #[test]
    fn the_next_wake_is_the_earlier_of_the_two_deadlines() {
        let mut w = watch();
        assert_eq!(w.next_wake(), at(1200));
        w.sent(at(1600));
        assert_eq!(
            w.next_wake(),
            at(1700),
            "the link dies before a filler is due"
        );
        w.heard(at(1650));
        assert_eq!(w.next_wake(), at(1800));
    }

    #[test]
    fn time_going_backwards_is_ignored() {
        let mut w = watch();
        w.heard(at(1500));
        w.heard(at(1100));
        w.sent(at(1500));
        w.sent(at(1100));
        assert!(!w.is_dead(at(2199)));
        assert!(!w.filler_due(at(1699)));
    }
}
