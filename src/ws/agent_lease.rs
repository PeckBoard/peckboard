//! Exclusive control leases for remote devices.
//!
//! Only one session may drive a `peckboard-agent` at a time, so two
//! sessions can't interleave conflicting mouse / keyboard / terminal
//! orders. A session takes the lease with `remote_agent_lock`, every
//! bridged call must present it, and it lapses on its own:
//!
//! - a fresh lease lives [`LEASE_INITIAL`] (30s);
//! - each use tops it up so at least [`LEASE_EXTEND`] (15s) remains —
//!   `expires_at = max(expires_at, now + 15s)`;
//! - `remote_agent_unlock` releases it early.
//!
//! While the holder has a request in flight (a long `remote_agent_run`)
//! the lease can't be taken over even if its clock ran out, and it is
//! topped up again when the reply lands — otherwise a second session could
//! start issuing orders mid-command.
//!
//! Server-side only and in memory: leases are a coordination gate between
//! sessions, not durable state, and a restart dropping them is harmless.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Lifetime of a freshly acquired lease.
pub const LEASE_INITIAL: Duration = Duration::from_secs(30);
/// Minimum remaining lifetime after each use.
pub const LEASE_EXTEND: Duration = Duration::from_secs(15);

struct Lease {
    session_id: String,
    expires_at: Instant,
}

/// Why a lease operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// Another session holds a live lease on the device.
    HeldByOther { remaining: Duration },
    /// The caller holds no live lease on the device.
    NotHeld,
}

/// Snapshot of a device's live lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseStatus {
    pub session_id: String,
    pub remaining: Duration,
}

// Proof token: bearer's session held a live lease on `device_id` when
// `LeaseTable::use_lease` minted it (and that use extended the lease).
// `DeviceRegistry::send_request` takes one, so no dispatch path can reach
// a device without passing the gate. See `remote_agent_call` for an example.
#[derive(Debug)]
pub struct DeviceLease {
    device_id: String,
    session_id: String,
}

impl DeviceLease {
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// device_id → live lease.
#[derive(Default)]
pub struct LeaseTable {
    leases: Mutex<HashMap<String, Lease>>,
}

fn remaining(lease: &Lease, now: Instant) -> Duration {
    lease.expires_at.saturating_duration_since(now)
}

/// Top up to at least [`LEASE_EXTEND`] remaining.
fn extend(lease: &mut Lease, now: Instant) {
    lease.expires_at = lease.expires_at.max(now + LEASE_EXTEND);
}

impl LeaseTable {
    /// Take the lease for `session_id`, or top up the one it already
    /// holds. `busy` = the device has requests in flight, which keeps a
    /// lapsed lease with its holder.
    pub fn acquire(
        &self,
        device_id: &str,
        session_id: &str,
        busy: bool,
        now: Instant,
    ) -> Result<LeaseStatus, LeaseError> {
        let mut leases = self.leases.lock().unwrap();
        if let Some(lease) = leases.get_mut(device_id) {
            let live = lease.expires_at > now || busy;
            if lease.session_id == session_id && live {
                extend(lease, now);
                return Ok(LeaseStatus {
                    session_id: session_id.to_string(),
                    remaining: remaining(lease, now),
                });
            }
            if live {
                return Err(LeaseError::HeldByOther {
                    remaining: remaining(lease, now),
                });
            }
        }
        leases.insert(
            device_id.to_string(),
            Lease {
                session_id: session_id.to_string(),
                expires_at: now + LEASE_INITIAL,
            },
        );
        Ok(LeaseStatus {
            session_id: session_id.to_string(),
            remaining: LEASE_INITIAL,
        })
    }

    /// Check `session_id` holds the device's lease and extend it; the
    /// returned token is what [`crate::ws::agent::DeviceRegistry::send_request`]
    /// requires.
    pub fn use_lease(
        &self,
        device_id: &str,
        session_id: &str,
        busy: bool,
        now: Instant,
    ) -> Result<DeviceLease, LeaseError> {
        let mut leases = self.leases.lock().unwrap();
        let Some(lease) = leases.get_mut(device_id) else {
            return Err(LeaseError::NotHeld);
        };
        let live = lease.expires_at > now || busy;
        if !live {
            return Err(LeaseError::NotHeld);
        }
        if lease.session_id != session_id {
            return Err(LeaseError::HeldByOther {
                remaining: remaining(lease, now),
            });
        }
        extend(lease, now);
        Ok(DeviceLease {
            device_id: device_id.to_string(),
            session_id: session_id.to_string(),
        })
    }

    /// Top up after a request completes, if the bearer still holds it.
    pub fn touch(&self, lease: &DeviceLease, now: Instant) {
        if let Some(l) = self.leases.lock().unwrap().get_mut(&lease.device_id)
            && l.session_id == lease.session_id
        {
            extend(l, now);
        }
    }

    /// Drop `session_id`'s lease. Refused if it holds none (or another
    /// session's is live).
    pub fn release(
        &self,
        device_id: &str,
        session_id: &str,
        busy: bool,
        now: Instant,
    ) -> Result<(), LeaseError> {
        let mut leases = self.leases.lock().unwrap();
        match leases.get(device_id) {
            Some(l) if l.session_id == session_id => {
                leases.remove(device_id);
                Ok(())
            }
            Some(l) if l.expires_at > now || busy => Err(LeaseError::HeldByOther {
                remaining: remaining(l, now),
            }),
            _ => Err(LeaseError::NotHeld),
        }
    }

    /// The device's live lease, if any.
    pub fn status(&self, device_id: &str, busy: bool, now: Instant) -> Option<LeaseStatus> {
        let leases = self.leases.lock().unwrap();
        let lease = leases.get(device_id)?;
        (lease.expires_at > now || busy).then(|| LeaseStatus {
            session_id: lease.session_id.clone(),
            remaining: remaining(lease, now),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn lease_lifecycle_follows_the_30s_then_15s_floor_rule() {
        let t = LeaseTable::default();
        let t0 = Instant::now();

        // No lease → calls refused.
        assert_eq!(
            t.use_lease("d", "a", false, t0).unwrap_err(),
            LeaseError::NotHeld
        );

        // Fresh lease: 30s. Another session is locked out.
        assert_eq!(t.acquire("d", "a", false, t0).unwrap().remaining, 30 * S);
        assert_eq!(
            t.acquire("d", "b", false, t0 + 5 * S).unwrap_err(),
            LeaseError::HeldByOther { remaining: 25 * S }
        );
        assert!(matches!(
            t.use_lease("d", "b", false, t0 + 5 * S),
            Err(LeaseError::HeldByOther { .. })
        ));

        // Use at 10s: 20s remain (≥15) → unchanged.
        t.use_lease("d", "a", false, t0 + 10 * S).unwrap();
        assert_eq!(t.status("d", false, t0 + 10 * S).unwrap().remaining, 20 * S);
        // Use at 20s: 10s remain (<15) → topped up to 15s.
        t.use_lease("d", "a", false, t0 + 20 * S).unwrap();
        assert_eq!(t.status("d", false, t0 + 20 * S).unwrap().remaining, 15 * S);

        // Lapsed at 35s: holder refused, another session may take it.
        assert_eq!(
            t.use_lease("d", "a", false, t0 + 36 * S).unwrap_err(),
            LeaseError::NotHeld
        );
        assert!(t.status("d", false, t0 + 36 * S).is_none());
        assert_eq!(
            t.acquire("d", "b", false, t0 + 36 * S).unwrap().remaining,
            30 * S
        );
    }

    #[test]
    fn in_flight_request_pins_a_lapsed_lease_and_release_frees_it() {
        let t = LeaseTable::default();
        let t0 = Instant::now();
        t.acquire("d", "a", false, t0).unwrap();

        // Clock ran out mid-command: still a's while busy.
        assert!(matches!(
            t.acquire("d", "b", true, t0 + 60 * S),
            Err(LeaseError::HeldByOther { .. })
        ));
        let token = t.use_lease("d", "a", true, t0 + 60 * S).unwrap();
        t.touch(&token, t0 + 60 * S);
        assert_eq!(t.status("d", false, t0 + 60 * S).unwrap().remaining, 15 * S);

        // Only the holder can release.
        assert!(matches!(
            t.release("d", "b", false, t0 + 61 * S),
            Err(LeaseError::HeldByOther { .. })
        ));
        t.release("d", "a", false, t0 + 61 * S).unwrap();
        assert_eq!(
            t.release("d", "a", false, t0 + 61 * S).unwrap_err(),
            LeaseError::NotHeld
        );
        t.acquire("d", "b", false, t0 + 61 * S).unwrap();
    }
}
