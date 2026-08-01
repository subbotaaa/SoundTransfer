//! Простая помощь для вывода статистики раз в секунду.

use std::time::{Duration, Instant};

pub struct EverySecond {
    last: Instant,
    period: Duration,
}

impl EverySecond {
    pub fn new() -> Self {
        Self {
            last: Instant::now(),
            period: Duration::from_secs(1),
        }
    }

    /// true — пора печатать очередную строку статистики.
    pub fn tick(&mut self) -> bool {
        if self.last.elapsed() >= self.period {
            self.last = Instant::now();
            true
        } else {
            false
        }
    }
}
