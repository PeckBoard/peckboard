//! Paired boxes. Non-secret metadata (name, relay, port) lives in a JSON file
//! in the app data dir; credentials go to secure storage (Keychain /
//! Keystore), one item per kind:
//!
//! | key            | value                              | lifetime                      |
//! | -------------- | ---------------------------------- | ----------------------------- |
//! | `box:<id>`     | the pairing link (v1 or v2 URI)    | pairing → activation (v2)     |
//! | `boxkey:<id>`  | this device's key seed, base64url  | before enrolling → activation |
//! | `box2:<id>`    | `peckboard-cred:2:…` (R, D, B)     | enrollment → removal          |
//!
//! Pairings from before v2 have only `box:`; they keep working as legacy
//! and move to `box2:` on their first connect to an updated box.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use ed25519_dalek::SigningKey;
use peckboard_relay::identity::{decode_key, encode_key};
use peckboard_relay::tunnel::{DeviceCredential, EnrolledCredential, OnEnrolled, PairingLink};
use serde::{Deserialize, Serialize};

/// Fixed per-box loopback ports. The web UI keeps its login in
/// per-origin localStorage, so a box must keep the same port across launches
/// or it silently logs out.
pub const PORT_BASE: u16 = 41000;
pub const PORT_LAST: u16 = 41999;
const MAX_NAME: usize = 64;

/// Secure storage for pairing credentials; the native plugin in the app, an
/// in-memory map in tests.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> anyhow::Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> anyhow::Result<()>;
    fn delete(&self, key: &str) -> anyhow::Result<()>;
}

/// How a box authenticates this device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    /// A v1 pairing: the link secret is the credential (upgrades on the
    /// first connect to an updated box). The default, so files written
    /// before pairing v2 read as legacy.
    #[default]
    Legacy,
    /// A v2 link stored, enrollment not finished yet.
    Enrolling,
    /// Enrolled: this device's own key, pinned to the box key.
    Enrolled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoxRecord {
    pub id: String,
    pub name: String,
    pub relay: String,
    pub port: u16,
    /// Public: a prefix of the relay-auth public key derived from the
    /// link secret (the relay already sees it). Used to refuse duplicates.
    pub fingerprint: String,
    pub added_at: u64,
    #[serde(default)]
    pub last_connected_at: Option<u64>,
    /// The user's answer to "Allow <box> to use the microphone?" for this
    /// box's UI; `None` until asked. Remembered per box, not per origin, so
    /// a box keeps its answer even if it ever has to run on another port.
    #[serde(default)]
    pub mic_allowed: Option<bool>,
    #[serde(default)]
    pub auth: Auth,
    /// `XXXX-XXXX-XXXX-XXXX` of the box identity key (v2 links; set at
    /// enrollment for legacy upgrades).
    #[serde(default)]
    pub box_fp: Option<String>,
    /// The v2 link's advisory expiry (unix seconds), for the UI.
    #[serde(default)]
    pub link_expires_at: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct FileV1 {
    version: u32,
    boxes: Vec<BoxRecord>,
    /// Lowest port never handed out (see [`next_port`]). Absent in files
    /// written before ports stopped being reused; [`Store::load`] then takes
    /// the high-water mark of the ports still in use.
    #[serde(default)]
    next_port: Option<u16>,
}

pub struct Store {
    path: PathBuf,
    boxes: Vec<BoxRecord>,
    next_port: u16,
}

/// `box:<id>`: the pairing link.
pub fn secret_key(id: &str) -> String {
    format!("box:{id}")
}

/// `boxkey:<id>`: this device's key seed while enrolling.
pub fn device_key_key(id: &str) -> String {
    format!("boxkey:{id}")
}

/// `box2:<id>`: the enrolled credential.
pub fn credential_key(id: &str) -> String {
    format!("box2:{id}")
}

pub fn fingerprint(link: &PairingLink) -> String {
    link.secret.derive().public_key()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The port for a new box: `high_water`, the lowest port this install has
/// never handed out. A removed box's port is **not** reused — the WebView
/// keeps that origin's website data (the box UI's login token in
/// localStorage, IndexedDB, caches) and native per-origin clearing is only
/// partial (see `commands::forget_site_data`), so a new box on an old port
/// would be served another box's data. Only once every port in
/// `PORT_BASE..=PORT_LAST` has been used (a thousand pairings) does it fall
/// back to the lowest port no current box holds; `None` when all are held.
pub fn next_port(high_water: u16, used: impl IntoIterator<Item = u16>) -> Option<u16> {
    if (PORT_BASE..=PORT_LAST).contains(&high_water) {
        return Some(high_water);
    }
    let used: std::collections::HashSet<u16> = used.into_iter().collect();
    (PORT_BASE..=PORT_LAST).find(|p| !used.contains(p))
}

/// The high-water mark implied by the ports in use: one past the highest
/// (an older file never recorded which lower ports had been used and
/// released, so those stay off limits too).
fn high_water(boxes: &[BoxRecord]) -> u16 {
    boxes
        .iter()
        .map(|b| b.port.saturating_add(1))
        .max()
        .unwrap_or(PORT_BASE)
        .max(PORT_BASE)
}

fn clean_name(name: &str) -> anyhow::Result<String> {
    let name = name.trim();
    if name.chars().count() > MAX_NAME {
        bail!("Name is too long (max {MAX_NAME} characters).");
    }
    Ok(name.to_string())
}

fn new_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The `on_enrolled` hook for `id`: stores the credential the box granted
/// under `box2:<id>` **before** the box is told (an error means no ack, so
/// the box re-delivers on the next attempt).
pub fn enrolled_writer(secrets: Arc<dyn SecretStore>, id: &str) -> OnEnrolled {
    let key = credential_key(id);
    Arc::new(move |c: &EnrolledCredential| {
        secrets
            .set(&key, &c.encode())
            .context("Couldn't save the pairing key to secure storage.")
    })
}

impl Store {
    pub fn load(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let (boxes, next_port) = match std::fs::read(&path) {
            Ok(b) => {
                let f: FileV1 = serde_json::from_slice(&b)
                    .with_context(|| format!("read {}", path.display()))?;
                // A file from before the mark existed, or one whose mark
                // somehow fell behind: never go below what's in use.
                let mark = f.next_port.unwrap_or(0).max(high_water(&f.boxes));
                (f.boxes, mark)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), PORT_BASE),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Ok(Self {
            path,
            boxes,
            next_port,
        })
    }

    pub fn boxes(&self) -> &[BoxRecord] {
        &self.boxes
    }

    pub fn get(&self, id: &str) -> Option<&BoxRecord> {
        self.boxes.iter().find(|b| b.id == id)
    }

    /// Pair a new box: link to secure storage first, then metadata. An
    /// empty `name` becomes "PeckBoard" / "PeckBoard 2" / …. A v2 link
    /// starts `Enrolling` (the caller runs the enrollment); a v1 link is
    /// `Legacy`.
    pub fn add(
        &mut self,
        secrets: &dyn SecretStore,
        link: &PairingLink,
        name: &str,
        now_ms: u64,
    ) -> anyhow::Result<BoxRecord> {
        let fp = fingerprint(link);
        if let Some(b) = self.boxes.iter().find(|b| b.fingerprint == fp) {
            bail!("This link is already paired as “{}”.", b.name);
        }
        let box_fp = link.box_fingerprint();
        if let Some(b) = self
            .boxes
            .iter()
            .find(|b| box_fp.is_some() && b.box_fp == box_fp)
        {
            bail!(
                "You already have this box as “{}”. Remove it first to pair it again.",
                b.name
            );
        }
        let mut name = clean_name(name)?;
        if name.is_empty() {
            name = match self.boxes.len() {
                0 => "PeckBoard".to_string(),
                n => format!("PeckBoard {}", n + 1),
            };
        }
        let port = next_port(self.next_port, self.boxes.iter().map(|b| b.port))
            .context("Too many paired boxes.")?;
        let rec = BoxRecord {
            id: new_id(),
            name,
            relay: link.relay.clone(),
            port,
            fingerprint: fp,
            added_at: now_ms,
            last_connected_at: None,
            mic_allowed: None,
            auth: if link.is_v2() {
                Auth::Enrolling
            } else {
                Auth::Legacy
            },
            box_fp,
            link_expires_at: link.expires,
        };
        secrets
            .set(&secret_key(&rec.id), &link.to_uri())
            .context("Couldn't save the pairing secret to secure storage.")?;
        let mark = self.next_port;
        self.next_port = self.next_port.max(port.saturating_add(1));
        self.boxes.push(rec.clone());
        if let Err(e) = self.save() {
            self.boxes.pop();
            self.next_port = mark;
            let _ = secrets.delete(&secret_key(&rec.id));
            return Err(e);
        }
        Ok(rec)
    }

    pub fn rename(&mut self, id: &str, name: &str) -> anyhow::Result<BoxRecord> {
        let name = clean_name(name)?;
        if name.is_empty() {
            bail!("Name can't be empty.");
        }
        let b = self
            .boxes
            .iter_mut()
            .find(|b| b.id == id)
            .context("No such box.")?;
        b.name = name;
        let out = b.clone();
        self.save()?;
        Ok(out)
    }

    /// Forget a box and wipe every credential it had. Its port is retired,
    /// never handed out again (see [`next_port`]). Returns the removed
    /// record so the caller can clear the WebView data its UI left at that
    /// port.
    pub fn remove(&mut self, secrets: &dyn SecretStore, id: &str) -> anyhow::Result<BoxRecord> {
        let i = self
            .boxes
            .iter()
            .position(|b| b.id == id)
            .context("No such box.")?;
        let removed = self.boxes.remove(i);
        if let Err(e) = self.save() {
            self.boxes.insert(i, removed);
            return Err(e);
        }
        let mut first_err = None;
        for key in [secret_key(id), device_key_key(id), credential_key(id)] {
            if let Err(e) = secrets.delete(&key) {
                first_err.get_or_insert(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(removed),
        }
    }

    /// What to connect with, from what secure storage holds — which also
    /// self-heals metadata that fell behind (a crash between the keychain
    /// write and the JSON save):
    ///
    /// 1. `box2:` present → `Enrolled`;
    /// 2. else a v2 link in `box:` → `Link`, enrolling the key in
    ///    `boxkey:` (created now if absent, so a retry reuses it);
    /// 3. else a v1 link → `Legacy`, upgrading to that same key.
    pub fn credential(
        &self,
        secrets: &dyn SecretStore,
        id: &str,
    ) -> anyhow::Result<DeviceCredential> {
        self.get(id).context("No such box.")?;
        if let Some(enc) = secrets.get(&credential_key(id))? {
            let cred = EnrolledCredential::parse(&enc).context(
                "This box's stored pairing key is unreadable. Remove it and pair again.",
            )?;
            return Ok(DeviceCredential::Enrolled(cred));
        }
        let uri = secrets.get(&secret_key(id))?.context(
            "This box's pairing secret is missing from secure storage (restored from a backup?). Remove it and pair again.",
        )?;
        let link = PairingLink::parse(&uri)?;
        let key = device_key(secrets, id)?;
        Ok(DeviceCredential::from_link(link, key))
    }

    /// The box granted this device its own key (`box2:` is written by
    /// [`enrolled_writer`]): remember it as enrolled, with the box
    /// fingerprint to show.
    pub fn set_enrolled(&mut self, id: &str, box_fp: &str) -> anyhow::Result<()> {
        let b = self
            .boxes
            .iter_mut()
            .find(|b| b.id == id)
            .context("No such box.")?;
        if b.auth != Auth::Enrolled || b.box_fp.as_deref() != Some(box_fp) {
            b.auth = Auth::Enrolled;
            b.box_fp = Some(box_fp.to_string());
            self.save()?;
        }
        Ok(())
    }

    /// First tunnel on the enrolled credential: the box has retired the
    /// link secret, so the link and the loose key seed go too.
    pub fn finish_activation(&mut self, secrets: &dyn SecretStore, id: &str) -> anyhow::Result<()> {
        let b = self
            .boxes
            .iter_mut()
            .find(|b| b.id == id)
            .context("No such box.")?;
        if b.auth != Auth::Enrolled {
            b.auth = Auth::Enrolled;
            self.save()?;
        }
        for key in [secret_key(id), device_key_key(id)] {
            secrets.delete(&key)?;
        }
        Ok(())
    }

    pub fn touch_connected(&mut self, id: &str, now_ms: u64) -> anyhow::Result<()> {
        if let Some(b) = self.boxes.iter_mut().find(|b| b.id == id) {
            b.last_connected_at = Some(now_ms);
            self.save()?;
        }
        Ok(())
    }

    /// Remember the microphone answer for `id`'s UI (`None` forgets it, so
    /// the box asks again).
    pub fn set_mic_allowed(&mut self, id: &str, allowed: Option<bool>) -> anyhow::Result<()> {
        let b = self
            .boxes
            .iter_mut()
            .find(|b| b.id == id)
            .context("No such box.")?;
        if b.mic_allowed != allowed {
            b.mic_allowed = allowed;
            self.save()?;
        }
        Ok(())
    }

    /// Write-then-rename so a crash never leaves a truncated file.
    fn save(&self) -> anyhow::Result<()> {
        write_atomic(
            &self.path,
            &serde_json::to_vec_pretty(&FileV1 {
                version: 1,
                boxes: self.boxes.clone(),
                next_port: Some(self.next_port),
            })?,
        )
    }
}

/// This device's key for `id` (`boxkey:`), generated and stored on first
/// use — before any enrollment request carries it, so a retry after a
/// crash presents the same key and the box re-delivers the same `R`.
fn device_key(secrets: &dyn SecretStore, id: &str) -> anyhow::Result<SigningKey> {
    let key = device_key_key(id);
    if let Some(enc) = secrets.get(&key)? {
        let seed = decode_key(&enc).context("This box's stored device key is unreadable.")?;
        return Ok(SigningKey::from_bytes(&seed));
    }
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    secrets
        .set(&key, &encode_key(&seed))
        .context("Couldn't save the device key to secure storage.")?;
    Ok(SigningKey::from_bytes(&seed))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use peckboard_relay::identity::BoxIdentity;
    use peckboard_relay::keys::{PairingSecret, RendezvousSecret};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct MemSecrets(pub Mutex<HashMap<String, String>>);

    impl SecretStore for MemSecrets {
        fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().insert(key.into(), value.into());
            Ok(())
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    fn link(n: u8) -> PairingLink {
        PairingLink::new(PairingSecret::from_bytes([n; 32]), "relay.example.com")
    }

    fn box_key() -> [u8; 32] {
        BoxIdentity::from_seed([4; 32]).public_key()
    }

    fn link_v2(n: u8) -> PairingLink {
        PairingLink::new_v2(
            PairingSecret::from_bytes([n; 32]),
            "relay.example.com",
            box_key(),
            1_800_000_000,
        )
    }

    /// The link's secret for `s2.credential()` comparisons.
    fn link_of(cred: &DeviceCredential) -> &PairingLink {
        match cred {
            DeviceCredential::Legacy { link, .. } | DeviceCredential::Link { link, .. } => link,
            DeviceCredential::Enrolled(_) => panic!("enrolled"),
        }
    }

    #[test]
    fn round_trip_keeps_metadata_and_secret_apart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boxes.json");
        let secrets = MemSecrets::default();
        let mut s = Store::load(&path).unwrap();
        let a = s.add(&secrets, &link(1), "  Home  ", 10).unwrap();
        let b = s.add(&secrets, &link(2), "", 20).unwrap();
        assert_eq!(a.name, "Home");
        assert_eq!(b.name, "PeckBoard 2");
        assert_eq!(a.auth, Auth::Legacy);
        s.touch_connected(&a.id, 99).unwrap();

        // The file holds no secret material.
        let raw = std::fs::read_to_string(&path).unwrap();
        let secret_b64 = link(1).to_uri();
        let secret_b64 = secret_b64
            .trim_start_matches("peckboard://pair/")
            .split('?')
            .next()
            .unwrap();
        assert!(!raw.contains(secret_b64));

        let s2 = Store::load(&path).unwrap();
        assert_eq!(s2.boxes(), s.boxes());
        assert_eq!(s2.get(&a.id).unwrap().last_connected_at, Some(99));
        let ca = s2.credential(&secrets, &a.id).unwrap();
        assert_eq!(link_of(&ca).to_uri(), link(1).to_uri());
        assert!(matches!(
            ca,
            DeviceCredential::Legacy {
                upgrade_key: Some(_),
                ..
            }
        ));
        let cb = s2.credential(&secrets, &b.id).unwrap();
        assert_eq!(link_of(&cb).to_uri(), link(2).to_uri());
    }

    #[test]
    fn duplicate_link_refused_and_remove_wipes_every_secret() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = MemSecrets::default();
        let mut s = Store::load(dir.path().join("boxes.json")).unwrap();
        let a = s.add(&secrets, &link(1), "Home", 0).unwrap();
        let err = s.add(&secrets, &link(1), "Again", 0).unwrap_err();
        assert!(err.to_string().contains("Home"), "{err}");
        // Creates `boxkey:`.
        s.credential(&secrets, &a.id).unwrap();
        secrets.set(&credential_key(&a.id), "x").unwrap();
        assert_eq!(secrets.0.lock().unwrap().len(), 3);

        let removed = s.remove(&secrets, &a.id).unwrap();
        assert_eq!(removed, a);
        assert!(s.boxes().is_empty());
        assert!(secrets.0.lock().unwrap().is_empty());
        assert!(s.credential(&secrets, &a.id).is_err());
        assert!(s.remove(&secrets, &a.id).is_err());
    }

    /// Pairing v2 on the device: a v2 link starts `Enrolling` with a device
    /// key that survives a retry; the enrolled credential wins once stored
    /// (even before the JSON caught up); activation drops the link and the
    /// loose key, and the same box can't be paired twice.
    #[test]
    fn v2_link_enrolls_with_a_stable_key_and_activation_retires_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boxes.json");
        let secrets = Arc::new(MemSecrets::default());
        let mut s = Store::load(&path).unwrap();
        let a = s.add(&*secrets, &link_v2(1), "Home", 0).unwrap();
        assert_eq!(a.auth, Auth::Enrolling);
        assert_eq!(
            a.box_fp.as_deref(),
            Some(link_v2(1).box_fingerprint().unwrap().as_str())
        );
        assert_eq!(a.link_expires_at, Some(1_800_000_000));
        // Same box, different link: refused by box fingerprint.
        let err = s.add(&*secrets, &link_v2(2), "Twice", 0).unwrap_err();
        assert!(err.to_string().contains("already have this box"), "{err}");

        // Enrolling: a `Link` credential whose key is created once and
        // reused (a crash between request and grant must not change it).
        let DeviceCredential::Link { link, device_key } = s.credential(&*secrets, &a.id).unwrap()
        else {
            panic!("expected a Link credential");
        };
        assert!(link.is_v2());
        let again = s.credential(&*secrets, &a.id).unwrap();
        let DeviceCredential::Link { device_key: k2, .. } = again else {
            panic!()
        };
        assert_eq!(device_key.to_bytes(), k2.to_bytes());
        assert!(secrets.get(&device_key_key(&a.id)).unwrap().is_some());

        // The box granted R: the writer stores `box2:` and the credential
        // is used from then on even though the record still says
        // `Enrolling` (JSON save lost to a crash).
        let cred = EnrolledCredential::new(
            RendezvousSecret::from_bytes([9; 32]),
            device_key.clone(),
            box_key(),
            "relay.example.com",
        );
        enrolled_writer(secrets.clone(), &a.id)(&cred).unwrap();
        assert_eq!(s.get(&a.id).unwrap().auth, Auth::Enrolling);
        let DeviceCredential::Enrolled(got) = s.credential(&*secrets, &a.id).unwrap() else {
            panic!("box2: must win");
        };
        assert_eq!(got.device_public_key(), cred.device_public_key());
        assert_eq!(got.box_key(), box_key());

        s.set_enrolled(&a.id, &got.box_fingerprint()).unwrap();
        assert_eq!(
            Store::load(&path).unwrap().get(&a.id).unwrap().auth,
            Auth::Enrolled
        );
        s.finish_activation(&*secrets, &a.id).unwrap();
        let keys = secrets.0.lock().unwrap().clone();
        assert_eq!(
            keys.keys().collect::<Vec<_>>(),
            vec![&credential_key(&a.id)]
        );
        assert!(matches!(
            s.credential(&*secrets, &a.id).unwrap(),
            DeviceCredential::Enrolled(_)
        ));
    }

    /// A pairing from before v2 (only `box:`, no `auth` in the JSON) keeps
    /// working as legacy — with an upgrade key — and becomes enrolled the
    /// same way once a new box grants one.
    #[test]
    fn legacy_pairing_loads_as_legacy_and_upgrades_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boxes.json");
        std::fs::write(
            &path,
            r#"{
  "version": 1,
  "boxes": [
    { "id": "aa", "name": "A", "relay": "relay.example.com", "port": 41000,
      "fingerprint": "00", "addedAt": 1 }
  ],
  "next_port": 41001
}"#,
        )
        .unwrap();
        let secrets = MemSecrets::default();
        secrets.set(&secret_key("aa"), &link(1).to_uri()).unwrap();
        let mut s = Store::load(&path).unwrap();
        let rec = s.get("aa").unwrap();
        assert_eq!(
            (rec.auth, rec.box_fp.clone(), rec.link_expires_at),
            (Auth::Legacy, None, None)
        );

        let DeviceCredential::Legacy {
            link: l,
            upgrade_key: Some(key),
        } = s.credential(&secrets, "aa").unwrap()
        else {
            panic!("expected Legacy with an upgrade key");
        };
        assert!(!l.is_v2());
        assert!(secrets.get(&device_key_key("aa")).unwrap().is_some());

        // The box upgraded it: credential stored, then activated.
        let cred = EnrolledCredential::new(
            RendezvousSecret::from_bytes([9; 32]),
            key,
            box_key(),
            "relay.example.com",
        );
        secrets.set(&credential_key("aa"), &cred.encode()).unwrap();
        s.set_enrolled("aa", &cred.box_fingerprint()).unwrap();
        s.finish_activation(&secrets, "aa").unwrap();
        assert!(matches!(
            s.credential(&secrets, "aa").unwrap(),
            DeviceCredential::Enrolled(_)
        ));
        assert!(secrets.get(&secret_key("aa")).unwrap().is_none());
        let s = Store::load(&path).unwrap();
        assert_eq!(s.get("aa").unwrap().auth, Auth::Enrolled);
        assert_eq!(s.get("aa").unwrap().box_fp, Some(cred.box_fingerprint()));
    }

    /// `enrolled_writer` writes `box2:<id>` as the encoded credential.
    #[test]
    fn enrolled_writer_stores_the_credential_under_box2() {
        let secrets = Arc::new(MemSecrets::default());
        let cred = EnrolledCredential::new(
            RendezvousSecret::from_bytes([9; 32]),
            SigningKey::from_bytes(&[3; 32]),
            box_key(),
            "relay.example.com",
        );
        enrolled_writer(secrets.clone(), "id1")(&cred).unwrap();
        let stored = secrets.get("box2:id1").unwrap().unwrap();
        assert!(stored.starts_with(EnrolledCredential::PREFIX));
        assert_eq!(
            EnrolledCredential::parse(&stored)
                .unwrap()
                .device_public_key(),
            cred.device_public_key()
        );
    }

    #[test]
    fn next_port_is_the_high_water_mark_until_the_range_is_spent() {
        assert_eq!(next_port(PORT_BASE, []), Some(PORT_BASE));
        // Gaps below the mark are retired ports, never reused.
        assert_eq!(next_port(41005, [41000, 41002]), Some(41005));
        assert_eq!(next_port(PORT_LAST, [PORT_BASE]), Some(PORT_LAST));
        // Every port used once: the lowest one no current box holds.
        assert_eq!(next_port(PORT_LAST + 1, [41000, 41001]), Some(41002));
        assert_eq!(next_port(PORT_LAST + 1, PORT_BASE..=PORT_LAST), None);
        assert_eq!(next_port(u16::MAX, []), Some(PORT_BASE));
    }

    #[test]
    fn ports_are_fixed_unique_and_never_reused_after_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boxes.json");
        let secrets = MemSecrets::default();
        let mut s = Store::load(&path).unwrap();
        let a = s.add(&secrets, &link(1), "A", 0).unwrap();
        let b = s.add(&secrets, &link(2), "B", 0).unwrap();
        let c = s.add(&secrets, &link(3), "C", 0).unwrap();
        assert_eq!((a.port, b.port, c.port), (41000, 41001, 41002));
        s.remove(&secrets, &b.id).unwrap();
        // Survivors keep their ports across reloads; the gap stays retired.
        let mut s = Store::load(&path).unwrap();
        assert_eq!(s.get(&c.id).unwrap().port, 41002);
        let d = s.add(&secrets, &link(4), "D", 0).unwrap();
        assert_eq!(d.port, 41003);
        // Even with every box gone, old ports stay retired.
        for id in [a.id, c.id, d.id] {
            s.remove(&secrets, &id).unwrap();
        }
        assert!(s.boxes().is_empty());
        let mut s = Store::load(&path).unwrap();
        assert_eq!(s.add(&secrets, &link(5), "E", 0).unwrap().port, 41004);
    }

    #[test]
    fn old_boxes_json_without_a_mark_retires_every_port_up_to_the_highest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boxes.json");
        // Written by a version that reused ports: no `nextPort`, no
        // `micAllowed`, and a gap at 41001 (a removed box's port).
        std::fs::write(
            &path,
            r#"{
  "version": 1,
  "boxes": [
    { "id": "aa", "name": "A", "relay": "relay.example.com", "port": 41000,
      "fingerprint": "00", "addedAt": 1 },
    { "id": "cc", "name": "C", "relay": "relay.example.com", "port": 41002,
      "fingerprint": "02", "addedAt": 3, "lastConnectedAt": 9 }
  ]
}"#,
        )
        .unwrap();
        let secrets = MemSecrets::default();
        let mut s = Store::load(&path).unwrap();
        assert_eq!(s.boxes().len(), 2);
        assert_eq!(s.get("cc").unwrap().mic_allowed, None);
        assert_eq!(s.get("cc").unwrap().last_connected_at, Some(9));
        // 41001 may have belonged to a removed box: skipped.
        let d = s.add(&secrets, &link(4), "D", 0).unwrap();
        assert_eq!(d.port, 41003);

        // The mark is now recorded and survives a reload — and a mark that
        // somehow fell behind the ports in use is corrected on load.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"next_port\": 41004"), "{raw}");
        let s = Store::load(&path).unwrap();
        assert_eq!(s.next_port, 41004);
        let behind = raw.replace("\"next_port\": 41004", "\"next_port\": 41000");
        std::fs::write(&path, behind).unwrap();
        assert_eq!(Store::load(&path).unwrap().next_port, 41004);
    }

    #[test]
    fn mic_decision_is_remembered_per_box() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boxes.json");
        let secrets = MemSecrets::default();
        let mut s = Store::load(&path).unwrap();
        let a = s.add(&secrets, &link(1), "A", 0).unwrap();
        let b = s.add(&secrets, &link(2), "B", 0).unwrap();
        assert_eq!(a.mic_allowed, None);
        s.set_mic_allowed(&a.id, Some(true)).unwrap();
        s.set_mic_allowed(&b.id, Some(false)).unwrap();
        assert!(s.set_mic_allowed("nope", Some(true)).is_err());

        let mut s = Store::load(&path).unwrap();
        assert_eq!(s.get(&a.id).unwrap().mic_allowed, Some(true));
        assert_eq!(s.get(&b.id).unwrap().mic_allowed, Some(false));
        s.set_mic_allowed(&a.id, None).unwrap();
        assert_eq!(
            Store::load(&path).unwrap().get(&a.id).unwrap().mic_allowed,
            None
        );
    }
}
