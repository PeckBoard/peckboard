---
title: Kaiad
parent: Plugins
nav_order: 16
---

# Kaiad

The Kaiad plugin is an MCP bridge to a [Kaiad](https://github.com/PeckBoard/kaiad-manager) control plane: services and deployments, builds, registry, agents, operators, and incidents, through the panel's hosted MCP server. The tool catalog is discovered live per credential scope, so what agents can call always matches what your credential allows.

It needs an API credential minted in the Kaiad panel with the `mcp.read` scope, plus `mcp.write` if agents should be able to deploy or mutate. Configure `base_url` and `api_key` in the plugin's settings form (the credential is stored masked) or with `kaiad_configure` from a session.

| Tool               | What it does                                                                    |
| ------------------ | ------------------------------------------------------------------------------- |
| `kaiad_configure`  | Stores the panel URL and credential, verifying the connection                   |
| `kaiad_status`     | Checks the connection and reports the discovered tools                          |
| `kaiad_list_tools` | Lists what your credential's scopes allow, with schemas on request              |
| `kaiad_call`       | Invokes any control-plane tool by name, e.g. `list_services` or `trigger_build` |

<details markdown="1">
<summary>Wire behaviour and permissions</summary>

Unlike the Nginx Proxy Manager bridge, Kaiad's MCP server is stateless — no session id, no initialized notification. Both SSE-framed and JSON-framed responses are handled, slow calls (a build trigger, for example) wait host-side past the short per-call plugin timeout, and a remote tool error surfaces as a tool error.

Hooks: `mcp.tool.invoke`. Permissions: `provide_mcp_tools`, `http_request` (outbound HTTP including private and LAN targets).

</details>
