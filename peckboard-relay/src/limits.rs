//! Rate limiting and privacy-preserving IP tags for logs.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

use sha2::{Digest, Sha256};

/// Collapse IPv6 to its /64 so one host can't dodge per-IP limits by
/// rotating through its prefix.
pub fn ip_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
    }
}

/// Short salted hash of an IP for logs: lets an operator correlate lines
/// within one process lifetime without ever writing a full address.
pub fn ip_tag(salt: &[u8; 16], ip: IpAddr) -> String {
    let mut h = Sha256::new();
    h.update(salt);
    match ip_key(ip) {
        IpAddr::V4(v) => h.update(v.octets()),
        IpAddr::V6(v) => h.update(v.octets()),
    }
    let d = h.finalize();
    d[..4].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token bucket per IP (v6 collapsed to /64) plus one global bucket. The
/// per-IP map is capped; when full, stale entries are pruned and, failing
/// that, unseen IPs share the global bucket only.
pub struct RateLimiter {
    per_ip_rate: f64,
    per_ip_burst: f64,
    global_rate: f64,
    global_burst: f64,
    max_entries: usize,
    inner: Mutex<(HashMap<IpAddr, Bucket>, Bucket)>,
}

impl RateLimiter {
    pub fn new(
        per_ip_per_sec: f64,
        per_ip_burst: f64,
        global_per_sec: f64,
        global_burst: f64,
    ) -> Self {
        let now = Instant::now();
        Self {
            per_ip_rate: per_ip_per_sec,
            per_ip_burst,
            global_rate: global_per_sec,
            global_burst,
            max_entries: 65_536,
            inner: Mutex::new((
                HashMap::new(),
                Bucket {
                    tokens: global_burst,
                    last: now,
                },
            )),
        }
    }

    fn refill(b: &mut Bucket, rate: f64, burst: f64, now: Instant) {
        let dt = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + dt * rate).min(burst);
        b.last = now;
    }

    pub fn allow(&self, ip: IpAddr) -> bool {
        self.allow_cost(ip, 1.0)
    }

    /// [`allow`](Self::allow) for an event that costs `cost` tokens (e.g.
    /// bytes); nothing is charged when it is refused.
    pub fn allow_cost(&self, ip: IpAddr, cost: f64) -> bool {
        let now = Instant::now();
        let key = ip_key(ip);
        let mut g = self.inner.lock().unwrap();
        let (map, global) = &mut *g;
        Self::refill(global, self.global_rate, self.global_burst, now);
        if global.tokens < cost {
            return false;
        }
        if !map.contains_key(&key) && map.len() >= self.max_entries {
            let (rate, burst) = (self.per_ip_rate, self.per_ip_burst);
            map.retain(|_, b| {
                Self::refill(b, rate, burst, now);
                b.tokens < burst
            });
        }
        if let Some(b) = map.get_mut(&key) {
            Self::refill(b, self.per_ip_rate, self.per_ip_burst, now);
            if b.tokens < cost {
                return false;
            }
            b.tokens -= cost;
        } else if map.len() < self.max_entries {
            if self.per_ip_burst < cost {
                return false;
            }
            map.insert(
                key,
                Bucket {
                    tokens: self.per_ip_burst - cost,
                    last: now,
                },
            );
        }
        global.tokens -= cost;
        true
    }

    /// Drop buckets that have fully refilled (called periodically).
    pub fn prune(&self) {
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap();
        let (rate, burst) = (self.per_ip_rate, self.per_ip_burst);
        g.0.retain(|_, b| {
            Self::refill(b, rate, burst, now);
            b.tokens < burst
        });
    }
}

/// One token bucket (e.g. the bytes one rendezvous id may relay).
#[derive(Clone, Copy)]
pub struct TokenBucket {
    b: Bucket,
}

impl TokenBucket {
    /// Starts full.
    pub fn new(burst: f64) -> Self {
        Self {
            b: Bucket {
                tokens: burst,
                last: Instant::now(),
            },
        }
    }

    /// Take `cost` tokens if available.
    pub fn take(&mut self, rate: f64, burst: f64, cost: f64) -> bool {
        RateLimiter::refill(&mut self.b, rate, burst, Instant::now());
        if self.b.tokens < cost {
            return false;
        }
        self.b.tokens -= cost;
        true
    }
}

/// Concurrent-connection counts per IP.
#[derive(Default)]
pub struct ConnCounter {
    inner: Mutex<HashMap<IpAddr, usize>>,
}

impl ConnCounter {
    pub fn try_acquire(&self, ip: IpAddr, max: usize) -> bool {
        let mut m = self.inner.lock().unwrap();
        let c = m.entry(ip_key(ip)).or_insert(0);
        if *c >= max {
            return false;
        }
        *c += 1;
        true
    }

    pub fn release(&self, ip: IpAddr) {
        let mut m = self.inner.lock().unwrap();
        let k = ip_key(ip);
        if let Some(c) = m.get_mut(&k) {
            *c -= 1;
            if *c == 0 {
                m.remove(&k);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_ip_bucket_exhausts() {
        let rl = RateLimiter::new(0.0, 3.0, 1000.0, 1000.0);
        let a: IpAddr = "198.51.100.1".parse().unwrap();
        let b: IpAddr = "198.51.100.2".parse().unwrap();
        assert!((0..3).all(|_| rl.allow(a)));
        assert!(!rl.allow(a));
        assert!(rl.allow(b));
        let v6a: IpAddr = "2001:db8::1".parse().unwrap();
        let v6b: IpAddr = "2001:db8::ffff".parse().unwrap();
        assert_eq!(ip_key(v6a), ip_key(v6b));
    }
}
