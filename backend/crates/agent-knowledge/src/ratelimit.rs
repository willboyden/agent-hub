//! Port of ratelimit.py: a sliding-window rate limiter per token id, in memory, with an injectable monotonic clock.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

pub type Clock = Box<dyn Fn() -> f64 + Send + Sync>;

pub struct RateLimiter {
    clock: Clock,
    hits: Mutex<HashMap<String, VecDeque<f64>>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        let t0 = Instant::now();
        Self::with_clock(Box::new(move || t0.elapsed().as_secs_f64()))
    }
}

impl RateLimiter {
    pub fn with_clock(clock: Clock) -> Self {
        Self {
            clock,
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// `allow(key, per_minute)`: at most `per_minute` hits in any 60 s window (a hit older than 60 s falls out).
    pub fn allow(&self, key: &str, per_minute: i64) -> bool {
        let now = (self.clock)();
        let Ok(mut hits) = self.hits.lock() else { return false };
        let dq = hits.entry(key.to_string()).or_default();
        while dq.front().is_some_and(|t| now - t > 60.0) {
            dq.pop_front();
        }
        if i64::try_from(dq.len()).unwrap_or(i64::MAX) >= per_minute {
            return false;
        }
        dq.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[test]
    fn window_slides_after_sixty_seconds() {
        let t = Arc::new(AtomicU64::new(0));
        let tc = t.clone();
        #[allow(clippy::cast_precision_loss)]
        let rl = RateLimiter::with_clock(Box::new(move || tc.load(Ordering::SeqCst) as f64));
        assert!(rl.allow("a", 2));
        assert!(rl.allow("a", 2));
        assert!(!rl.allow("a", 2));
        assert!(rl.allow("b", 2)); // per key
        t.store(60, Ordering::SeqCst);
        assert!(!rl.allow("a", 2)); // exactly 60 s old still counts (`> 60.0`)
        t.store(61, Ordering::SeqCst);
        assert!(rl.allow("a", 2));
        assert!(!rl.allow("z", 0));
    }
}
