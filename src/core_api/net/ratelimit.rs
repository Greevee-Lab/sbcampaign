// SPDX-License-Identifier: AGPL-3.0-only
//! Token bucket per provider access.

use std::{collections::HashMap, sync::Mutex, time::Instant};

#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl RateLimiter {
    /// Take one token from the bucket of `name` (rate tokens per second, at
    /// most `burst` stored). False if the bucket is empty.
    pub fn allow(&self, name: &str, rate: f64, burst: f64) -> bool {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let (tokens, last) = buckets.entry(name.to_owned()).or_insert((burst, now));
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * rate).min(burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_refuse_per_access() {
        let l = RateLimiter::default();
        for _ in 0..3 {
            assert!(l.allow("a", 0.001, 3.0));
        }
        assert!(!l.allow("a", 0.001, 3.0));
        assert!(l.allow("b", 0.001, 3.0), "buckets are per access");
    }

    #[test]
    fn refills_over_time() {
        let l = RateLimiter::default();
        assert!(l.allow("a", 1000.0, 1.0));
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(l.allow("a", 1000.0, 1.0));
    }
}
