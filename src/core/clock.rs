//! Time, behind a trait.
//!
//! Every timestamp MinWin persists comes from a `Clock`. Tests substitute a
//! fixed clock so that recorded sessions, and the rendered output derived from
//! them, are byte-for-byte deterministic.

use chrono::{DateTime, TimeZone, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that returns one fixed instant, optionally advancing by a fixed step
/// on each read so that ordered events still get ordered timestamps.
#[derive(Debug)]
pub struct FixedClock {
    start: DateTime<Utc>,
    step_seconds: i64,
    reads: std::sync::atomic::AtomicI64,
}

impl FixedClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            start,
            step_seconds: 0,
            reads: std::sync::atomic::AtomicI64::new(0),
        }
    }

    pub fn stepping(start: DateTime<Utc>, step_seconds: i64) -> Self {
        Self {
            start,
            step_seconds,
            reads: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// `2026-01-01T00:00:00Z`, the instant the test suite pretends it is.
    pub fn epoch() -> Self {
        Self::new(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        let reads = self
            .reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.start + chrono::Duration::seconds(reads * self.step_seconds)
    }
}

/// Renders a timestamp in the local time zone as MinWin's CLI shows it.
pub fn format_local(value: DateTime<Utc>) -> String {
    value
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_clock_does_not_move() {
        let clock = FixedClock::epoch();
        assert_eq!(clock.now(), clock.now());
    }

    #[test]
    fn a_stepping_clock_orders_successive_reads() {
        let clock = FixedClock::stepping(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(), 5);
        let first = clock.now();
        let second = clock.now();
        assert_eq!((second - first).num_seconds(), 5);
    }
}
