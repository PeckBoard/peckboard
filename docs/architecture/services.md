# Background Services

## Push Notifications

- VAPID key pair generated on first boot, persisted in DocStore
- Push subscriptions stored per-endpoint with p256dh + auth keys
- Notifications sent on: session completion, worker step completion, card terminal state
- 410/404 endpoint responses auto-prune the subscription
- Transient errors (5xx, network) are logged but don't prune

## TLS Certificate Manager

- Default: self-signed ECDSA P-256 cert, SHA-256 signed, 365-day validity
- SANs: `localhost`, both loopback addresses, every non-loopback interface IP the host has, and the system hostname — snapshotted at generation time, so a cert issued before a new network attaches won't cover it until regenerated
- Renewal check runs on `ensure_certs`, called once at server startup (no background timer): regenerates when the cert is within its 30-day expiry window, when the on-disk pair fails to validate, or when the current SAN set no longer matches the sidecar the last generation recorded
- Operator-uploaded: `POST /api/settings/tls/cert` validates and installs a PEM pair under `<data_dir>/certs/uploaded-{cert,key}.pem`; once present it always wins over the self-signed pair and hot-swaps into the live listener with no restart
- `DELETE /api/settings/tls/cert` drops the uploaded pair and reverts to self-signed (generating one if none exists)
- `POST /api/settings/tls/regenerate` mints a fresh self-signed cert over the host's current addresses; it refreshes the self-signed fallback only — uploaded material still wins if one is installed
- Private key mode 0o600 (uploaded and self-signed alike)
- Hot-swap: a `ResolvesServerCert` resolver is built once at boot; regenerate/upload/revert swap the key it hands back, so a running listener picks up new material on the next handshake with no rebind
- Startup TLS failure (cert generation or listener bind fails) leaves plain HTTP serving; HTTPS is disabled and an announcement banner (`tls-startup-failure`) reports the failure to the next user who logs in
- A self-signed certificate still trips the browser's untrusted-certificate warning on first HTTPS visit — only an uploaded, CA-issued certificate removes that warning

## mDNS Advertiser

- Publishes `<config.mdnsName>.local` at the HTTPS port over mDNS
- Name generated on first boot: adjective-animal-color + digit (e.g. `musing-cats-amber7`)
- Validated against DNS-label regex
- Republish on wake (mDNS socket may have missed network-recovery transition)
- Unpublish + destroy on graceful shutdown
- Failures are logged, never block boot

## Wake-from-Sleep Detector

- Polls `Date.now()` every 10s
- When a tick's gap exceeds 3x the interval (30s), host is considered to have slept
- Emits `wake` event on an EventEmitter
- Platform-agnostic (works on macOS, Windows, Linux)

On wake:

1. ClaudeManager: rebase `lastUsed` for all live processes, skip next idle sweep, restart active inactivity timers
2. WorkerManager: open 30s grace window (suppress retry/crash counter increments)
3. mDNS: republish service advertisement

## Keep-Awake (Host Sleep Blocker)

- macOS: spawns `caffeinate -i -w <pid>`
- Windows: PowerShell `SetThreadExecutionState` loop
- Linux: not supported (toggle disabled in UI)
- Watchdog respawns child if it dies
- Hard-coded argv, never user input
- User-toggleable in Options + status bar

## Session Auto-Titler

- After first turn on an unnamed plain session, spawns a throwaway `claude -p` subprocess (Haiku, no --resume, no MCP)
- Generates a 40-char title from the first user message
- 5s hard timeout; falls back to first 40 chars of user message
- Wake handler kills in-flight gens past threshold
- Boot replays pending titles for sessions whose gen was interrupted by crash

## Status Line

- Delegates to a configured external command (shell-quote parsed, shell: false)
- Pipes hook payload (session/cost/model/cwd) on stdin
- Stdout surfaces in the UI
- Rejects shell operators / pipes / redirects / command substitution
- 15s cache TTL to avoid repeated subprocess spawns

## Usage Tracker

- Walks `~/.claude/projects/**/*.jsonl` and sums token usage × list prices
- Computes month/last-hour/24h windows
- Malformed JSONL lines tolerated (skipped)
- 5m/1h cache buckets

## Model Registry

- Seeds `opus`/`sonnet`/`haiku`/`default` aliases
- Discovers extra model IDs (including Bedrock ARNs) from Claude CLI transcripts
- Sanitizes caller-supplied model strings (regex: aliases, Claude model ids, Bedrock ARNs)
- Bedrock detection via env vars (ANTHROPIC*DEFAULT*\*\_MODEL)
