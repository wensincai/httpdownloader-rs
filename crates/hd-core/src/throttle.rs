//! Token-bucket rate limiting — mirrors the original speed limiter
//! (download_speed_limit in DOWNLOAD_INFO + global limit; the original
//! throttles inside the IO loop on a 1-second tick).

use std::time::{Duration, Instant};

/// Simple byte-bucket limiter: refill `rate` bytes per second, capacity = rate
/// (bounded burst of 1 second). Zero rate = unlimited.
pub struct Throttle {
    rate: u64,
    budget: f64,
    last: Instant,
}

impl Throttle {
    pub fn new(rate: u64) -> Self {
        Throttle { rate, budget: rate as f64, last: Instant::now() }
    }

    pub fn rate(&self) -> u64 {
        self.rate
    }

    pub fn set_rate(&mut self, rate: u64) {
        self.rate = rate;
        if rate > 0 && self.budget > rate as f64 {
            self.budget = rate as f64;
        }
    }

    /// Returns how long the caller must wait before `n` bytes may pass.
    /// 0 duration = go ahead.
    pub fn acquire(&mut self, n: u64) -> Duration {
        if self.rate == 0 {
            return Duration::ZERO;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.budget = (self.budget + elapsed * self.rate as f64).min(self.rate as f64);

        if self.budget >= n as f64 {
            self.budget -= n as f64;
            return Duration::ZERO;
        }
        let deficit = n as f64 - self.budget;
        self.budget = 0.0;
        Duration::from_secs_f64(deficit / self.rate as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_passes() {
        let mut t = Throttle::new(0);
        assert_eq!(t.acquire(u64::MAX), Duration::ZERO);
    }

    #[test]
    fn full_bucket_passes_immediately() {
        let mut t = Throttle::new(1000);
        assert_eq!(t.acquire(1000), Duration::ZERO);
    }

    #[test]
    fn over_bucket_requires_wait() {
        let mut t = Throttle::new(1000);
        t.acquire(600);
        let wait = t.acquire(600); // only 400 left
        assert!(wait.as_millis() >= 150, "expect ~200ms wait, got {:?}", wait);
    }
}
