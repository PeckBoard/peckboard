//! Seed catalog copied from `src/provider/mock/mod.rs::mock_model_infos`
//! so the picker is complete even while send is a stub.

pub const PROVIDER_ID: &str = "mock";
pub const DISPLAY_NAME: &str = "Mock";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "doc-review",
            "display_name": "Mock: document review",
            "capabilities": ["mock", "tools", "interactive"],
            "tier": 3
        },
        {
            "id": "echo",
            "display_name": "Mock: echo",
            "capabilities": ["mock"],
            "tier": 1
        },
        {
            "id": "plan-review",
            "display_name": "Mock: plan review (thinking)",
            "capabilities": ["mock", "reasoning"],
            "tier": 3
        },
        {
            "id": "happy-path",
            "display_name": "Mock: happy path",
            "capabilities": ["mock", "tools"],
            "tier": 3
        },
        {
            "id": "run-command",
            "display_name": "Mock: run command",
            "capabilities": ["mock", "tools"],
            "tier": 2
        },
        {
            "id": "subagent",
            "display_name": "Mock: spawn subagent",
            "capabilities": ["mock", "tools"],
            "tier": 2
        },
        {
            "id": "mcp",
            "display_name": "Mock: run mcp blocks from the message",
            "capabilities": ["mock", "tools", "reasoning"],
            "tier": 2
        },
        {
            "id": "tool-use",
            "display_name": "Mock: tool use",
            "capabilities": ["mock", "tools"],
            "tier": 2
        },
        {
            "id": "usage",
            "display_name": "Mock: usage",
            "capabilities": ["mock", "tools"],
            "tier": 2
        },
        {
            "id": "crash",
            "display_name": "Mock: crash",
            "capabilities": ["mock"],
            "tier": 1
        },
        {
            "id": "tool-orphan-crash",
            "display_name": "Mock: tool start without end then crash",
            "capabilities": ["mock", "tools"],
            "tier": 2
        },
        {
            "id": "tool-error",
            "display_name": "Mock: tool error",
            "capabilities": ["mock", "tools"],
            "tier": 2
        },
        {
            "id": "cli-tools",
            "display_name": "Mock: non-Claude CLI tool names",
            "capabilities": ["mock", "tools"],
            "tier": 2
        },
        {
            "id": "system-blob",
            "display_name": "Mock: system notice without text",
            "capabilities": ["mock"],
            "tier": 1
        },
        {
            "id": "ask",
            "display_name": "Mock: ask",
            "capabilities": ["mock", "interactive"],
            "tier": 2
        },
        {
            "id": "markdown",
            "display_name": "Mock: markdown",
            "capabilities": ["mock", "markdown"],
            "tier": 1
        },
        {
            "id": "ctx",
            "display_name": "Mock: context",
            "capabilities": ["mock"],
            "tier": 1
        },
        {
            "id": "block",
            "display_name": "Mock: block",
            "capabilities": ["mock", "interactive"],
            "tier": 2
        }
    ])
}
