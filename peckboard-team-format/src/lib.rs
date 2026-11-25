//! Teams of PeckBoard servers, with no accounts.
//!
//! A team is an append-only **membership log**. Every record is signed by
//! the server that made the change and names the record before it, so the
//! log can only grow, and anyone holding it can replay it and work out who
//! is in the team and with what role — without trusting whoever stored it.
//! The team's id is a hash of its first record, so a different first record
//! is a different team.
//!
//! - [`Record`] / [`Action`]: one signed change.
//! - [`Team::apply`] / [`replay`]: the rules (who may add, remove, promote).
//! - [`Invite`]: the invite link, and what the joiner and the approver
//!   derive from its secret ([`gate`], [`join_proof`], [`verification_code`]).
//!
//! A member is a server, named by its Ed25519 public key (standard base64
//! of the 32 bytes — the same text form the skills hub uses).

use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Signed-message prefix of a log record.
pub const LOG_CONTEXT: &str = "peckboard-team-log-v1";
const TEAM_ID_CONTEXT: &str = "peckboard-team-id-v1";
const GATE_CONTEXT: &str = "peckboard-team-gate-v1";
const JOIN_CONTEXT: &str = "peckboard-team-join-v1";
const VERIFY_CONTEXT: &str = "peckboard-team-verify-v1";

/// Longest team name / server label, in chars.
pub const MAX_NAME_CHARS: usize = 64;
/// Most servers one team may hold.
pub const MAX_MEMBERS: usize = 200;
/// Largest record body, in bytes.
pub const MAX_BODY_BYTES: usize = 2048;

/// What a member may do. Owners can do everything; admins can add and
/// remove plain members; members can only leave.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }
}

/// One change to a team — the signed body of a [`Record`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// The first record. `founder` signs it and becomes the first owner;
    /// `label` is the founder server's display name. `salt` makes the team
    /// id unique even for two teams created alike.
    Create {
        name: String,
        founder: String,
        label: String,
        salt: String,
    },
    /// Add the server `key` with `role`, shown as `label`.
    Add {
        key: String,
        role: Role,
        label: String,
    },
    /// Remove the server `key` (a member may remove itself: leaving).
    Remove { key: String },
    /// Change a member's role.
    SetRole { key: String, role: Role },
    /// Rename the team.
    Rename { name: String },
}

/// One signed entry of the membership log, exactly as stored and served.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Position in the log, from 0.
    pub seq: u64,
    /// [`Record::hash`] of the record before this one; empty for the first.
    pub prev: String,
    /// When the signer made the change (RFC 3339). Signed, not checked.
    pub at: String,
    /// The [`Action`] as the exact JSON text that was signed.
    pub body: String,
    /// Public key of the signing server.
    pub signer: String,
    /// Its Ed25519 signature (standard base64) over [`Record::message`].
    pub signature: String,
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// The text a record's signer signs.
pub fn log_message(seq: u64, prev: &str, at: &str, body: &str) -> String {
    format!(
        "{LOG_CONTEXT}\n{seq}\n{prev}\n{at}\n{}",
        sha256_hex(body.as_bytes())
    )
}

impl Record {
    /// The text this record's signature covers.
    pub fn message(&self) -> String {
        log_message(self.seq, &self.prev, &self.at, &self.body)
    }

    /// What the next record must carry as `prev`. Covers the signer and the
    /// signature too, so two different signatures over the same change are
    /// two different histories.
    pub fn hash(&self) -> String {
        sha256_hex(format!("{}\n{}\n{}", self.message(), self.signer, self.signature).as_bytes())
    }

    pub fn action(&self) -> Result<Action, LogError> {
        if self.body.len() > MAX_BODY_BYTES {
            return Err(LogError::Malformed("the record is too large".into()));
        }
        serde_json::from_str(&self.body)
            .map_err(|e| LogError::Malformed(format!("the record body is not a team change: {e}")))
    }
}

/// The id of the team a first record founds: 32 hex chars.
pub fn team_id(first_body: &str) -> String {
    let mut id = sha256_hex(format!("{TEAM_ID_CONTEXT}\n{first_body}").as_bytes());
    id.truncate(32);
    id
}

/// Why a record was refused. The text is written for the person looking at
/// the Team settings, not for a log file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogError {
    /// Not a well-formed record.
    Malformed(String),
    /// The signature does not belong to the signer and this record.
    BadSignature,
    /// Not the next record of this log (wrong `seq` or `prev`).
    OutOfOrder,
    /// The signer is not allowed to make this change.
    NotAllowed(String),
    /// The change makes no sense for the team as it stands.
    Invalid(String),
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogError::Malformed(m) | LogError::NotAllowed(m) | LogError::Invalid(m) => {
                f.write_str(m)
            }
            LogError::BadSignature => {
                f.write_str("The signature on this team change does not match its signer.")
            }
            LogError::OutOfOrder => {
                f.write_str("The team changed in the meantime. Reload the team and try again.")
            }
        }
    }
}

impl std::error::Error for LogError {}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub role: Role,
    pub label: String,
    /// `at` of the record that added this server.
    pub added_at: String,
}

/// A team as its log says it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Team {
    pub id: String,
    pub name: String,
    /// Members by public key.
    pub members: BTreeMap<String, Member>,
    /// How many records the log holds (the next record's `seq`).
    pub len: u64,
    /// [`Record::hash`] of the last record (the next record's `prev`).
    pub head: String,
}

/// A display name: trimmed, 1..=64 chars, no control characters.
pub fn clean_name(raw: &str, what: &str) -> Result<String, LogError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(LogError::Invalid(format!("{what} must not be empty.")));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(LogError::Invalid(format!(
            "{what} must be {MAX_NAME_CHARS} characters or fewer."
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(LogError::Invalid(format!(
            "{what} must not contain control characters."
        )));
    }
    Ok(name.to_string())
}

fn check_name(stored: &str, what: &str) -> Result<(), LogError> {
    if clean_name(stored, what)? != stored {
        return Err(LogError::Invalid(format!(
            "{what} must not start or end with spaces."
        )));
    }
    Ok(())
}

impl Team {
    fn owners(&self) -> usize {
        self.members
            .values()
            .filter(|m| m.role == Role::Owner)
            .count()
    }

    pub fn role_of(&self, key: &str) -> Option<Role> {
        self.members.get(key).map(|m| m.role)
    }

    /// The team after `record`, or why the record is refused. `team` is
    /// `None` for a log's first record. `verify(key, message, signature)`
    /// is the caller's Ed25519 check; it must return false for a key or
    /// signature that does not parse.
    pub fn apply(
        team: Option<&Team>,
        record: &Record,
        verify: &dyn Fn(&str, &str, &str) -> bool,
    ) -> Result<Team, LogError> {
        let action = record.action()?;
        let signed = |r: &Record| verify(&r.signer, &r.message(), &r.signature);

        let Some(team) = team else {
            let Action::Create {
                name,
                founder,
                label,
                salt,
            } = &action
            else {
                return Err(LogError::Invalid(
                    "A team's first record must create it.".into(),
                ));
            };
            if record.seq != 0 || !record.prev.is_empty() {
                return Err(LogError::OutOfOrder);
            }
            if record.signer != *founder {
                return Err(LogError::NotAllowed(
                    "A team must be created by its founder's own key.".into(),
                ));
            }
            check_name(name, "The team name")?;
            check_name(label, "The server name")?;
            if salt.len() < 16 || salt.len() > 64 {
                return Err(LogError::Malformed("the team salt has a bad length".into()));
            }
            if !signed(record) {
                return Err(LogError::BadSignature);
            }
            let mut members = BTreeMap::new();
            members.insert(
                founder.clone(),
                Member {
                    role: Role::Owner,
                    label: label.clone(),
                    added_at: record.at.clone(),
                },
            );
            return Ok(Team {
                id: team_id(&record.body),
                name: name.clone(),
                members,
                len: 1,
                head: record.hash(),
            });
        };

        if record.seq != team.len || record.prev != team.head {
            return Err(LogError::OutOfOrder);
        }
        let Some(signer_role) = team.role_of(&record.signer) else {
            return Err(LogError::NotAllowed(
                "Only a member of the team can change it.".into(),
            ));
        };
        let owner_only = |what: &str| {
            if signer_role == Role::Owner {
                Ok(())
            } else {
                Err(LogError::NotAllowed(format!("Only an owner can {what}.")))
            }
        };
        let mut next = team.clone();
        match action {
            Action::Create { .. } => {
                return Err(LogError::Invalid("This team already exists.".into()));
            }
            Action::Add { key, role, label } => {
                if signer_role == Role::Member {
                    return Err(LogError::NotAllowed(
                        "Only an owner or admin can add a server.".into(),
                    ));
                }
                if role != Role::Member {
                    owner_only("add an owner or admin")?;
                }
                if team.members.contains_key(&key) {
                    return Err(LogError::Invalid(
                        "That server is already in the team.".into(),
                    ));
                }
                if team.members.len() >= MAX_MEMBERS {
                    return Err(LogError::Invalid(format!(
                        "A team can hold at most {MAX_MEMBERS} servers."
                    )));
                }
                check_name(&label, "The server name")?;
                next.members.insert(
                    key,
                    Member {
                        role,
                        label,
                        added_at: record.at.clone(),
                    },
                );
            }
            Action::Remove { key } => {
                let Some(target) = team.role_of(&key) else {
                    return Err(LogError::Invalid("That server is not in the team.".into()));
                };
                if key != record.signer {
                    match (signer_role, target) {
                        (Role::Owner, _) | (Role::Admin, Role::Member) => {}
                        (Role::Admin, _) => owner_only("remove an owner or admin")?,
                        (Role::Member, _) => {
                            return Err(LogError::NotAllowed(
                                "Only an owner or admin can remove a server.".into(),
                            ));
                        }
                    }
                }
                if target == Role::Owner && team.owners() == 1 {
                    return Err(LogError::Invalid(
                        "A team needs at least one owner. Make another server an owner first."
                            .into(),
                    ));
                }
                next.members.remove(&key);
            }
            Action::SetRole { key, role } => {
                owner_only("change a server's role")?;
                let Some(current) = team.role_of(&key) else {
                    return Err(LogError::Invalid("That server is not in the team.".into()));
                };
                if current == Role::Owner && role != Role::Owner && team.owners() == 1 {
                    return Err(LogError::Invalid(
                        "A team needs at least one owner. Make another server an owner first."
                            .into(),
                    ));
                }
                if let Some(m) = next.members.get_mut(&key) {
                    m.role = role;
                }
            }
            Action::Rename { name } => {
                owner_only("rename the team")?;
                check_name(&name, "The team name")?;
                next.name = name;
            }
        }
        if !signed(record) {
            return Err(LogError::BadSignature);
        }
        next.len = team.len + 1;
        next.head = record.hash();
        Ok(next)
    }
}

/// Replay a whole log. Fails on the first record that breaks a rule; an
/// empty log is not a team.
pub fn replay(
    records: &[Record],
    verify: &dyn Fn(&str, &str, &str) -> bool,
) -> Result<Team, LogError> {
    let mut team: Option<Team> = None;
    for record in records {
        team = Some(Team::apply(team.as_ref(), record, verify)?);
    }
    team.ok_or_else(|| LogError::Malformed("the team has no records".into()))
}

// ── Invites ─────────────────────────────────────────────────────────────

/// An invite as carried by its link. The `secret` never goes to the hub:
/// the hub only ever sees [`gate`] of it, so the hub cannot make a join
/// request look like it came from someone who held the link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    /// Base URL of the team hub, no trailing slash.
    pub hub: String,
    pub team_id: String,
    pub invite_id: String,
    pub secret: String,
}

fn is_token(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Invite {
    /// `<hub>/join#<team_id>.<invite_id>.<secret>`. Everything after `#`
    /// stays in the browser / the pasted text and is never sent to a server
    /// by following the link.
    pub fn link(&self) -> String {
        format!(
            "{}/join#{}.{}.{}",
            self.hub, self.team_id, self.invite_id, self.secret
        )
    }

    /// Parse a pasted invite link. `None` when it is not one.
    pub fn parse(link: &str) -> Option<Invite> {
        let (base, fragment) = link.trim().split_once('#')?;
        let hub = base.strip_suffix("/join")?.trim_end_matches('/');
        if !(hub.starts_with("https://") || hub.starts_with("http://")) || hub.len() > 200 {
            return None;
        }
        let mut parts = fragment.split('.');
        let (team_id, invite_id, secret) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some()
            || team_id.len() != 32
            || !team_id.bytes().all(|b| b.is_ascii_hexdigit())
            || !is_token(invite_id, 8, 64)
            || !is_token(secret, 32, 128)
        {
            return None;
        }
        Some(Invite {
            hub: hub.to_string(),
            team_id: team_id.to_string(),
            invite_id: invite_id.to_string(),
            secret: secret.to_string(),
        })
    }
}

fn keyed(secret: &str, context: &str, team_id: &str, joiner_key: &str) -> Vec<u8> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(format!("{context}\n{team_id}\n{joiner_key}").as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// What the hub stores for an invite and a joiner presents to use it: shows
/// the joiner holds the link, tells the hub nothing about the secret.
pub fn gate(secret: &str) -> String {
    sha256_hex(format!("{GATE_CONTEXT}\n{secret}").as_bytes())
}

/// The joiner's proof, checked by the approving server (which made the
/// invite and so knows its secret): this key was put forward by someone
/// holding the link. The hub cannot forge it for another key.
pub fn join_proof(secret: &str, team_id: &str, joiner_key: &str) -> String {
    hex::encode(keyed(secret, JOIN_CONTEXT, team_id, joiner_key))
}

/// The six-digit code both people read out before the join is approved,
/// formatted `123 456`. It depends on the joining server's key, so a second
/// person who got hold of the link sees a different code than the invited
/// coworker does.
pub fn verification_code(secret: &str, team_id: &str, joiner_key: &str) -> String {
    let mac = keyed(secret, VERIFY_CONTEXT, team_id, joiner_key);
    let n = u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for Ed25519: the "signature" of `key` over `msg`.
    fn sign(key: &str, msg: &str) -> String {
        sha256_hex(format!("sig\n{key}\n{msg}").as_bytes())
    }

    fn verify(key: &str, msg: &str, sig: &str) -> bool {
        sign(key, msg) == sig
    }

    fn record(team: Option<&Team>, signer: &str, action: &Action) -> Record {
        let body = serde_json::to_string(action).unwrap();
        let (seq, prev) = team.map_or((0, String::new()), |t| (t.len, t.head.clone()));
        let at = "2026-11-25T10:00:00Z".to_string();
        let signature = sign(signer, &log_message(seq, &prev, &at, &body));
        Record {
            seq,
            prev,
            at,
            body,
            signer: signer.into(),
            signature,
        }
    }

    fn create(founder: &str) -> (Team, Vec<Record>) {
        let r = record(
            None,
            founder,
            &Action::Create {
                name: "Acme".into(),
                founder: founder.into(),
                label: "office".into(),
                salt: "0123456789abcdef".into(),
            },
        );
        (Team::apply(None, &r, &verify).unwrap(), vec![r])
    }

    fn step(
        team: &mut Team,
        log: &mut Vec<Record>,
        signer: &str,
        action: Action,
    ) -> Result<(), LogError> {
        let r = record(Some(team), signer, &action);
        *team = Team::apply(Some(team), &r, &verify)?;
        log.push(r);
        Ok(())
    }

    fn add(key: &str, role: Role) -> Action {
        Action::Add {
            key: key.into(),
            role,
            label: key.into(),
        }
    }

    #[test]
    fn a_team_grows_by_signed_records_and_replays_to_the_same_state() {
        let (mut team, mut log) = create("owner");
        assert_eq!(team.id, team_id(&log[0].body));
        assert_eq!(team.role_of("owner"), Some(Role::Owner));

        step(&mut team, &mut log, "owner", add("admin", Role::Admin)).unwrap();
        step(&mut team, &mut log, "admin", add("laptop", Role::Member)).unwrap();
        step(
            &mut team,
            &mut log,
            "owner",
            Action::Rename {
                name: "Acme Ltd".into(),
            },
        )
        .unwrap();
        step(
            &mut team,
            &mut log,
            "laptop",
            Action::Remove {
                key: "laptop".into(),
            },
        )
        .unwrap();

        let replayed = replay(&log, &verify).unwrap();
        assert_eq!(replayed, team);
        assert_eq!(replayed.name, "Acme Ltd");
        assert_eq!(replayed.members.len(), 2);
        assert_eq!(replayed.len, 5);
    }

    #[test]
    fn roles_limit_who_may_change_what() {
        let (mut team, mut log) = create("owner");
        step(&mut team, &mut log, "owner", add("admin", Role::Admin)).unwrap();
        step(&mut team, &mut log, "owner", add("member", Role::Member)).unwrap();

        let refused = |team: &Team, signer: &str, action: Action| {
            Team::apply(Some(team), &record(Some(team), signer, &action), &verify).unwrap_err()
        };
        // A stranger, and a plain member, cannot add anyone.
        assert!(matches!(
            refused(&team, "stranger", add("x", Role::Member)),
            LogError::NotAllowed(_)
        ));
        assert!(matches!(
            refused(&team, "member", add("x", Role::Member)),
            LogError::NotAllowed(_)
        ));
        // An admin cannot mint admins, remove the owner, or change roles.
        assert!(matches!(
            refused(&team, "admin", add("x", Role::Admin)),
            LogError::NotAllowed(_)
        ));
        assert!(matches!(
            refused(
                &team,
                "admin",
                Action::Remove {
                    key: "owner".into()
                }
            ),
            LogError::NotAllowed(_)
        ));
        assert!(matches!(
            refused(
                &team,
                "admin",
                Action::SetRole {
                    key: "member".into(),
                    role: Role::Admin
                }
            ),
            LogError::NotAllowed(_)
        ));
        // The last owner cannot leave or step down.
        assert!(matches!(
            refused(
                &team,
                "owner",
                Action::Remove {
                    key: "owner".into()
                }
            ),
            LogError::Invalid(_)
        ));
        assert!(matches!(
            refused(
                &team,
                "owner",
                Action::SetRole {
                    key: "owner".into(),
                    role: Role::Member
                }
            ),
            LogError::Invalid(_)
        ));
        // With a second owner, the first may leave.
        step(
            &mut team,
            &mut log,
            "owner",
            Action::SetRole {
                key: "admin".into(),
                role: Role::Owner,
            },
        )
        .unwrap();
        step(
            &mut team,
            &mut log,
            "owner",
            Action::Remove {
                key: "owner".into(),
            },
        )
        .unwrap();
        assert_eq!(team.role_of("admin"), Some(Role::Owner));
    }

    #[test]
    fn a_tampered_reordered_or_forged_log_is_refused() {
        let (mut team, mut log) = create("owner");
        step(&mut team, &mut log, "owner", add("member", Role::Member)).unwrap();
        step(
            &mut team,
            &mut log,
            "owner",
            Action::Remove {
                key: "member".into(),
            },
        )
        .unwrap();

        // Dropping the removal (a store hiding it) leaves a valid shorter
        // log — which is why a member also pins the newest head it has
        // seen. Dropping a middle record breaks the chain.
        assert!(replay(&log[..2], &verify).is_ok());
        let gap = [log[0].clone(), log[2].clone()];
        assert_eq!(replay(&gap, &verify).unwrap_err(), LogError::OutOfOrder);

        // A changed body no longer matches its signature.
        let mut forged = log.clone();
        forged[1].body = forged[1].body.replace("member", "admin");
        assert_eq!(
            replay(&forged, &verify).unwrap_err(),
            LogError::BadSignature
        );

        // A record signed by someone else in the signer's name.
        let mut stolen = log.clone();
        stolen[1].signature = sign("mallory", &stolen[1].message());
        assert_eq!(
            replay(&stolen, &verify).unwrap_err(),
            LogError::BadSignature
        );

        // A first record not signed by its founder founds nothing.
        let mut other = log[0].clone();
        other.signer = "mallory".into();
        other.signature = sign("mallory", &other.message());
        assert!(matches!(
            replay(&[other], &verify).unwrap_err(),
            LogError::NotAllowed(_)
        ));
        assert!(replay(&[], &verify).is_err());
    }

    #[test]
    fn invite_links_round_trip_and_reject_anything_else() {
        let invite = Invite {
            hub: "https://team.peckboard.com".into(),
            team_id: "0123456789abcdef0123456789abcdef".into(),
            invite_id: "inv_12345678".into(),
            secret: "s".repeat(43),
        };
        let link = invite.link();
        assert_eq!(Invite::parse(&format!("  {link}\n")), Some(invite.clone()));
        for bad in [
            "https://team.peckboard.com/join",
            "https://team.peckboard.com/join#short.inv_12345678.x",
            "ftp://team.peckboard.com/join#0123456789abcdef0123456789abcdef.inv_12345678.ssssssssssssssssssssssssssssssssssssssss",
            "not a link",
        ] {
            assert_eq!(Invite::parse(bad), None, "{bad}");
        }

        // The code and the proof depend on the joining key; the gate does not.
        let code = verification_code(&invite.secret, &invite.team_id, "key-a");
        assert_eq!(code.len(), 7);
        assert_ne!(
            code,
            verification_code(&invite.secret, &invite.team_id, "key-b")
        );
        assert_ne!(
            join_proof(&invite.secret, &invite.team_id, "key-a"),
            join_proof(&invite.secret, &invite.team_id, "key-b")
        );
        assert_ne!(gate(&invite.secret), gate("another secret"));
    }
}
