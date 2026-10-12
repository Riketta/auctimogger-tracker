//! Wall-clock adapter.

use amtracker_core::ports::ClockPort;

/// `SystemTime`-backed clock (unix seconds).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl ClockPort for SystemClock {
    fn now(&self) -> i64 {
        // `u64` unix seconds only exceed `i64` ~292 billion years after the
        // epoch, so the conversion saturates instead of wrapping; a clock
        // before the epoch reads as 0.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use amtracker_core::domain::utc_day;

    use super::*;

    #[test]
    fn now_is_plausibly_current() {
        let now = SystemClock.now();
        // 2026-01-01 .. 2100-01-01 sanity window.
        assert!((1_767_225_600..4_107_542_400).contains(&now));
    }

    #[test]
    fn utc_day_divides_at_day_boundaries() {
        assert_eq!(utc_day(0), 0);
        assert_eq!(utc_day(86_399), 0);
        assert_eq!(utc_day(86_400), 1);
        assert_eq!(utc_day(-1), -1);
    }
}
