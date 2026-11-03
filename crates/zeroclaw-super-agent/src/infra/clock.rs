//! System clock port implementation. Dates are UTC (no timezone database in
//! M1); the local-date refinement is deferred to the wiring milestone.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::application::ports::Clock;
use crate::domain::{DateYmd, TimestampMs};

pub struct SystemClock;

impl Clock for SystemClock {
    fn today(&self) -> DateYmd {
        DateYmd::from_days_since_epoch((self.now_ms() / 86_400_000) as i64)
    }

    fn now_ms(&self) -> TimestampMs {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as TimestampMs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_is_a_plausible_modern_date() {
        let today = SystemClock.today();
        assert!(today.year >= 2024, "unexpected clock year: {today}");
    }
}
