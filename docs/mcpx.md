---
title: mcpx (MCP Client CLI)
parent: Plugins
nav_order: 4
---

# mcpx (MCP Client CLI)

[mcpx](https://github.com/PeckBoard/mcpx) is an MCP **client** for the shell, plus a local gateway. It reads the same `mcp.json` you use with Claude, Cursor, or VS Code. Scripts can then list and call any server's tools, and a bot can reach all of them through one authenticated endpoint on `127.0.0.1`.

Install it from **App Manager** (entry `mcpx`) on a Linux or macOS target. App Manager downloads the sha256-checked release binary to `~/.local/bin/mcpx`; on macOS, add `~/.local/bin` to your `PATH` to run it by name. Binaries for Linux (x86_64, aarch64), macOS (Apple silicon, Intel), and Windows (x86_64) are on the [releases](https://github.com/PeckBoard/mcpx/releases) page — on Windows, download `mcpx-windows-x86_64.exe` by hand (see the README's PowerShell snippet); App Manager doesn't drive Windows targets.

```bash
mcpx servers                                   # status, serverInfo, capabilities
mcpx tools github --pretty
mcpx call fs.read_file --arg path=/tmp/notes/todo.md
mcpx login linear                              # OAuth in the browser; --no-browser prints the URL
mcpx serve --allow 'github__*' --deny '*__delete*'
```

- **Servers:** stdio, streamable HTTP, and legacy HTTP+SSE. `${VAR}` is expanded in commands, args, env, URLs, and headers.
- **OAuth:** follows the MCP Authorization spec — discovery, dynamic client registration, PKCE, refresh, and `client_credentials`. Tokens are stored in `~/.config/mcpx/tokens.json` (mode 0600; `%APPDATA%\mcpx` on Windows).
- **Output:** JSON on stdout and errors as JSON on stderr. `call` exits 1 when the tool result is an error.

## Calling PeckBoard's Own Tools

PeckBoard's own MCP server (`/mcp`) can be one of mcpx's servers, so scripts can list projects, create cards, and so on. Create a client under Settings → Connections → External Access, scoped to a folder (and optionally a project), then add it to `mcp.json` with the secret kept in the environment:

```json
{
  "mcpServers": {
    "peckboard": {
      "url": "http://127.0.0.1:3344/mcp",
      "oauth": {
        "grant": "client_credentials",
        "client_id": "<client id>",
        "client_secret": "${PECKBOARD_MCP_SECRET}"
      }
    }
  }
}
```

mcpx exchanges the secret for one-hour tokens itself. The loopback URL works when mcpx runs on the PeckBoard host; from elsewhere use the HTTPS MCP URL that External Access shows. This needs mcpx 0.2.2 or later.

Servers added under Settings → Connections → MCP Servers are a separate list: PeckBoard injects them into agent sessions, and mcpx doesn't read them.

## Gateway

`mcpx serve` listens on `127.0.0.1:7790` and writes a bearer token to `~/.config/mcpx/gateway.json`. Servers stay connected between calls, and a running gateway also speeds up plain CLI calls.

- **MCP:** `http://127.0.0.1:7790/mcp` exposes every upstream tool as `<server>__<tool>`. Point any MCP-capable bot at it with the bearer token.
- **REST:** for bots that don't speak MCP — `GET /tools`, `POST /call {server, tool, arguments}`, `GET /servers`, `GET /resources`, `POST /read`, `GET /prompts`, `POST /prompt`.

The gateway runs every tool with your credentials. Use `--allow` and `--deny` to expose only what a bot needs. See the [README](https://github.com/PeckBoard/mcpx#readme) for the full configuration and REST reference.
