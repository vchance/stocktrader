use chrono::DateTime;
use st_core::{Clock, Timestamp};

/// A clock the backtester advances to each event's `available_at`.
pub struct SimClock {
    now: Timestamp,
}

impl SimClock {
    pub fn new() -> Self {
        Self { now: DateTime::UNIX_EPOCH }
    }

    pub fn set(&mut self, now: Timestamp) {
        self.now = now;
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SimClock {
    fn now(&self) -> Timestamp {
        self.now
    }
}
