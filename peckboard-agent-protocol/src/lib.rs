//! Wire protocol between the `peckboard-agent` daemon and Peckboard core.
//!
//! Pure serde types — no transport lives here. The daemon dials home over
//! an outbound WebSocket and both ends exchange JSON frames wrapped in a
//! versioned [`Envelope`]. Two internally-tagged enums model the two
//! directions: [`AgentFrame`] (daemon → server) and [`ServerFrame`]
//! (server → daemon).
//!
//! The tagging convention (`#[serde(tag = "type")]`, snake_case) mirrors
//! `WsEvent` in `src/ws/broadcaster.rs`, so a frame serializes to e.g.
//! `{"type":"heartbeat"}` or `{"type":"hello", ...}`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current wire protocol version. Bump on any breaking change to the
/// frame shapes. Carried on every [`Envelope`] so a peer can detect skew.
pub const PROTOCOL_VERSION: u32 = 1;

/// Versioned wrapper around a direction frame. The `v` field lets either
/// end detect a version mismatch before interpreting the frame; the frame
/// fields are flattened alongside it, so the tag (`type`) and the version
/// (`v`) sit at the same JSON level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// Protocol version the sender used.
    pub v: u32,
    #[serde(flatten)]
    pub frame: T,
}

impl<T> Envelope<T> {
    /// Wrap a frame, stamping the current [`PROTOCOL_VERSION`].
    pub fn new(frame: T) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            frame,
        }
    }
}

/// Frames the daemon sends to the server.
///
/// Not `deny_unknown_fields`: a newer peer may add fields we ignore, so we
/// stay forward-compatible within a protocol version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentFrame {
    /// Sent once on connect: identifies the daemon and its capabilities.
    Hello {
        agent_version: String,
        platform: String,
        hostname: String,
        capabilities: Vec<String>,
    },
    /// Periodic liveness ping (server may also [`ServerFrame::Ping`]).
    Heartbeat,
    /// Reply to a [`ServerFrame::Request`], keyed by its `corr_id`.
    /// On success `ok` is true and `payload` carries the result; on
    /// failure `ok` is false and `error` carries the reason.
    Result {
        corr_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Unsolicited event (e.g. server-log line, screenshot notice).
    Event { kind: String, data: Value },
    /// Reports a local per-capability allow/deny toggle changing.
    CapabilityState { capability: String, enabled: bool },
}

/// Frames the server sends to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Ask the daemon to run a capability. The daemon replies with an
    /// [`AgentFrame::Result`] carrying the same `corr_id`.
    Request {
        corr_id: String,
        capability: String,
        args: Value,
    },
    /// Cancel an in-flight [`Request`](ServerFrame::Request).
    Cancel { corr_id: String },
    /// Ask the daemon to shut down.
    Shutdown,
    /// Liveness ping; the daemon answers with [`AgentFrame::Heartbeat`].
    Ping,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip_agent(frame: AgentFrame) {
        let env = Envelope::new(frame);
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope<AgentFrame> = serde_json::from_str(&s).unwrap();
        assert_eq!(env, back);
    }

    fn round_trip_server(frame: ServerFrame) {
        let env = Envelope::new(frame);
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope<ServerFrame> = serde_json::from_str(&s).unwrap();
        assert_eq!(env, back);
    }

    #[test]
    fn agent_frames_round_trip() {
        round_trip_agent(AgentFrame::Hello {
            agent_version: "0.1.0".into(),
            platform: "linux".into(),
            hostname: "box".into(),
            capabilities: vec!["echo".into(), "terminal".into()],
        });
        round_trip_agent(AgentFrame::Heartbeat);
        round_trip_agent(AgentFrame::Result {
            corr_id: "c1".into(),
            ok: true,
            payload: Some(json!({"stdout": "hi"})),
            error: None,
        });
        round_trip_agent(AgentFrame::Result {
            corr_id: "c2".into(),
            ok: false,
            payload: None,
            error: Some("boom".into()),
        });
        round_trip_agent(AgentFrame::Event {
            kind: "log".into(),
            data: json!({"line": "started"}),
        });
        round_trip_agent(AgentFrame::CapabilityState {
            capability: "mouse".into(),
            enabled: false,
        });
    }

    #[test]
    fn server_frames_round_trip() {
        round_trip_server(ServerFrame::Request {
            corr_id: "c1".into(),
            capability: "echo".into(),
            args: json!({"msg": "hi"}),
        });
        round_trip_server(ServerFrame::Cancel {
            corr_id: "c1".into(),
        });
        round_trip_server(ServerFrame::Shutdown);
        round_trip_server(ServerFrame::Ping);
    }

    #[test]
    fn hello_wire_shape_is_pinned() {
        let env = Envelope::new(AgentFrame::Hello {
            agent_version: "0.1.0".into(),
            platform: "linux".into(),
            hostname: "box".into(),
            capabilities: vec!["echo".into()],
        });
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(
            v,
            json!({
                "v": 1,
                "type": "hello",
                "agent_version": "0.1.0",
                "platform": "linux",
                "hostname": "box",
                "capabilities": ["echo"],
            })
        );
    }

    #[test]
    fn request_wire_shape_is_pinned() {
        let env = Envelope::new(ServerFrame::Request {
            corr_id: "c1".into(),
            capability: "echo".into(),
            args: json!({"msg": "hi"}),
        });
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(
            v,
            json!({
                "v": 1,
                "type": "request",
                "corr_id": "c1",
                "capability": "echo",
                "args": {"msg": "hi"},
            })
        );
    }

    #[test]
    fn unit_variant_wire_shape() {
        let env = Envelope::new(ServerFrame::Ping);
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v, json!({"v": 1, "type": "ping"}));
    }

    #[test]
    fn unknown_type_is_rejected() {
        let s = r#"{"v":1,"type":"bogus"}"#;
        let r: Result<Envelope<ServerFrame>, _> = serde_json::from_str(s);
        assert!(r.is_err());
    }
}
