//! Rate limiting, concurrency caps and privacy-preserving IP tags for logs.
//!
//! Every limit keys on the canonical client address ([`canonical_ip`]) and,
//! for IPv6, on three nested prefixes at once ([`ip_keys`]): a host can't
//! dodge a limit by rotating through its /64, nor a site through its /48.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Share of a per-address limit an IPv6 /56 gets, relative to a /64.
pub const V6_56_SCALE: f64 = 2.0;
/// Share an IPv6 /48 gets, relative to a /64. A /48 is what an attacker gets
/// for free (tunnel brokers), so it is treated like one more address, not
/// 65,536 of them.
pub const V6_48_SCALE: f64 = 4.0;
/// Keys a [`RateLimiter`] tracks before unseen keys share one overflow
/// bucket (bounds memory under spoofed-source floods).
pub const MAX_LIMITER_ENTRIES: usize = 65_536;
/// The overflow bucket gets this many per-address budgets.
pub const OVERFLOW_SCALE: f64 = 16.0;
/// A full limiter table is swept on the request path at most this often;
/// otherwise only by housekeeping.
const PRUNE_INTERVAL: Duration = Duration::from_secs(1);

/// The address every limit and log tag is computed from: v4-mapped
/// (`::ffff:a.b.c.d`), 6to4 (`2002:aabb:ccdd::/48`) and Teredo
/// (`2001:0::/32`) addresses fold to the IPv4 address they embed, so the
/// same client can't appear as several unrelated keys.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    let IpAddr::V6(v6) = ip else {
        return ip;
    };
    if let Some(v4) = v6.to_ipv4_mapped() {
        return IpAddr::V4(v4);
    }
    let s = v6.segments();
    let o = v6.octets();
    match s[0] {
        0x2002 => IpAddr::V4(Ipv4Addr::new(o[2], o[3], o[4], o[5])),
        // Teredo: the client's public IPv4 is the last 32 bits, inverted.
        0x2001 if s[1] == 0 => IpAddr::V4(Ipv4Addr::new(!o[12], !o[13], !o[14], !o[15])),
        _ => ip,
    }
}

/// One limit key: an IPv4 address, or an IPv6 /64, /56 or /48.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IpKey {
    addr: IpAddr,
    prefix: u8,
}

impl IpKey {
    fn v6(a: Ipv6Addr, prefix: u8) -> Self {
        let mask = !0u128 << (128 - u32::from(prefix));
        Self {
            addr: IpAddr::V6(Ipv6Addr::from(u128::from(a) & mask)),
            prefix,
        }
    }

    /// Share of the per-address limit this key gets.
    pub fn scale(&self) -> f64 {
        match self.prefix {
            56 => V6_56_SCALE,
            48 => V6_48_SCALE,
            _ => 1.0,
        }
    }

    /// Whether `ip` (any form) is limited under this key.
    pub fn contains(&self, ip: IpAddr) -> bool {
        ip_keys(ip).iter().any(|k| k == *self)
    }
}

/// The keys one address is limited under, most specific first.
#[derive(Clone, Copy, Debug)]
pub struct Keys {
    k: [IpKey; 3],
    n: usize,
}

impl Keys {
    pub fn iter(&self) -> impl Iterator<Item = IpKey> + '_ {
        self.k[..self.n].iter().copied()
    }

    /// The most specific key (IPv4 address or IPv6 /64).
    pub fn primary(&self) -> IpKey {
        self.k[0]
    }
}

/// Every key `ip` counts against: its IPv4 address, or its IPv6 /64, /56
/// and /48 (after [`canonical_ip`]).
pub fn ip_keys(ip: IpAddr) -> Keys {
    match canonical_ip(ip) {
        IpAddr::V4(v4) => {
            let k = IpKey {
                addr: IpAddr::V4(v4),
                prefix: 32,
            };
            Keys { k: [k; 3], n: 1 }
        }
        IpAddr::V6(v6) => Keys {
            k: [IpKey::v6(v6, 64), IpKey::v6(v6, 56), IpKey::v6(v6, 48)],
            n: 3,
        },
    }
}

/// The primary key's address (IPv4, or the IPv6 /64).
pub fn ip_key(ip: IpAddr) -> IpAddr {
    ip_keys(ip).primary().addr
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

struct Inner {
    map: HashMap<IpKey, Bucket>,
    global: Bucket,
    overflow: Bucket,
    last_prune: Instant,
}

/// Token buckets per address key (see [`ip_keys`]) plus an optional global
/// bucket. Every per-key bucket is checked before the global one, and
/// nothing is charged unless all pass, so a refused source never drains
/// the global budget. The per-key map is capped at
/// [`MAX_LIMITER_ENTRIES`]; when full, unseen keys share one overflow
/// bucket while known keys keep their own.
pub struct RateLimiter {
    per_ip_rate: f64,
    per_ip_burst: f64,
    global: Option<(f64, f64)>,
    max_entries: usize,
    inner: Mutex<Inner>,
}

impl RateLimiter {
    pub fn new(
        per_ip_per_sec: f64,
        per_ip_burst: f64,
        global_per_sec: f64,
        global_burst: f64,
    ) -> Self {
        Self::build(
            per_ip_per_sec,
            per_ip_burst,
            Some((global_per_sec, global_burst)),
        )
    }

    /// Per-key buckets only; the caller applies any global ceiling itself
    /// (e.g. only to well-formed requests).
    pub fn per_ip_only(per_ip_per_sec: f64, per_ip_burst: f64) -> Self {
        Self::build(per_ip_per_sec, per_ip_burst, None)
    }

    fn build(rate: f64, burst: f64, global: Option<(f64, f64)>) -> Self {
        let now = Instant::now();
        Self {
            per_ip_rate: rate,
            per_ip_burst: burst,
            global,
            max_entries: MAX_LIMITER_ENTRIES,
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                global: Bucket {
                    tokens: global.map_or(0.0, |g| g.1),
                    last: now,
                },
                overflow: Bucket {
                    tokens: burst * OVERFLOW_SCALE,
                    last: now,
                },
                last_prune: now,
            }),
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
        let keys = ip_keys(ip);
        let (rate, burst) = (self.per_ip_rate, self.per_ip_burst);
        let mut g = self.inner.lock().unwrap();
        let inner = &mut *g;
        let unseen = keys.iter().filter(|k| !inner.map.contains_key(k)).count();
        if unseen > 0
            && inner.map.len() + unseen > self.max_entries
            && now.saturating_duration_since(inner.last_prune) >= PRUNE_INTERVAL
        {
            Self::prune_map(&mut inner.map, rate, burst, now);
            inner.last_prune = now;
        }
        let room = inner.map.len() + unseen <= self.max_entries;
        for k in keys.iter() {
            let s = k.scale();
            match inner.map.get_mut(&k) {
                Some(b) => {
                    Self::refill(b, rate * s, burst * s, now);
                    if b.tokens < cost {
                        return false;
                    }
                }
                None if burst * s < cost => return false,
                None => {}
            }
        }
        let overflow = unseen > 0 && !room;
        if overflow {
            let o = &mut inner.overflow;
            Self::refill(o, rate * OVERFLOW_SCALE, burst * OVERFLOW_SCALE, now);
            if o.tokens < cost {
                return false;
            }
        }
        if let Some((gr, gb)) = self.global {
            Self::refill(&mut inner.global, gr, gb, now);
            if inner.global.tokens < cost {
                return false;
            }
            inner.global.tokens -= cost;
        }
        if overflow {
            inner.overflow.tokens -= cost;
        }
        for k in keys.iter() {
            if let Some(b) = inner.map.get_mut(&k) {
                b.tokens -= cost;
            } else if room {
                inner.map.insert(
                    k,
                    Bucket {
                        tokens: burst * k.scale() - cost,
                        last: now,
                    },
                );
            }
        }
        true
    }

    fn prune_map(map: &mut HashMap<IpKey, Bucket>, rate: f64, burst: f64, now: Instant) {
        map.retain(|k, b| {
            let s = k.scale();
            Self::refill(b, rate * s, burst * s, now);
            b.tokens < burst * s
        });
    }

    /// Drop buckets that have fully refilled (called periodically).
    pub fn prune(&self) {
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap();
        Self::prune_map(&mut g.map, self.per_ip_rate, self.per_ip_burst, now);
        g.last_prune = now;
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

/// Concurrency caps per address key: `v4` per IPv4 address, `v6_64` per
/// IPv6 /64, and that scaled by [`V6_56_SCALE`] / [`V6_48_SCALE`] per /56
/// and /48.
#[derive(Clone, Copy, Debug)]
pub struct PrefixCaps {
    pub v4: usize,
    pub v6_64: usize,
}

impl PrefixCaps {
    fn cap(&self, k: &IpKey) -> usize {
        match k.addr {
            IpAddr::V4(_) => self.v4,
            IpAddr::V6(_) => (self.v6_64 as f64 * k.scale()) as usize,
        }
    }
}

/// Concurrent holders (connections, relay pair slots) per address key.
#[derive(Default)]
pub struct ConnCounter {
    inner: Mutex<HashMap<IpKey, usize>>,
}

impl ConnCounter {
    /// Count one more holder for `ip` under every key, or return the first
    /// key that is already at its cap (nothing is counted then).
    pub fn try_acquire(&self, ip: IpAddr, caps: PrefixCaps) -> Result<(), IpKey> {
        let keys = ip_keys(ip);
        let mut m = self.inner.lock().unwrap();
        if let Some(full) = keys
            .iter()
            .find(|k| m.get(k).copied().unwrap_or(0) >= caps.cap(k))
        {
            return Err(full);
        }
        for k in keys.iter() {
            *m.entry(k).or_insert(0) += 1;
        }
        Ok(())
    }

    pub fn release(&self, ip: IpAddr) {
        let mut m = self.inner.lock().unwrap();
        for k in ip_keys(ip).iter() {
            if let Some(c) = m.get_mut(&k) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    m.remove(&k);
                }
            }
        }
    }

    /// Holders under `ip`'s most specific key.
    pub fn count(&self, ip: IpAddr) -> usize {
        let k = ip_keys(ip).primary();
        self.inner.lock().unwrap().get(&k).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn per_ip_bucket_exhausts() {
        let rl = RateLimiter::new(0.0, 3.0, 1000.0, 1000.0);
        let a = ip("198.51.100.1");
        let b = ip("198.51.100.2");
        assert!((0..3).all(|_| rl.allow(a)));
        assert!(!rl.allow(a));
        assert!(rl.allow(b));
        assert_eq!(ip_key(ip("2001:db8::1")), ip_key(ip("2001:db8::ffff")));
    }

    #[test]
    fn refused_source_does_not_drain_global() {
        let rl = RateLimiter::new(0.0, 2.0, 0.0, 5.0);
        let a = ip("198.51.100.1");
        assert!((0..2).all(|_| rl.allow(a)));
        // Thousands of refusals from `a` leave the global bucket alone.
        assert!((0..5000).all(|_| !rl.allow(a)));
        assert!(rl.allow(ip("198.51.100.2")));
        assert!(rl.allow(ip("198.51.100.3")));
        assert!(rl.allow(ip("198.51.100.4")));
        assert!(!rl.allow(ip("198.51.100.5")), "global burst is 5");
    }

    #[test]
    fn embedded_v4_forms_are_canonicalised() {
        let v4 = ip("192.0.2.33");
        assert_eq!(canonical_ip(ip("::ffff:192.0.2.33")), v4);
        assert_eq!(canonical_ip(ip("2002:c000:221::1")), v4);
        assert_eq!(canonical_ip(ip("2002:c000:221:ffff::9")), v4);
        // Teredo: 192.0.2.33 inverted = 3f.ff.fd.de
        assert_eq!(canonical_ip(ip("2001:0:4136:e378:8000:63bf:3fff:fdde")), v4);
        assert_eq!(canonical_ip(ip("2001:db8::1")), ip("2001:db8::1"));
    }

    #[test]
    fn v6_caps_aggregate_by_56_and_48() {
        let c = ConnCounter::default();
        let caps = PrefixCaps { v4: 100, v6_64: 2 };
        // /56 cap = 4, /48 cap = 8. Each /64 holds 2.
        let addr = |n: u16| ip(&format!("2001:db8:1:{n:x}::1"));
        for n in 0..2u16 {
            assert!(c.try_acquire(addr(n), caps).is_ok());
            assert!(c.try_acquire(addr(n), caps).is_ok());
        }
        // Third /64 in the same /56 (2001:db8:1:00xx): refused by the /56.
        let err = c.try_acquire(addr(2), caps).unwrap_err();
        assert!(err.contains(addr(0)) && err.contains(addr(0xff)));
        assert!(!err.contains(addr(0x100)));
        // Other /56s in the same /48 fill the /48 at 8.
        for n in [0x100u16, 0x100, 0x200, 0x200] {
            assert!(c.try_acquire(addr(n), caps).is_ok());
        }
        assert!(c.try_acquire(addr(0x300), caps).is_err());
        // A different /48 is unaffected; releasing frees the aggregate.
        assert!(c.try_acquire(ip("2001:db8:2::1"), caps).is_ok());
        c.release(addr(0));
        assert!(c.try_acquire(addr(0x300), caps).is_ok());
        // v4-mapped and native IPv4 share one key.
        let caps1 = PrefixCaps { v4: 1, v6_64: 1 };
        assert!(c.try_acquire(ip("192.0.2.1"), caps1).is_ok());
        assert!(c.try_acquire(ip("::ffff:192.0.2.1"), caps1).is_err());
    }
}
