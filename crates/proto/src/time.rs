//! Time for sans-IO code.
//!
//! Sans-IO code never reads a clock; it is told the time. Two types keep the two meanings of
//! "a time" apart, and keep units out of variable names:
//!
//! - [`Timestamp`] is a point in time: microseconds since the node started (or since the
//!   start of a simulation). It only makes sense relative to another timestamp from the same
//!   clock.
//! - [`Duration`] (the standard one) is a span.
//!
//! A timestamp minus a timestamp is a duration; a timestamp plus a duration is a timestamp.
//! Both operations saturate rather than overflow or go negative.

use core::ops::{Add, Sub};
use core::time::Duration;

/// A point in time on the caller's monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(u64);

impl Timestamp {
    /// The clock's origin.
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis.saturating_mul(1000))
    }

    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.0
    }

    /// How long after `earlier` this is; zero if it is not after it.
    #[must_use]
    pub const fn saturating_since(self, earlier: Self) -> Duration {
        Duration::from_micros(self.0.saturating_sub(earlier.0))
    }

    /// This time moved later by `span`, stopping at the largest representable time.
    #[must_use]
    pub fn saturating_add(self, span: Duration) -> Self {
        Self(
            self.0
                .saturating_add(u64::try_from(span.as_micros()).unwrap_or(u64::MAX)),
        )
    }
}

impl Add<Duration> for Timestamp {
    type Output = Self;

    fn add(self, span: Duration) -> Self {
        self.saturating_add(span)
    }
}

impl Sub for Timestamp {
    type Output = Duration;

    fn sub(self, earlier: Self) -> Duration {
        self.saturating_since(earlier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_minus_a_time_is_a_span_and_a_time_plus_a_span_is_a_time() {
        let (a, b) = (Timestamp::from_millis(100), Timestamp::from_millis(350));
        assert_eq!(b - a, Duration::from_millis(250));
        assert_eq!(a + Duration::from_millis(250), b);
        assert_eq!(b.saturating_since(a), Duration::from_millis(250));
        assert!(a < b);
    }

    #[test]
    fn arithmetic_saturates_in_both_directions() {
        let (a, b) = (Timestamp::from_millis(100), Timestamp::from_millis(350));
        assert_eq!(a - b, Duration::ZERO);
        assert_eq!(
            Timestamp::from_micros(u64::MAX) + Duration::from_secs(1),
            Timestamp::from_micros(u64::MAX)
        );
        assert_eq!(
            Timestamp::ZERO + Duration::MAX,
            Timestamp::from_micros(u64::MAX)
        );
        assert_eq!(
            Timestamp::from_millis(u64::MAX),
            Timestamp::from_micros(u64::MAX)
        );
    }

    #[test]
    fn spans_shorter_than_a_microsecond_are_dropped() {
        assert_eq!(Timestamp::ZERO + Duration::from_nanos(999), Timestamp::ZERO);
        assert_eq!(
            (Timestamp::ZERO + Duration::from_nanos(1500)).as_micros(),
            1
        );
    }
}
