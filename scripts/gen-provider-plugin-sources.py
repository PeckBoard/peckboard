#!/usr/bin/env python3
"""Copy deleted native parsers into first-party provider plugins and rewrite imports."""
from __future__ import annotations

import re
from pathlib import Path

ROOT = Path("/home/infra/peckboard")
OLD = ROOT / ".tmp-old-providers"
PLUGINS = ROOT / "peck-plugins"

EVENT_RS = r'''//! Wire-compatible `ProviderEvent` (and friends) matching
//! `src/provider/stream.rs` serde (`tag = "kind", rename_all = "snake_case"`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolImage {
    pub mime_type: String,
    pub data_base64: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashKind {
    AuthExpired,
    RateLimit,
    Timeout,
    Interrupted,
    SpawnFailed,
    ResumeFailed,
    ExitedMidTurn,
    NoOutput,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    pub fn from_provider(raw: &str) -> Self {
        match raw {
            "in_progress" => TodoStatus::InProgress,
            "completed" | "done" => TodoStatus::Done,
            _ => TodoStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    #[serde(default, rename = "activeForm", skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderEvent {
    Started {
        model: String,
        conversation_id: Option<String>,
        #[serde(default)]
        metadata: serde_json::Value,
    },
    Text { text: String },
    Thinking { text: String },
    ToolStart {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolEnd {
        tool_use_id: String,
        output: Option<String>,
        error: Option<String>,
        #[serde(default)]
        images: Vec<ToolImage>,
    },
    FileDiff {
        path: String,
        diff: String,
        added: i64,
        removed: i64,
        #[serde(default)]
        created: bool,
    },
    Todo { todos: Vec<TodoItem> },
    Usage {
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_creation_tokens: i64,
        total_tokens: i64,
        context_tokens: i64,
        model: Option<String>,
        #[serde(default)]
        turn_seq: Option<i32>,
    },
    System {
        text: String,
        subtype: String,
        #[serde(default)]
        detail: serde_json::Value,
    },
    Completed {
        conversation_id: Option<String>,
        #[serde(default)]
        result_meta: serde_json::Value,
    },
    Crashed {
        reason: String,
        #[serde(default)]
        error_kind: CrashKind,
        exit_code: Option<i32>,
        stderr: Option<String>,
    },
    ControlRequest {
        request_id: String,
        request_type: String,
        payload: serde_json::Value,
    },
}
'''

HOST_CLI_RS = r'''//! FFI layer: Peckboard core host functions this plugin calls.

pub enum HostFn {
    RegisterProvider,
    EmitProviderEvent,
    ProviderShouldStop,
    ProviderTakeMessage,
    ProviderGetSession,
    ProviderGetMcpConfig,
    ProviderAccountEnv,
    ProviderWriteFile,
    ProviderSpawn,
    ProviderReadLine,
    ProviderWriteStdin,
    ProviderReadStdin,
    ProviderKill,
    GetPluginSetting,
    HttpRequest,
    StorePut,
    StoreGet,
    StoreList,
    StoreDelete,
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::HostFn;
    use extism_pdk::*;

    #[host_fn]
    extern "ExtismHost" {
        fn peckboard_register_provider(input: String) -> String;
        fn peckboard_emit_provider_event(input: String) -> String;
        fn peckboard_provider_should_stop(input: String) -> String;
        fn peckboard_provider_take_message(input: String) -> String;
        fn peckboard_provider_get_session(input: String) -> String;
        fn peckboard_provider_get_mcp_config(input: String) -> String;
        fn peckboard_provider_account_env(input: String) -> String;
        fn peckboard_provider_write_file(input: String) -> String;
        fn peckboard_provider_spawn(input: String) -> String;
        fn peckboard_provider_read_line(input: String) -> String;
        fn peckboard_provider_write_stdin(input: String) -> String;
        fn peckboard_provider_read_stdin(input: String) -> String;
        fn peckboard_provider_kill(input: String) -> String;
        fn peckboard_get_plugin_setting(input: String) -> String;
        fn peckboard_http_request(input: String) -> String;
        fn peckboard_store_put(input: String) -> String;
        fn peckboard_store_get(input: String) -> String;
        fn peckboard_store_list(input: String) -> String;
        fn peckboard_store_delete(input: String) -> String;
    }

    pub fn call_host(
        which: HostFn,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let s = input.to_string();
        let out = unsafe {
            match which {
                HostFn::RegisterProvider => peckboard_register_provider(s),
                HostFn::EmitProviderEvent => peckboard_emit_provider_event(s),
                HostFn::ProviderShouldStop => peckboard_provider_should_stop(s),
                HostFn::ProviderTakeMessage => peckboard_provider_take_message(s),
                HostFn::ProviderGetSession => peckboard_provider_get_session(s),
                HostFn::ProviderGetMcpConfig => peckboard_provider_get_mcp_config(s),
                HostFn::ProviderAccountEnv => peckboard_provider_account_env(s),
                HostFn::ProviderWriteFile => peckboard_provider_write_file(s),
                HostFn::ProviderSpawn => peckboard_provider_spawn(s),
                HostFn::ProviderReadLine => peckboard_provider_read_line(s),
                HostFn::ProviderWriteStdin => peckboard_provider_write_stdin(s),
                HostFn::ProviderReadStdin => peckboard_provider_read_stdin(s),
                HostFn::ProviderKill => peckboard_provider_kill(s),
                HostFn::GetPluginSetting => peckboard_get_plugin_setting(s),
                HostFn::HttpRequest => peckboard_http_request(s),
                HostFn::StorePut => peckboard_store_put(s),
                HostFn::StoreGet => peckboard_store_get(s),
                HostFn::StoreList => peckboard_store_list(s),
                HostFn::StoreDelete => peckboard_store_delete(s),
            }
        }
        .map_err(|e| e.to_string())?;
        let v: serde_json::Value =
            serde_json::from_str(&out).map_err(|e| format!("host returned invalid json: {e}"))?;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            return Err(err.to_string());
        }
        Ok(v)
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::HostFn;

    pub fn call_host(
        _which: HostFn,
        _input: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        unimplemented!("host calls are only available on wasm32")
    }
}

pub use imp::call_host;
'''

HOST_OLLAMA_RS = HOST_CLI_RS  # ollama also needs spawn? no, but account/write/http all useful


def rewrite_rust(text: str) -> str:
    repls = [
        (
            "use crate::provider::stream::{ProviderEvent, ToolImage};",
            "use crate::event::{ProviderEvent, ToolImage};",
        ),
        (
            "use crate::provider::stream::ProviderEvent;",
            "use crate::event::ProviderEvent;",
        ),
        (
            "use crate::todo::{TodoItem, TodoStatus};",
            "use crate::event::{TodoItem, TodoStatus};",
        ),
        (
            "use crate::provider::stream::{CrashKind, ModelInfo, ProviderEvent};",
            "use crate::event::{CrashKind, ProviderEvent};",
        ),
    ]
    for a, b in repls:
        text = text.replace(a, b)
    text = re.sub(r"tracing::debug!\([^;]*\);", "", text)
    text = re.sub(r"tracing::(?:info|warn|error|debug)!\([\s\S]*?\);", "", text)
    return text


def copy_rewritten(src: Path, dst: Path) -> None:
    text = rewrite_rust(src.read_text())
    dst.parent.mkdir(parents=True, exist_ok=True)
    dst.write_text(text)


def main() -> None:
    cli = ["claude", "grok", "cursor", "kimi", "codex"]
    for pid in cli:
        d = PLUGINS / pid / "src"
        (d / "event.rs").write_text(EVENT_RS)
        (d / "host.rs").write_text(HOST_CLI_RS)

    (PLUGINS / "ollama" / "src" / "host.rs").write_text(HOST_CLI_RS)
    (PLUGINS / "ollama" / "src" / "event.rs").write_text(EVENT_RS)

    copy_rewritten(OLD / "claude/process/parser.rs", PLUGINS / "claude/src/parser.rs")
    copy_rewritten(OLD / "claude/process/sandbox.rs", PLUGINS / "claude/src/sandbox.rs")
    copy_rewritten(OLD / "claude/process/usage.rs", PLUGINS / "claude/src/usage.rs")
    copy_rewritten(OLD / "grok/parser.rs", PLUGINS / "grok/src/parser.rs")
    copy_rewritten(OLD / "cursor/parser.rs", PLUGINS / "cursor/src/parser.rs")
    copy_rewritten(OLD / "kimi/parser.rs", PLUGINS / "kimi/src/parser.rs")
    copy_rewritten(OLD / "codex/parser.rs", PLUGINS / "codex/src/parser.rs")
    print("copied parsers + event/host")


if __name__ == "__main__":
    main()
