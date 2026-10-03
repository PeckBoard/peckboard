//! Peckboard relay: a handshake-only rendezvous server.
//!
//! It introduces a user's device to their Peckboard box and then gets out
//! of the way — all traffic flows directly between the two over UDP hole
//! punching. The relay never carries user traffic; the only peer↔peer
//! bytes it forwards are E2E-sealed signaling blobs it cannot read.

#[cfg(feature = "client")]
pub mod client;
pub mod keys;
pub mod limits;
pub mod proto;
pub mod server;
pub mod stun;
pub mod tls;
