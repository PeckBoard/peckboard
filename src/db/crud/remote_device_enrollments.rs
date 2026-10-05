//! Pairing-v2 state of remote-access devices (`remote_device_enrollments`).
//!
//! Every transition of the state machine (pending → staged → active, see
//! the migration) is one transaction here, so the service and the routes
//! never hold a check-then-act window across two queries. Sealed blobs are
//! opaque to this module: the service seals `R` / `S` before calling in
//! and opens what comes back.

use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// Least gap between two `reuse_attempts` writes on one row, so a link
/// holder hammering the refuse loop can't hammer the DB.
const REUSE_WRITE_MIN_SECS: i64 = 60;

/// Why an enrollment request was turned down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrollRefusal {
    /// The link (or legacy pairing) already enrolled another key.
    AlreadyUsed,
    /// The link's hour is over.
    Expired,
    /// No such device, a legacy upgrade on a v2 row or vice versa.
    NotEnrollable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollOutcome {
    /// First enrollment: the attempt's sealed `R` is stored, the row is
    /// `staged`.
    Granted,
    /// The same key enrolled before (crash recovery): hand it the stored
    /// sealed `R` again.
    ReDelivered {
        rendezvous_ciphertext: Vec<u8>,
        rendezvous_nonce: Vec<u8>,
    },
    Refused(EnrollRefusal),
}

/// One verified `EnrollRequest`, with `R` already generated and sealed
/// (AAD `<id>/rendezvous/2`) in case it is the first.
pub struct EnrollAttempt {
    pub device_id: String,
    /// `EnrollMode::LegacyUpgrade`: a pairing without an enrollment row.
    pub legacy_upgrade: bool,
    pub device_pubkey: [u8; 32],
    pub rendezvous_ciphertext: Vec<u8>,
    pub rendezvous_nonce: Vec<u8>,
    pub device_name_hint: String,
    /// The device's address as seen on the tunnel.
    pub from: String,
    /// RFC3339 now.
    pub now: String,
}

/// The activation transition (first handshake on `rid(R)`): `S` moves to
/// the refuse loop (sealed with AAD `<id>/link-refuse`) and the device
/// row's secret is overwritten with a random tombstone.
pub struct ActivateAttempt {
    pub device_id: String,
    pub now: String,
    pub link_secret_ciphertext: Vec<u8>,
    pub link_secret_nonce: Vec<u8>,
    /// Until when the `rid(S)` loop keeps refusing (RFC3339).
    pub link_refuse_until: String,
    pub tombstone_ciphertext: Vec<u8>,
    pub tombstone_nonce: Vec<u8>,
}

/// `a` is strictly after `b`; unparseable stamps count as "not after".
pub fn rfc3339_after(a: &str, b: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(a), Ok(b)) => a > b,
        _ => false,
    }
}

fn secs_between(earlier: &str, later: &str) -> Option<i64> {
    let e = chrono::DateTime::parse_from_rfc3339(earlier).ok()?;
    let l = chrono::DateTime::parse_from_rfc3339(later).ok()?;
    Some((l - e).num_seconds())
}

impl Db {
    /// Pair a device with a v2 link: the device row and its `pending`
    /// enrollment (expiring at `link_expires_at`, RFC3339) in one
    /// transaction.
    pub async fn insert_remote_device_with_link(
        &self,
        new: NewRemoteDevice,
        link_expires_at: String,
    ) -> anyhow::Result<RemoteDevice> {
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::insert_into(remote_devices::table)
                    .values(&new)
                    .execute(conn)?;
                diesel::insert_into(remote_device_enrollments::table)
                    .values(&NewRemoteDeviceEnrollment {
                        device_id: new.id.clone(),
                        state: enrollment_state::PENDING.into(),
                        link_expires_at: Some(link_expires_at),
                        device_pubkey: None,
                        rendezvous_ciphertext: None,
                        rendezvous_nonce: None,
                        enrolled_at: None,
                        enrolled_from: None,
                        device_name_hint: None,
                        created_at: new.created_at.clone(),
                    })
                    .execute(conn)?;
                remote_devices::table
                    .find(&new.id)
                    .select(RemoteDevice::as_select())
                    .first(conn)
                    .map_err(Into::into)
            })
        })
        .await
    }

    pub async fn get_remote_device_enrollment(
        &self,
        device_id: &str,
    ) -> anyhow::Result<Option<RemoteDeviceEnrollment>> {
        let id = device_id.to_string();
        self.with_conn(move |conn| {
            remote_device_enrollments::table
                .find(&id)
                .select(RemoteDeviceEnrollment::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_remote_device_enrollments(
        &self,
    ) -> anyhow::Result<Vec<RemoteDeviceEnrollment>> {
        self.with_conn(move |conn| {
            remote_device_enrollments::table
                .select(RemoteDeviceEnrollment::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Answer a verified enrollment request (see [`EnrollOutcome`]). First
    /// key wins; the same key is re-delivered; a different key is refused
    /// and counted (at most one count per minute).
    pub async fn enroll_remote_device(&self, a: EnrollAttempt) -> anyhow::Result<EnrollOutcome> {
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let exists: Option<String> = remote_devices::table
                    .find(&a.device_id)
                    .select(remote_devices::id)
                    .first(conn)
                    .optional()?;
                if exists.is_none() {
                    return Ok(EnrollOutcome::Refused(EnrollRefusal::NotEnrollable));
                }
                let enr: Option<RemoteDeviceEnrollment> = remote_device_enrollments::table
                    .find(&a.device_id)
                    .select(RemoteDeviceEnrollment::as_select())
                    .first(conn)
                    .optional()?;
                let Some(enr) = enr else {
                    if !a.legacy_upgrade {
                        return Ok(EnrollOutcome::Refused(EnrollRefusal::NotEnrollable));
                    }
                    diesel::insert_into(remote_device_enrollments::table)
                        .values(&NewRemoteDeviceEnrollment {
                            device_id: a.device_id.clone(),
                            state: enrollment_state::STAGED.into(),
                            link_expires_at: None,
                            device_pubkey: Some(a.device_pubkey.to_vec()),
                            rendezvous_ciphertext: Some(a.rendezvous_ciphertext.clone()),
                            rendezvous_nonce: Some(a.rendezvous_nonce.clone()),
                            enrolled_at: Some(a.now.clone()),
                            enrolled_from: Some(a.from.clone()),
                            device_name_hint: Some(a.device_name_hint.clone()),
                            created_at: a.now.clone(),
                        })
                        .execute(conn)?;
                    return Ok(EnrollOutcome::Granted);
                };
                if enr.state == enrollment_state::PENDING {
                    if a.legacy_upgrade {
                        return Ok(EnrollOutcome::Refused(EnrollRefusal::NotEnrollable));
                    }
                    let expired = enr
                        .link_expires_at
                        .as_deref()
                        .is_none_or(|e| rfc3339_after(&a.now, e));
                    if expired {
                        return Ok(EnrollOutcome::Refused(EnrollRefusal::Expired));
                    }
                    diesel::update(remote_device_enrollments::table.find(&a.device_id))
                        .set((
                            remote_device_enrollments::state.eq(enrollment_state::STAGED),
                            remote_device_enrollments::device_pubkey
                                .eq(Some(a.device_pubkey.to_vec())),
                            remote_device_enrollments::rendezvous_ciphertext
                                .eq(Some(&a.rendezvous_ciphertext)),
                            remote_device_enrollments::rendezvous_nonce
                                .eq(Some(&a.rendezvous_nonce)),
                            remote_device_enrollments::enrolled_at.eq(Some(&a.now)),
                            remote_device_enrollments::enrolled_from.eq(Some(&a.from)),
                            remote_device_enrollments::device_name_hint
                                .eq(Some(&a.device_name_hint)),
                        ))
                        .execute(conn)?;
                    return Ok(EnrollOutcome::Granted);
                }
                // staged | active
                if enr.device_pubkey.as_deref() == Some(a.device_pubkey.as_slice()) {
                    return Ok(EnrollOutcome::ReDelivered {
                        rendezvous_ciphertext: enr.rendezvous_ciphertext.unwrap_or_default(),
                        rendezvous_nonce: enr.rendezvous_nonce.unwrap_or_default(),
                    });
                }
                note_reuse(conn, &enr, &a.from, &a.now)?;
                Ok(EnrollOutcome::Refused(EnrollRefusal::AlreadyUsed))
            })
        })
        .await
    }

    /// A refusing `rid(S)` loop was contacted (a used or expired link in
    /// someone's hands): count it, at most once per minute. `false` if
    /// nothing was written.
    pub async fn note_remote_link_reuse(
        &self,
        device_id: &str,
        from: &str,
        now: &str,
    ) -> anyhow::Result<bool> {
        let (id, from, now) = (device_id.to_string(), from.to_string(), now.to_string());
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let enr: Option<RemoteDeviceEnrollment> = remote_device_enrollments::table
                    .find(&id)
                    .select(RemoteDeviceEnrollment::as_select())
                    .first(conn)
                    .optional()?;
                match enr {
                    Some(enr) => note_reuse(conn, &enr, &from, &now),
                    None => Ok(false),
                }
            })
        })
        .await
    }

    /// `staged` → `active` (see [`ActivateAttempt`]). Idempotent: `false`
    /// when the row isn't `staged` (already active, pending, or gone).
    pub async fn activate_remote_device_enrollment(
        &self,
        a: ActivateAttempt,
    ) -> anyhow::Result<bool> {
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let n = diesel::update(
                    remote_device_enrollments::table
                        .find(&a.device_id)
                        .filter(remote_device_enrollments::state.eq(enrollment_state::STAGED)),
                )
                .set((
                    remote_device_enrollments::state.eq(enrollment_state::ACTIVE),
                    remote_device_enrollments::activated_at.eq(Some(&a.now)),
                    remote_device_enrollments::link_secret_ciphertext
                        .eq(Some(&a.link_secret_ciphertext)),
                    remote_device_enrollments::link_secret_nonce.eq(Some(&a.link_secret_nonce)),
                    remote_device_enrollments::link_refuse_until.eq(Some(&a.link_refuse_until)),
                ))
                .execute(conn)?;
                if n == 0 {
                    return Ok(false);
                }
                diesel::update(remote_devices::table.find(&a.device_id))
                    .set((
                        remote_devices::secret_ciphertext.eq(&a.tombstone_ciphertext),
                        remote_devices::secret_nonce.eq(&a.tombstone_nonce),
                    ))
                    .execute(conn)?;
                Ok(true)
            })
        })
        .await
    }

    /// Drop the refuse-loop copy of `S` from every active row whose
    /// `link_refuse_until` has passed. Returns how many rows changed.
    pub async fn clear_expired_remote_link_secrets(&self, now: &str) -> anyhow::Result<usize> {
        let now = now.to_string();
        self.with_conn(move |conn| {
            let rows: Vec<RemoteDeviceEnrollment> = remote_device_enrollments::table
                .filter(remote_device_enrollments::state.eq(enrollment_state::ACTIVE))
                .filter(remote_device_enrollments::link_secret_ciphertext.is_not_null())
                .select(RemoteDeviceEnrollment::as_select())
                .load(conn)?;
            let mut cleared = 0;
            for r in rows {
                let due = r
                    .link_refuse_until
                    .as_deref()
                    .is_none_or(|u| rfc3339_after(&now, u));
                if !due {
                    continue;
                }
                cleared += diesel::update(remote_device_enrollments::table.find(&r.device_id))
                    .set((
                        remote_device_enrollments::link_secret_ciphertext.eq(None::<Vec<u8>>),
                        remote_device_enrollments::link_secret_nonce.eq(None::<Vec<u8>>),
                    ))
                    .execute(conn)?;
            }
            Ok(cleared)
        })
        .await
    }

    /// Give a `pending` (unused or expired) link a fresh `S` and expiry;
    /// `false` when the row isn't pending (staged / active / legacy / gone).
    pub async fn reissue_remote_device_link(
        &self,
        device_id: &str,
        secret_ciphertext: Vec<u8>,
        secret_nonce: Vec<u8>,
        link_expires_at: &str,
    ) -> anyhow::Result<bool> {
        let (id, exp) = (device_id.to_string(), link_expires_at.to_string());
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let n = diesel::update(
                    remote_device_enrollments::table
                        .find(&id)
                        .filter(remote_device_enrollments::state.eq(enrollment_state::PENDING)),
                )
                .set(remote_device_enrollments::link_expires_at.eq(Some(&exp)))
                .execute(conn)?;
                if n == 0 {
                    return Ok(false);
                }
                diesel::update(remote_devices::table.find(&id))
                    .set((
                        remote_devices::secret_ciphertext.eq(&secret_ciphertext),
                        remote_devices::secret_nonce.eq(&secret_nonce),
                    ))
                    .execute(conn)?;
                Ok(true)
            })
        })
        .await
    }
}

/// Bump `reuse_attempts` unless the last bump was under a minute ago.
fn note_reuse(
    conn: &mut SqliteConnection,
    enr: &RemoteDeviceEnrollment,
    from: &str,
    now: &str,
) -> anyhow::Result<bool> {
    let recent = enr
        .last_reuse_at
        .as_deref()
        .and_then(|last| secs_between(last, now))
        .is_some_and(|s| s < REUSE_WRITE_MIN_SECS);
    if recent {
        return Ok(false);
    }
    diesel::update(remote_device_enrollments::table.find(&enr.device_id))
        .set((
            remote_device_enrollments::reuse_attempts.eq(enr.reuse_attempts + 1),
            remote_device_enrollments::last_reuse_at.eq(Some(now)),
            remote_device_enrollments::last_reuse_from.eq(Some(from)),
        ))
        .execute(conn)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_plus(secs: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()
    }

    async fn pair(db: &Db, id: &str, expires_in: i64) -> RemoteDevice {
        db.insert_remote_device_with_link(
            NewRemoteDevice {
                id: id.into(),
                user_id: "u1".into(),
                name: "phone".into(),
                secret_ciphertext: vec![1; 48],
                secret_nonce: vec![2; 12],
                created_at: chrono::Utc::now().to_rfc3339(),
                last_connected_at: None,
            },
            now_plus(expires_in),
        )
        .await
        .unwrap()
    }

    fn attempt(id: &str, key: u8, legacy_upgrade: bool) -> EnrollAttempt {
        EnrollAttempt {
            device_id: id.into(),
            legacy_upgrade,
            device_pubkey: [key; 32],
            rendezvous_ciphertext: vec![key; 48],
            rendezvous_nonce: vec![key; 12],
            device_name_hint: "iPhone".into(),
            from: "203.0.113.7:4000".into(),
            now: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// pending → staged → active; the same key is re-delivered, a different
    /// key refused and counted (rate-limited), activation tombstones the
    /// device secret and moves S, and revoke deletes both rows.
    #[tokio::test]
    async fn state_machine_pending_staged_active() {
        let db = Db::in_memory().unwrap();
        pair(&db, "d1", 3600).await;
        let e = db
            .get_remote_device_enrollment("d1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e.state, "pending");

        // A legacy upgrade on a v2 row is a mode mismatch.
        assert_eq!(
            db.enroll_remote_device(attempt("d1", 7, true))
                .await
                .unwrap(),
            EnrollOutcome::Refused(EnrollRefusal::NotEnrollable)
        );
        assert_eq!(
            db.enroll_remote_device(attempt("d1", 7, false))
                .await
                .unwrap(),
            EnrollOutcome::Granted
        );
        let e = db
            .get_remote_device_enrollment("d1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e.state, "staged");
        assert_eq!(e.device_pubkey.as_deref(), Some(&[7u8; 32][..]));
        assert_eq!(e.enrolled_from.as_deref(), Some("203.0.113.7:4000"));
        assert_eq!(e.device_name_hint.as_deref(), Some("iPhone"));

        // Same key: the stored R comes back; a different key is refused.
        let mut again = attempt("d1", 7, false);
        again.rendezvous_ciphertext = vec![9; 48];
        assert_eq!(
            db.enroll_remote_device(again).await.unwrap(),
            EnrollOutcome::ReDelivered {
                rendezvous_ciphertext: vec![7; 48],
                rendezvous_nonce: vec![7; 12],
            }
        );
        for _ in 0..3 {
            assert_eq!(
                db.enroll_remote_device(attempt("d1", 8, false))
                    .await
                    .unwrap(),
                EnrollOutcome::Refused(EnrollRefusal::AlreadyUsed)
            );
        }
        let e = db
            .get_remote_device_enrollment("d1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e.reuse_attempts, 1, "counted once per minute");
        assert_eq!(e.last_reuse_from.as_deref(), Some("203.0.113.7:4000"));

        // Activation: S moves to the refuse loop, the device secret is a
        // tombstone; a second activation is a no-op.
        let act = || ActivateAttempt {
            device_id: "d1".into(),
            now: chrono::Utc::now().to_rfc3339(),
            link_secret_ciphertext: vec![5; 48],
            link_secret_nonce: vec![5; 12],
            link_refuse_until: now_plus(86_400),
            tombstone_ciphertext: vec![6; 48],
            tombstone_nonce: vec![6; 12],
        };
        assert!(db.activate_remote_device_enrollment(act()).await.unwrap());
        assert!(!db.activate_remote_device_enrollment(act()).await.unwrap());
        let e = db
            .get_remote_device_enrollment("d1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e.state, "active");
        assert!(e.activated_at.is_some());
        assert_eq!(e.link_secret_ciphertext, Some(vec![5; 48]));
        let d = db.get_remote_device("d1").await.unwrap().unwrap();
        assert_eq!(d.secret_ciphertext, vec![6; 48]);
        // Not due yet: nothing cleared; a stamp past the deadline clears it.
        let now = chrono::Utc::now().to_rfc3339();
        assert_eq!(db.clear_expired_remote_link_secrets(&now).await.unwrap(), 0);
        let later = now_plus(2 * 86_400);
        assert_eq!(
            db.clear_expired_remote_link_secrets(&later).await.unwrap(),
            1
        );
        let e = db
            .get_remote_device_enrollment("d1")
            .await
            .unwrap()
            .unwrap();
        assert!(e.link_secret_ciphertext.is_none());
        // A used link can't be re-issued.
        assert!(
            !db.reissue_remote_device_link("d1", vec![3; 48], vec![3; 12], &now_plus(3600))
                .await
                .unwrap()
        );

        // Revoke: both rows go in one transaction.
        assert!(db.delete_remote_device("d1").await.unwrap());
        assert!(
            db.get_remote_device_enrollment("d1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.list_remote_device_enrollments()
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// An expired link refuses every key until it is re-issued; a legacy
    /// pairing (no row) enrolls straight into `staged` with no expiry.
    #[tokio::test]
    async fn expired_links_reissue_and_legacy_upgrade() {
        let db = Db::in_memory().unwrap();
        pair(&db, "d1", -5).await;
        assert_eq!(
            db.enroll_remote_device(attempt("d1", 7, false))
                .await
                .unwrap(),
            EnrollOutcome::Refused(EnrollRefusal::Expired)
        );
        assert!(
            db.reissue_remote_device_link("d1", vec![3; 48], vec![3; 12], &now_plus(3600))
                .await
                .unwrap()
        );
        let d = db.get_remote_device("d1").await.unwrap().unwrap();
        assert_eq!(d.secret_ciphertext, vec![3; 48]);
        assert_eq!(
            db.enroll_remote_device(attempt("d1", 7, false))
                .await
                .unwrap(),
            EnrollOutcome::Granted
        );

        db.insert_remote_device(NewRemoteDevice {
            id: "legacy".into(),
            user_id: "u1".into(),
            name: "old phone".into(),
            secret_ciphertext: vec![1; 48],
            secret_nonce: vec![2; 12],
            created_at: chrono::Utc::now().to_rfc3339(),
            last_connected_at: None,
        })
        .await
        .unwrap();
        assert_eq!(
            db.enroll_remote_device(attempt("legacy", 7, false))
                .await
                .unwrap(),
            EnrollOutcome::Refused(EnrollRefusal::NotEnrollable),
            "a v2 link request on a legacy pairing"
        );
        assert_eq!(
            db.enroll_remote_device(attempt("legacy", 7, true))
                .await
                .unwrap(),
            EnrollOutcome::Granted
        );
        let e = db
            .get_remote_device_enrollment("legacy")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e.state, "staged");
        assert!(e.link_expires_at.is_none());
        assert_eq!(
            db.enroll_remote_device(attempt("nope", 7, true))
                .await
                .unwrap(),
            EnrollOutcome::Refused(EnrollRefusal::NotEnrollable)
        );
    }
}
