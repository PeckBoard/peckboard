//! Minimal RFC 5389 STUN: Binding requests/responses with short-term
//! credentials (USERNAME + MESSAGE-INTEGRITY, HMAC-SHA1 keyed by the
//! password). The relay answers *only* requests whose integrity verifies
//! against a credential it issued over the authenticated TLS channel;
//! everything else is dropped without a reply. No FINGERPRINT, no
//! long-term credentials, no other methods.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use hmac::{Hmac, Mac};
use sha1::Sha1;
use subtle::ConstantTimeEq;

pub const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_FINGERPRINT: u16 = 0x8028;
const HEADER_LEN: usize = 20;
const MI_LEN: usize = 20;
/// RFC 5389 recommends ≤ 548 bytes when path MTU is unknown; requests from
/// our client are ~80 bytes, so anything bigger is junk.
pub const MAX_PACKET: usize = 548;
const MAX_USERNAME: usize = 64;

type HmacSha1 = Hmac<Sha1>;

/// A parsed Binding request whose integrity has *not* been checked yet.
pub struct Request<'a> {
    pub txid: [u8; 12],
    pub username: &'a str,
    packet: &'a [u8],
    mi_offset: usize,
    mi: [u8; MI_LEN],
}

impl Request<'_> {
    /// Constant-time integrity check against the credential's password.
    pub fn verify(&self, password: &[u8]) -> bool {
        let expected = integrity(&self.packet[..self.mi_offset], password);
        expected.ct_eq(&self.mi).into()
    }
}

fn integrity(prefix: &[u8], key: &[u8]) -> [u8; MI_LEN] {
    // The header length must cover everything up to and including the
    // MESSAGE-INTEGRITY attribute itself (RFC 5389 §15.4).
    let mut hdr = [0u8; HEADER_LEN];
    hdr.copy_from_slice(&prefix[..HEADER_LEN]);
    let len = (prefix.len() - HEADER_LEN + 4 + MI_LEN) as u16;
    hdr[2..4].copy_from_slice(&len.to_be_bytes());
    let mut mac = HmacSha1::new_from_slice(key).expect("hmac any key len");
    mac.update(&hdr);
    mac.update(&prefix[HEADER_LEN..]);
    mac.finalize().into_bytes().into()
}

fn header_ok(p: &[u8], msg_type: u16) -> Option<[u8; 12]> {
    if p.len() < HEADER_LEN || p.len() > MAX_PACKET || !p.len().is_multiple_of(4) {
        return None;
    }
    if u16::from_be_bytes([p[0], p[1]]) != msg_type {
        return None;
    }
    if u16::from_be_bytes([p[2], p[3]]) as usize != p.len() - HEADER_LEN {
        return None;
    }
    if u32::from_be_bytes([p[4], p[5], p[6], p[7]]) != MAGIC_COOKIE {
        return None;
    }
    Some(p[8..20].try_into().unwrap())
}

/// Walk attributes, calling `f(type, value, offset_of_attr_header)`.
/// Returns None on any structural error.
fn walk_attrs<'a>(
    p: &'a [u8],
    mut f: impl FnMut(u16, &'a [u8], usize) -> Option<()>,
) -> Option<()> {
    let mut i = HEADER_LEN;
    while i < p.len() {
        if i + 4 > p.len() {
            return None;
        }
        let t = u16::from_be_bytes([p[i], p[i + 1]]);
        let l = u16::from_be_bytes([p[i + 2], p[i + 3]]) as usize;
        let start = i + 4;
        let end = start.checked_add(l)?;
        if end > p.len() {
            return None;
        }
        f(t, &p[start..end], i)?;
        i = start + l.div_ceil(4) * 4;
    }
    Some(())
}

/// Parse a Binding request carrying USERNAME + MESSAGE-INTEGRITY.
pub fn parse_request(p: &[u8]) -> Option<Request<'_>> {
    let txid = header_ok(p, BINDING_REQUEST)?;
    let mut username = None;
    let mut mi: Option<([u8; MI_LEN], usize)> = None;
    walk_attrs(p, |t, v, off| {
        if mi.is_some() && t != ATTR_FINGERPRINT {
            // Nothing but FINGERPRINT may follow MESSAGE-INTEGRITY.
            return None;
        }
        match t {
            ATTR_USERNAME => {
                if v.len() > MAX_USERNAME || username.is_some() {
                    return None;
                }
                username = Some(std::str::from_utf8(v).ok()?);
            }
            ATTR_MESSAGE_INTEGRITY => {
                mi = Some((v.try_into().ok()?, off));
            }
            _ => {}
        }
        Some(())
    })?;
    let (mi, mi_offset) = mi?;
    Some(Request {
        txid,
        username: username?,
        packet: p,
        mi_offset,
        mi,
    })
}

fn put_attr(out: &mut Vec<u8>, t: u16, v: &[u8]) {
    out.extend_from_slice(&t.to_be_bytes());
    out.extend_from_slice(&(v.len() as u16).to_be_bytes());
    out.extend_from_slice(v);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn finish_with_integrity(mut out: Vec<u8>, key: &[u8]) -> Vec<u8> {
    let mi = integrity(&out, key);
    put_attr(&mut out, ATTR_MESSAGE_INTEGRITY, &mi);
    let len = (out.len() - HEADER_LEN) as u16;
    out[2..4].copy_from_slice(&len.to_be_bytes());
    out
}

fn header(msg_type: u16, txid: &[u8; 12]) -> Vec<u8> {
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(txid);
    out
}

/// Client side: build an authenticated Binding request.
pub fn build_request(txid: &[u8; 12], username: &str, password: &[u8]) -> Vec<u8> {
    let mut out = header(BINDING_REQUEST, txid);
    put_attr(&mut out, ATTR_USERNAME, username.as_bytes());
    finish_with_integrity(out, password)
}

/// Relay side: Binding success with XOR-MAPPED-ADDRESS, integrity-protected
/// with the same credential so the client can trust the mapping.
pub fn build_response(txid: &[u8; 12], mapped: SocketAddr, password: &[u8]) -> Vec<u8> {
    let mut out = header(BINDING_SUCCESS, txid);
    put_attr(&mut out, ATTR_XOR_MAPPED_ADDRESS, &xor_addr(mapped, txid));
    finish_with_integrity(out, password)
}

fn xor_addr(a: SocketAddr, txid: &[u8; 12]) -> Vec<u8> {
    let port = a.port() ^ (MAGIC_COOKIE >> 16) as u16;
    let mut v = vec![0u8];
    let mut mask = MAGIC_COOKIE.to_be_bytes().to_vec();
    mask.extend_from_slice(txid);
    match a.ip() {
        IpAddr::V4(ip) => {
            v.push(1);
            v.extend_from_slice(&port.to_be_bytes());
            v.extend(ip.octets().iter().zip(&mask).map(|(b, m)| b ^ m));
        }
        IpAddr::V6(ip) => {
            v.push(2);
            v.extend_from_slice(&port.to_be_bytes());
            v.extend(ip.octets().iter().zip(&mask).map(|(b, m)| b ^ m));
        }
    }
    v
}

/// Client side: verify and decode a Binding success for `txid`.
pub fn parse_response(p: &[u8], txid: &[u8; 12], password: &[u8]) -> Option<SocketAddr> {
    if header_ok(p, BINDING_SUCCESS)? != *txid {
        return None;
    }
    let mut mapped = None;
    let mut mi: Option<([u8; MI_LEN], usize)> = None;
    walk_attrs(p, |t, v, off| {
        if mi.is_some() && t != ATTR_FINGERPRINT {
            return None;
        }
        match t {
            ATTR_XOR_MAPPED_ADDRESS => mapped = Some(decode_xor_addr(v, txid)?),
            ATTR_MESSAGE_INTEGRITY => mi = Some((v.try_into().ok()?, off)),
            _ => {}
        }
        Some(())
    })?;
    let (mi, off) = mi?;
    if !bool::from(integrity(&p[..off], password).ct_eq(&mi)) {
        return None;
    }
    mapped
}

fn decode_xor_addr(v: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let port = u16::from_be_bytes([v[2], v[3]]) ^ (MAGIC_COOKIE >> 16) as u16;
    let mut mask = MAGIC_COOKIE.to_be_bytes().to_vec();
    mask.extend_from_slice(txid);
    let raw: Vec<u8> = v[4..].iter().zip(&mask).map(|(b, m)| b ^ m).collect();
    let ip = match (v[1], raw.len()) {
        (1, 4) => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&raw[..]).ok()?)),
        (2, 16) => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&raw[..]).ok()?)),
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_response_roundtrip() {
        let txid = [5u8; 12];
        let req = build_request(&txid, "user1", b"pw");
        let parsed = parse_request(&req).unwrap();
        assert_eq!(parsed.username, "user1");
        assert!(parsed.verify(b"pw"));
        assert!(!parsed.verify(b"nope"));
        for addr in ["203.0.113.9:4444", "[2001:db8::1]:9"] {
            let a: SocketAddr = addr.parse().unwrap();
            let resp = build_response(&txid, a, b"pw");
            assert_eq!(parse_response(&resp, &txid, b"pw"), Some(a));
            assert_eq!(parse_response(&resp, &txid, b"bad"), None);
        }
    }

    #[test]
    fn rejects_unauthenticated_and_junk() {
        // Plain RFC 5389 binding request with no attributes.
        let mut plain = header(BINDING_REQUEST, &[1; 12]);
        assert!(parse_request(&plain).is_none());
        plain.extend_from_slice(&[0, 6, 0, 200]); // attr overruns packet
        plain[3] = 4;
        assert!(parse_request(&plain).is_none());
        assert!(parse_request(b"GET / HTTP/1.1\r\n\r\n").is_none());
    }
}
