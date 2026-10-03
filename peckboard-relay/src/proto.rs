//! Signaling wire protocol, spoken inside TLS 1.3 under ALPN [`ALPN`] (or
//! [`ALPN_V2`], which adds the relay data channel).
//!
//! Frames are `u32` big-endian length + payload, payload = type byte +
//! fixed fields. Variable-length blobs carry a `u16` length prefix; a v2
//! datagram frame is the type byte followed by the raw packet. The codec is
//! hand-rolled (a dozen messages) to keep the attack surface and dependency
//! list small; every decode is bounds-checked and rejects trailing bytes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ALPN: &[u8] = b"peckrelay/1";
/// Protocol v2 = v1 + the relay data channel ([`ClientMsg::Data`] /
/// [`ServerMsg::Data`]). Negotiated by ALPN: a v2 client offers
/// `[ALPN_V2, ALPN]`, a v2 relay prefers [`ALPN_V2`]; an old relay only knows
/// [`ALPN`] and picks it, an old client never offers v2. v2 frames are only
/// ever sent on a v2 session.
pub const ALPN_V2: &[u8] = b"peckrelay/2";
/// Hard cap on one frame's payload.
pub const MAX_FRAME: usize = 8 * 1024;
/// Hard cap on an opaque peer blob (E2E-sealed message or candidate list).
pub const MAX_BLOB: usize = 4 * 1024;
/// Hard cap on one relayed datagram (QUIC packets are ≤ 1452 bytes).
pub const MAX_PACKET: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Role {
    Box = 0,
    Device = 1,
}

impl Role {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Role::Box),
            1 => Some(Role::Device),
            _ => None,
        }
    }
    pub fn other(self) -> Self {
        match self {
            Role::Box => Role::Device,
            Role::Device => Role::Box,
        }
    }
    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMsg {
    Hello {
        role: Role,
        rendezvous_id: [u8; 32],
        public_key: [u8; 32],
    },
    Auth {
        signature: [u8; 64],
    },
    Ping,
    /// E2E-sealed message for the other peer.
    Forward {
        blob: Vec<u8>,
    },
    /// E2E-sealed candidate list (LAN addresses etc.), echoed to the other
    /// peer inside every `PunchNow`.
    SetCandidates {
        blob: Vec<u8>,
    },
    /// Ask for another `PunchNow` round (retry).
    PunchRequest,
    /// Ask for a fresh STUN credential.
    RefreshStun,
    /// v2: one opaque datagram for the other peer, forwarded verbatim. In
    /// practice a QUIC packet — already encrypted end to end.
    Data {
        packet: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerMsg {
    Challenge {
        nonce: [u8; 32],
    },
    Registered {
        stun_username: String,
        stun_password: String,
        ttl_secs: u32,
        stun_port: u16,
    },
    Pong,
    PeerOnline,
    PeerOffline,
    Forwarded {
        blob: Vec<u8>,
    },
    PunchNow {
        /// Peer's STUN-observed public endpoint.
        peer_public: SocketAddr,
        /// Unix ms at which both sides should start sending.
        start_at_ms: u64,
        nonce: [u8; 16],
        attempt: u8,
        /// Peer's E2E-sealed candidate blob (may be empty).
        peer_candidates: Vec<u8>,
    },
    /// v2: a datagram the other peer sent with [`ClientMsg::Data`].
    Data {
        packet: Vec<u8>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large")]
    TooLarge,
    #[error("malformed message")]
    Malformed,
}

// ---- framing ---------------------------------------------------------

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>, ProtoError> {
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME {
        return Err(ProtoError::TooLarge);
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8]) -> std::io::Result<()> {
    debug_assert!(payload.len() <= MAX_FRAME);
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    w.write_all(&out).await?;
    w.flush().await
}

// ---- codec helpers ---------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if self.buf.len() < n {
            return Err(ProtoError::Malformed);
        }
        let (a, b) = self.buf.split_at(n);
        self.buf = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtoError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, ProtoError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], ProtoError> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    fn blob(&mut self) -> Result<Vec<u8>, ProtoError> {
        let n = self.u16()? as usize;
        if n > MAX_BLOB {
            return Err(ProtoError::Malformed);
        }
        Ok(self.take(n)?.to_vec())
    }
    /// The rest of the frame as one datagram (1..=[`MAX_PACKET`] bytes).
    fn packet(&mut self) -> Result<Vec<u8>, ProtoError> {
        let n = self.buf.len();
        if n == 0 || n > MAX_PACKET {
            return Err(ProtoError::Malformed);
        }
        Ok(self.take(n)?.to_vec())
    }
    fn string(&mut self) -> Result<String, ProtoError> {
        String::from_utf8(self.blob()?).map_err(|_| ProtoError::Malformed)
    }
    fn addr(&mut self) -> Result<SocketAddr, ProtoError> {
        let ip = match self.u8()? {
            4 => IpAddr::V4(Ipv4Addr::from(self.arr::<4>()?)),
            6 => IpAddr::V6(Ipv6Addr::from(self.arr::<16>()?)),
            _ => return Err(ProtoError::Malformed),
        };
        Ok(SocketAddr::new(ip, self.u16()?))
    }
    fn finish(self) -> Result<(), ProtoError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(ProtoError::Malformed)
        }
    }
}

fn put_blob(out: &mut Vec<u8>, b: &[u8]) {
    debug_assert!(b.len() <= MAX_BLOB);
    out.extend_from_slice(&(b.len() as u16).to_be_bytes());
    out.extend_from_slice(b);
}

pub fn put_addr(out: &mut Vec<u8>, a: &SocketAddr) {
    match a.ip() {
        IpAddr::V4(v4) => {
            out.push(4);
            out.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            out.push(6);
            out.extend_from_slice(&v6.octets());
        }
    }
    out.extend_from_slice(&a.port().to_be_bytes());
}

/// Encode a list of socket addresses (used inside E2E candidate blobs).
pub fn encode_addrs(addrs: &[SocketAddr]) -> Vec<u8> {
    let mut out = vec![addrs.len().min(32) as u8];
    for a in addrs.iter().take(32) {
        put_addr(&mut out, a);
    }
    out
}

pub fn decode_addrs(buf: &[u8]) -> Option<Vec<SocketAddr>> {
    let mut r = Reader { buf };
    let n = r.u8().ok()? as usize;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(r.addr().ok()?);
    }
    r.finish().ok()?;
    Some(v)
}

// ---- messages --------------------------------------------------------

impl ClientMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            ClientMsg::Hello {
                role,
                rendezvous_id,
                public_key,
            } => {
                o.push(0x01);
                o.push(*role as u8);
                o.extend_from_slice(rendezvous_id);
                o.extend_from_slice(public_key);
            }
            ClientMsg::Auth { signature } => {
                o.push(0x02);
                o.extend_from_slice(signature);
            }
            ClientMsg::Ping => o.push(0x03),
            ClientMsg::Forward { blob } => {
                o.push(0x04);
                put_blob(&mut o, blob);
            }
            ClientMsg::SetCandidates { blob } => {
                o.push(0x05);
                put_blob(&mut o, blob);
            }
            ClientMsg::PunchRequest => o.push(0x06),
            ClientMsg::RefreshStun => o.push(0x07),
            ClientMsg::Data { packet } => {
                o.push(0x10);
                o.extend_from_slice(packet);
            }
        }
        o
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        let mut r = Reader { buf };
        let msg = match r.u8()? {
            0x01 => ClientMsg::Hello {
                role: Role::from_u8(r.u8()?).ok_or(ProtoError::Malformed)?,
                rendezvous_id: r.arr()?,
                public_key: r.arr()?,
            },
            0x02 => ClientMsg::Auth {
                signature: r.arr()?,
            },
            0x03 => ClientMsg::Ping,
            0x04 => ClientMsg::Forward { blob: r.blob()? },
            0x05 => ClientMsg::SetCandidates { blob: r.blob()? },
            0x06 => ClientMsg::PunchRequest,
            0x07 => ClientMsg::RefreshStun,
            0x10 => ClientMsg::Data {
                packet: r.packet()?,
            },
            _ => return Err(ProtoError::Malformed),
        };
        r.finish()?;
        Ok(msg)
    }
}

impl ServerMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            ServerMsg::Challenge { nonce } => {
                o.push(0x81);
                o.extend_from_slice(nonce);
            }
            ServerMsg::Registered {
                stun_username,
                stun_password,
                ttl_secs,
                stun_port,
            } => {
                o.push(0x82);
                put_blob(&mut o, stun_username.as_bytes());
                put_blob(&mut o, stun_password.as_bytes());
                o.extend_from_slice(&ttl_secs.to_be_bytes());
                o.extend_from_slice(&stun_port.to_be_bytes());
            }
            ServerMsg::Pong => o.push(0x83),
            ServerMsg::PeerOnline => o.push(0x84),
            ServerMsg::PeerOffline => o.push(0x85),
            ServerMsg::Forwarded { blob } => {
                o.push(0x86);
                put_blob(&mut o, blob);
            }
            ServerMsg::PunchNow {
                peer_public,
                start_at_ms,
                nonce,
                attempt,
                peer_candidates,
            } => {
                o.push(0x87);
                put_addr(&mut o, peer_public);
                o.extend_from_slice(&start_at_ms.to_be_bytes());
                o.extend_from_slice(nonce);
                o.push(*attempt);
                put_blob(&mut o, peer_candidates);
            }
            ServerMsg::Data { packet } => {
                o.push(0x90);
                o.extend_from_slice(packet);
            }
        }
        o
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        let mut r = Reader { buf };
        let msg = match r.u8()? {
            0x81 => ServerMsg::Challenge { nonce: r.arr()? },
            0x82 => ServerMsg::Registered {
                stun_username: r.string()?,
                stun_password: r.string()?,
                ttl_secs: r.u32()?,
                stun_port: r.u16()?,
            },
            0x83 => ServerMsg::Pong,
            0x84 => ServerMsg::PeerOnline,
            0x85 => ServerMsg::PeerOffline,
            0x86 => ServerMsg::Forwarded { blob: r.blob()? },
            0x87 => ServerMsg::PunchNow {
                peer_public: r.addr()?,
                start_at_ms: r.u64()?,
                nonce: r.arr()?,
                attempt: r.u8()?,
                peer_candidates: r.blob()?,
            },
            0x90 => ServerMsg::Data {
                packet: r.packet()?,
            },
            _ => return Err(ProtoError::Malformed),
        };
        r.finish()?;
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_reject_trailing() {
        let m = ServerMsg::PunchNow {
            peer_public: "[::1]:9".parse().unwrap(),
            start_at_ms: 42,
            nonce: [3; 16],
            attempt: 2,
            peer_candidates: vec![1, 2, 3],
        };
        let mut b = m.encode();
        assert_eq!(ServerMsg::decode(&b).unwrap(), m);
        b.push(0);
        assert!(ServerMsg::decode(&b).is_err());
        let c = ClientMsg::Forward { blob: vec![9; 50] };
        assert_eq!(ClientMsg::decode(&c.encode()).unwrap(), c);
        assert!(ClientMsg::decode(&[0x01, 9]).is_err());
        let addrs: Vec<SocketAddr> = vec!["10.0.0.2:5".parse().unwrap()];
        assert_eq!(decode_addrs(&encode_addrs(&addrs)).unwrap(), addrs);
    }

    #[test]
    fn data_frames_roundtrip_and_cap() {
        let c = ClientMsg::Data {
            packet: vec![0xc3; 1200],
        };
        assert_eq!(ClientMsg::decode(&c.encode()).unwrap(), c);
        let s = ServerMsg::Data {
            packet: vec![0x41; 40],
        };
        assert_eq!(ServerMsg::decode(&s.encode()).unwrap(), s);
        // Empty and oversize datagrams are malformed.
        assert!(ClientMsg::decode(&[0x10]).is_err());
        let mut big = vec![0x10];
        big.resize(MAX_PACKET + 2, 0);
        assert!(ClientMsg::decode(&big).is_err());
    }
}
