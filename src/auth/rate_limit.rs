use std::collections::HashMap;
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Rate limiter with a linear delay ramp for failed attempts.
///
/// - First 2 failures: no delay
/// - After that: 500ms × (count - 2), capped at 5s
/// - Window: per-minute (failures older than 1 min are forgotten)
///
/// Generic over the key type so the same machinery can throttle by IP
/// address (login) or by user id (password change, etc.).
pub struct RateLimiter<K: Eq + Hash + Clone = IpAddr> {
    state: Mutex<HashMap<K, KeyState>>,
    max_per_minute: u32,
}

struct KeyState {
    attempts: Vec<Instant>,
    failures: u32,
    last_failure: Option<Instant>,
}

impl<K: Eq + Hash + Clone> RateLimiter<K> {
    pub fn new(max_per_minute: u32) -> Self {
        RateLimiter {
            state: Mutex::new(HashMap::new()),
            max_per_minute,
        }
    }

    /// Check if a request from this key should be allowed.
    /// Returns Ok(delay) with the delay to impose, or Err if rate limited.
    pub fn check(&self, key: K) -> Result<Duration, ()> {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let window = Duration::from_secs(60);

        let entry = state.entry(key).or_insert(KeyState {
            attempts: Vec::new(),
            failures: 0,
            last_failure: None,
        });

        // Prune old attempts
        entry.attempts.retain(|t| now.duration_since(*t) < window);

        // Reset failure count if last failure was more than 1 minute ago
        if let Some(last) = entry.last_failure {
            if now.duration_since(last) > window {
                entry.failures = 0;
            }
        }

        if entry.attempts.len() >= self.max_per_minute as usize {
            return Err(());
        }

        entry.attempts.push(now);

        // Calculate delay based on failure count
        let delay = if entry.failures <= 2 {
            Duration::ZERO
        } else {
            let ms = 500 * (entry.failures - 2) as u64;
            Duration::from_millis(ms.min(5000))
        };

        Ok(delay)
    }

    /// Record a failed attempt from this key.
    pub fn record_failure(&self, key: K) {
        let mut state = self.state.lock().unwrap();
        let entry = state.entry(key).or_insert(KeyState {
            attempts: Vec::new(),
            failures: 0,
            last_failure: None,
        });
        entry.failures += 1;
        entry.last_failure = Some(Instant::now());
    }

    /// Reset failure count for a key (on successful operation).
    pub fn reset(&self, key: &K) {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.get_mut(key) {
            entry.failures = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_allows_within_limit() {
        let limiter: RateLimiter = RateLimiter::new(5);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        for _ in 0..5 {
            assert!(limiter.check(ip).is_ok());
        }
        // 6th should be denied
        assert!(limiter.check(ip).is_err());
    }

    #[test]
    fn test_delay_ramp() {
        let limiter: RateLimiter = RateLimiter::new(100);
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

        // No delay for first 2 failures
        assert_eq!(limiter.check(ip).unwrap(), Duration::ZERO);
        limiter.record_failure(ip);
        assert_eq!(limiter.check(ip).unwrap(), Duration::ZERO);
        limiter.record_failure(ip);
        assert_eq!(limiter.check(ip).unwrap(), Duration::ZERO);

        // 3rd failure starts delay
        limiter.record_failure(ip);
        assert_eq!(limiter.check(ip).unwrap(), Duration::from_millis(500));

        // 4th failure increases delay
        limiter.record_failure(ip);
        assert_eq!(limiter.check(ip).unwrap(), Duration::from_millis(1000));

        // Cap at 5s
        for _ in 0..20 {
            limiter.record_failure(ip);
        }
        assert!(limiter.check(ip).unwrap() <= Duration::from_millis(5000));
    }

    #[test]
    fn test_reset_clears_failures() {
        let limiter: RateLimiter = RateLimiter::new(100);
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        for _ in 0..5 {
            limiter.record_failure(ip);
        }
        assert!(limiter.check(ip).unwrap() > Duration::ZERO);

        limiter.reset(&ip);
        assert_eq!(limiter.check(ip).unwrap(), Duration::ZERO);
    }

    #[test]
    fn test_works_with_string_keys() {
        let limiter: RateLimiter<String> = RateLimiter::new(3);
        let user = "alice".to_string();
        for _ in 0..3 {
            assert!(limiter.check(user.clone()).is_ok());
        }
        assert!(limiter.check(user.clone()).is_err());

        // Different key, fresh budget.
        let other = "bob".to_string();
        assert!(limiter.check(other).is_ok());
    }
}
