# Background Services

## Push Notifications

- VAPID key pair generated on first boot, persisted in DocStore
- Push subscriptions stored per-endpoint with p256dh + auth keys
- Notifications sent on: session completion, worker step completion, card terminal state
- 410/404 endpoint responses auto-prune the subscription
- Transient errors (5xx, network) are logged but don't prune

## TLS Certificate Manager

- Default: self-signed ECDSA P-256 cert, SHA-256 signed, 365-day validity
- SANs: commonName, localhost, 127.0.0.1, ::1
- Auto-renewal: 24h check interval, regenerates within 30-day expiry window
- Legacy RSA certs auto-upgraded to EC on restart
- User-provided: set `tls.certPath` + `tls.keyPath` in config — used verbatim, no renewal
- Private key mode 0o600
- Hot-swap: onRotate callback allows live cert replacement without restart

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
- Bedrock detection via env vars (ANTHROPIC_DEFAULT_*_MODEL)
