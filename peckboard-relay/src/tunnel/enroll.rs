//! Enrollment stream ([`STREAM_ENROLL`](super::STREAM_ENROLL), `0x03`):
//! a device proves it holds `S` (by the TLS handshake) and its own key `D`
//! (by signing), and the box answers with the rendezvous secret `R`, signed
//! by the box identity `B`. Both signatures cover the connection's TLS
//! exporter, so a captured message is useless on any other connection.
//!
//! After the type byte the stream carries frames `u16 BE len ‖ body`
//! (len ≤ [`MAX_FRAME`]), `body = msg u8 ‖ fields`:
//!
//! | msg | dir | fields |
//! | --- | --- | --- |
//! | `0x01` request | D→B | `ver ‖ mode ‖ D[32] ‖ name_len ‖ name ‖ sig_D[64]` |
//! | `0x02` grant | B→D | `ver ‖ R[32] ‖ B[32] ‖ sig_B[64]` |
//! | `0x03` ack | D→B | `ver` |
//! | `0x04` refused | B→D | `ver ‖ reason` |

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use quinn::{Connection, RecvStream, SendStream, VarInt};
use sha2::{Digest, Sha256};

use super::cred::{EnrollMode, EnrolledCredential, RefuseReason};
use super::{CLOSE_ENROLLED, HEADER_TIMEOUT, STREAM_ENROLL};
use crate::identity::{BoxIdentity, is_valid_public_key};
use crate::keys::RendezvousSecret;

/// Largest frame body.
pub const MAX_FRAME: usize = 1024;
/// Longest device name, in bytes.
pub const MAX_NAME: usize = 64;
/// TLS exporter label of the channel binding `X`.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-peckboard-enroll";
const VERSION: u8 = 1;
const REQUEST_CONTEXT: &[u8] = b"peckboard-enroll/1";
const GRANT_CONTEXT: &[u8] = b"peckboard-grant/1";

const MSG_REQUEST: u8 = 0x01;
const MSG_GRANT: u8 = 0x02;
const MSG_ACK: u8 = 0x03;
const MSG_REFUSED: u8 = 0x04;

/// Box-side enrollment hook. The library does framing, channel binding and
/// signatures; the implementation owns the state (Peckboard: the DB).
#[async_trait::async_trait]
pub trait EnrollHandler: Send + Sync {
    /// A verified request (signature and channel binding checked). Persist
    /// durably before returning `Ok`. Must be idempotent: the same
    /// `device_key` again gets the same `R` (crash recovery); a different
    /// key for a used link gets [`RefuseReason::AlreadyUsed`].
    async fn enroll(
        &self,
        mode: EnrollMode,
        device_key: [u8; 32],
        name: &str,
        from: SocketAddr,
    ) -> Result<RendezvousSecret, RefuseReason>;

    /// The device stored the grant (informational; activation is what
    /// counts).
    async fn acked(&self, _device_key: [u8; 32]) {}

    /// A request or old-app connection reached a refusing link loop.
    async fn link_reuse(&self, _from: SocketAddr, _reason: RefuseReason) {}

    /// An `Enrolled` loop accepted the device's handshake. Called on every
    /// accepted connection (the first one is the activation); make it
    /// idempotent.
    async fn activated(&self, _device_key: [u8; 32], _from: SocketAddr) {}
}

// ---- codec --------------------------------------------------------------

/// `EnrollRequest`. `signature` = `Sig_D(request_message(..))`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollRequest {
    pub mode: EnrollMode,
    pub device_key: [u8; 32],
    pub name: String,
    pub signature: [u8; 64],
}

/// `EnrollGrant`. `signature` = `Sig_B(grant_message(..))`.
#[derive(Clone)]
pub struct EnrollGrant {
    pub r: RendezvousSecret,
    pub box_key: [u8; 32],
    pub signature: [u8; 64],
}

impl fmt::Debug for EnrollGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnrollGrant").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub enum EnrollMsg {
    Request(EnrollRequest),
    Grant(EnrollGrant),
    Ack,
    Refused(RefuseReason),
}

/// `"peckboard-enroll/1" ‖ X ‖ mode ‖ D ‖ name`.
pub fn request_message(
    x: &[u8; 32],
    mode: EnrollMode,
    device_key: &[u8; 32],
    name: &str,
) -> Vec<u8> {
    let mut m = Vec::with_capacity(REQUEST_CONTEXT.len() + 65 + name.len());
    m.extend_from_slice(REQUEST_CONTEXT);
    m.extend_from_slice(x);
    m.push(mode as u8);
    m.extend_from_slice(device_key);
    m.extend_from_slice(name.as_bytes());
    m
}

/// `"peckboard-grant/1" ‖ X ‖ D ‖ B ‖ SHA-256(R)`.
pub fn grant_message(
    x: &[u8; 32],
    device_key: &[u8; 32],
    box_key: &[u8; 32],
    r: &RendezvousSecret,
) -> Vec<u8> {
    let mut m = Vec::with_capacity(GRANT_CONTEXT.len() + 128);
    m.extend_from_slice(GRANT_CONTEXT);
    m.extend_from_slice(x);
    m.extend_from_slice(device_key);
    m.extend_from_slice(box_key);
    m.extend_from_slice(&Sha256::digest(r.as_bytes()));
    m
}

fn verify(key: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    if !is_valid_public_key(key) {
        return false;
    }
    let Ok(vk) = VerifyingKey::from_bytes(key) else {
        return false;
    };
    vk.verify_strict(msg, &Signature::from_bytes(sig)).is_ok()
}

/// `name` cut to [`MAX_NAME`] bytes on a char boundary.
pub fn clamp_name(name: &str) -> &str {
    let mut end = name.len().min(MAX_NAME);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

impl EnrollRequest {
    pub fn sign(x: &[u8; 32], mode: EnrollMode, key: &SigningKey, name: &str) -> Self {
        let name = clamp_name(name).to_string();
        let device_key = key.verifying_key().to_bytes();
        let signature = key
            .sign(&request_message(x, mode, &device_key, &name))
            .to_bytes();
        Self {
            mode,
            device_key,
            name,
            signature,
        }
    }

    /// Signed by `device_key` for the connection with exporter `x`.
    pub fn verify(&self, x: &[u8; 32]) -> bool {
        verify(
            &self.device_key,
            &request_message(x, self.mode, &self.device_key, &self.name),
            &self.signature,
        )
    }
}

impl EnrollGrant {
    pub fn sign(
        x: &[u8; 32],
        device_key: &[u8; 32],
        identity: &BoxIdentity,
        r: RendezvousSecret,
    ) -> Self {
        let box_key = identity.public_key();
        let signature = identity
            .signing_key()
            .sign(&grant_message(x, device_key, &box_key, &r))
            .to_bytes();
        Self {
            r,
            box_key,
            signature,
        }
    }

    /// Signed by `box_key` for `device_key` on the connection with exporter
    /// `x`.
    pub fn verify(&self, x: &[u8; 32], device_key: &[u8; 32]) -> bool {
        verify(
            &self.box_key,
            &grant_message(x, device_key, &self.box_key, &self.r),
            &self.signature,
        )
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        if self.0.len() < n {
            bail!("enroll: truncated message");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn arr<const N: usize>(&mut self) -> anyhow::Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }
}

impl EnrollMsg {
    /// Frame body (without the length prefix).
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(160);
        match self {
            Self::Request(r) => {
                b.extend_from_slice(&[MSG_REQUEST, VERSION, r.mode as u8]);
                b.extend_from_slice(&r.device_key);
                b.push(r.name.len() as u8);
                b.extend_from_slice(r.name.as_bytes());
                b.extend_from_slice(&r.signature);
            }
            Self::Grant(g) => {
                b.extend_from_slice(&[MSG_GRANT, VERSION]);
                b.extend_from_slice(g.r.as_bytes());
                b.extend_from_slice(&g.box_key);
                b.extend_from_slice(&g.signature);
            }
            Self::Ack => b.extend_from_slice(&[MSG_ACK, VERSION]),
            Self::Refused(r) => b.extend_from_slice(&[MSG_REFUSED, VERSION, r.as_u8()]),
        }
        b
    }

    /// Inverse of [`encode`](Self::encode); rejects truncation, trailing
    /// bytes, an unknown version / message / mode and an over-long name.
    pub fn decode(body: &[u8]) -> anyhow::Result<Self> {
        if body.len() > MAX_FRAME {
            bail!("enroll: message too large");
        }
        let mut r = Reader(body);
        let msg = r.u8()?;
        if r.u8()? != VERSION {
            bail!("enroll: unsupported version");
        }
        let out = match msg {
            MSG_REQUEST => {
                let mode =
                    EnrollMode::from_u8(r.u8()?).ok_or_else(|| anyhow!("enroll: bad mode"))?;
                let device_key = r.arr()?;
                let len = r.u8()? as usize;
                if len > MAX_NAME {
                    bail!("enroll: name too long");
                }
                let name = std::str::from_utf8(r.take(len)?)
                    .map_err(|_| anyhow!("enroll: name is not UTF-8"))?
                    .to_string();
                Self::Request(EnrollRequest {
                    mode,
                    device_key,
                    name,
                    signature: r.arr()?,
                })
            }
            MSG_GRANT => Self::Grant(EnrollGrant {
                r: RendezvousSecret::from_bytes(r.arr()?),
                box_key: r.arr()?,
                signature: r.arr()?,
            }),
            MSG_ACK => Self::Ack,
            MSG_REFUSED => Self::Refused(RefuseReason::from_u8(r.u8()?)),
            _ => bail!("enroll: unknown message"),
        };
        if !r.0.is_empty() {
            bail!("enroll: trailing bytes");
        }
        Ok(out)
    }

    /// `u16 BE len ‖ body`.
    pub fn frame(&self) -> Vec<u8> {
        let body = self.encode();
        let mut f = Vec::with_capacity(2 + body.len());
        f.extend_from_slice(&(body.len() as u16).to_be_bytes());
        f.extend_from_slice(&body);
        f
    }
}

pub async fn write_msg(send: &mut SendStream, msg: &EnrollMsg) -> anyhow::Result<()> {
    send.write_all(&msg.frame()).await?;
    Ok(())
}

/// One frame, within [`HEADER_TIMEOUT`](super::HEADER_TIMEOUT). Read
/// errors stay downcastable to [`quinn::ReadExactError`].
pub async fn read_msg(recv: &mut RecvStream) -> anyhow::Result<EnrollMsg> {
    let read = async {
        let mut len = [0u8; 2];
        recv.read_exact(&mut len).await?;
        let len = u16::from_be_bytes(len) as usize;
        if len > MAX_FRAME {
            bail!("enroll: frame too large");
        }
        let mut body = vec![0u8; len];
        recv.read_exact(&mut body).await?;
        EnrollMsg::decode(&body)
    };
    tokio::time::timeout(HEADER_TIMEOUT, read)
        .await
        .map_err(|_| anyhow!("enroll: timed out"))?
}

/// The channel binding `X` of `conn` (same on both ends).
pub fn exporter(conn: &Connection) -> anyhow::Result<[u8; 32]> {
    let mut x = [0u8; 32];
    conn.export_keying_material(&mut x, EXPORTER_LABEL, b"")
        .map_err(|_| anyhow!("TLS exporter unavailable"))?;
    Ok(x)
}

// ---- box ----------------------------------------------------------------

/// Which enrollments a box connection accepts.
#[derive(Clone, Copy, Debug)]
pub(super) enum EnrollGate {
    /// A v2 link connection: mode 1 only; `refuse` answers every request.
    Link { refuse: Option<RefuseReason> },
    /// A legacy connection: mode 2 only.
    Upgrade,
}

/// Per-connection state an enrollment stream needs.
pub(super) struct BoxEnroll {
    pub conn: Connection,
    pub gate: EnrollGate,
    pub identity: BoxIdentity,
    pub handler: Option<Arc<dyn EnrollHandler>>,
}

fn reset(send: &mut SendStream, recv: &mut RecvStream) {
    let _ = send.reset(VarInt::from_u32(1));
    let _ = recv.stop(VarInt::from_u32(1));
}

/// Box half of one `0x03` stream (type byte already read).
pub(super) async fn serve(mut send: SendStream, mut recv: RecvStream, ctx: &BoxEnroll) {
    let from = ctx.conn.remote_address();
    let Ok(x) = exporter(&ctx.conn) else {
        return reset(&mut send, &mut recv);
    };
    let req = match read_msg(&mut recv).await {
        Ok(EnrollMsg::Request(r)) if r.verify(&x) => r,
        // Wrong message, bad signature, or bound to another connection
        // (replay): nothing reaches the handler.
        _ => {
            tracing::debug!(%from, "tunnel: rejected enrollment request");
            return reset(&mut send, &mut recv);
        }
    };
    let expected = match ctx.gate {
        EnrollGate::Link { .. } => EnrollMode::Link,
        EnrollGate::Upgrade => EnrollMode::LegacyUpgrade,
    };
    let refuse = |reason: RefuseReason| EnrollMsg::Refused(reason);
    let reply = if req.mode != expected {
        refuse(RefuseReason::NotEnrollable)
    } else if let EnrollGate::Link {
        refuse: Some(reason),
    } = ctx.gate
    {
        if let Some(h) = &ctx.handler {
            h.link_reuse(from, reason).await;
        }
        refuse(reason)
    } else if let Some(h) = &ctx.handler {
        match h.enroll(req.mode, req.device_key, &req.name, from).await {
            Ok(r) => EnrollMsg::Grant(EnrollGrant::sign(&x, &req.device_key, &ctx.identity, r)),
            Err(reason) => refuse(reason),
        }
    } else {
        refuse(RefuseReason::Internal)
    };
    let granted = matches!(reply, EnrollMsg::Grant(_));
    if write_msg(&mut send, &reply).await.is_err() {
        return;
    }
    let _ = send.finish();
    if !granted {
        return;
    }
    if let Ok(EnrollMsg::Ack) = read_msg(&mut recv).await {
        tracing::info!(%from, mode = ?req.mode, "tunnel: device enrolled");
        if let Some(h) = &ctx.handler {
            h.acked(req.device_key).await;
        }
        if matches!(ctx.gate, EnrollGate::Link { .. }) {
            ctx.conn
                .close(VarInt::from_u32(CLOSE_ENROLLED), b"enrolled");
        }
    }
}

// ---- device -------------------------------------------------------------

/// How a device-side enrollment ended.
pub(super) enum Outcome {
    /// Granted, persisted (`on_enrolled`), acknowledged.
    Enrolled(Box<EnrolledCredential>),
    Refused(RefuseReason),
    /// The box doesn't know `0x03` (reset the stream): an old box.
    Unsupported,
    /// `on_enrolled` failed: nothing acknowledged, retry later.
    SaveFailed(String),
}

/// Run one enrollment on `conn`. `expect_box`: the pinned box key (a v2
/// link); `None` trusts the key the grant is signed with (legacy upgrade,
/// inside an `S`-authenticated channel).
pub(super) async fn device_enroll(
    conn: &Connection,
    mode: EnrollMode,
    device_key: &SigningKey,
    name: &str,
    expect_box: Option<[u8; 32]>,
    relay: &str,
    on_enrolled: Option<&super::OnEnrolled>,
) -> anyhow::Result<Outcome> {
    let x = exporter(conn)?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[STREAM_ENROLL]).await?;
    let req = EnrollRequest::sign(&x, mode, device_key, name);
    write_msg(&mut send, &EnrollMsg::Request(req)).await?;
    let reply = match read_msg(&mut recv).await {
        Ok(m) => m,
        Err(e) => {
            let reset = matches!(
                e.downcast_ref::<quinn::ReadExactError>(),
                Some(quinn::ReadExactError::ReadError(quinn::ReadError::Reset(_)))
            );
            return if reset {
                Ok(Outcome::Unsupported)
            } else {
                Err(e)
            };
        }
    };
    match reply {
        EnrollMsg::Grant(g) => {
            let d = device_key.verifying_key().to_bytes();
            if expect_box.is_some_and(|b| b != g.box_key) {
                bail!("enrollment grant is not from the pinned box key");
            }
            if !g.verify(&x, &d) {
                bail!("enrollment grant signature is invalid");
            }
            let cred = EnrolledCredential::new(g.r, device_key.clone(), g.box_key, relay);
            if let Some(cb) = on_enrolled
                && let Err(e) = cb(&cred)
            {
                return Ok(Outcome::SaveFailed(format!(
                    "Couldn't save the pairing key: {e:#}"
                )));
            }
            write_msg(&mut send, &EnrollMsg::Ack).await?;
            let _ = send.finish();
            Ok(Outcome::Enrolled(Box::new(cred)))
        }
        EnrollMsg::Refused(r) => Ok(Outcome::Refused(r)),
        _ => bail!("unexpected enrollment message"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_roundtrip_and_rejects() {
        let key = SigningKey::from_bytes(&[3; 32]);
        let x = [7u8; 32];
        let req = EnrollRequest::sign(&x, EnrollMode::Link, &key, "iPhone");
        assert!(req.verify(&x));
        assert!(!req.verify(&[8; 32]));
        let id = BoxIdentity::from_seed([5; 32]);
        let grant = EnrollGrant::sign(
            &x,
            &req.device_key,
            &id,
            RendezvousSecret::from_bytes([1; 32]),
        );
        assert!(grant.verify(&x, &req.device_key));
        assert!(!grant.verify(&x, &[9; 32]));
        for m in [
            EnrollMsg::Request(req.clone()),
            EnrollMsg::Grant(grant),
            EnrollMsg::Ack,
            EnrollMsg::Refused(RefuseReason::AlreadyUsed),
        ] {
            let b = m.encode();
            let d = EnrollMsg::decode(&b).unwrap();
            assert_eq!(d.encode(), b);
            // Truncation and trailing bytes.
            assert!(EnrollMsg::decode(&b[..b.len() - 1]).is_err());
            let mut t = b.clone();
            t.push(0);
            assert!(EnrollMsg::decode(&t).is_err());
        }
        assert!(EnrollMsg::decode(&vec![MSG_ACK; MAX_FRAME + 1]).is_err());
        assert!(EnrollMsg::decode(&[0x09, VERSION]).is_err());
        assert!(EnrollMsg::decode(&[MSG_ACK, 2]).is_err());
        // Over-long name.
        let mut long = req;
        long.name = "x".repeat(MAX_NAME + 1);
        assert!(EnrollMsg::decode(&EnrollMsg::Request(long).encode()).is_err());
        assert_eq!(clamp_name(&"é".repeat(40)).len(), 64);
    }
}
