# Peckboard Implementation Epic

Full implementation plan covering backend (Rust/Axum), frontend (React/Zustand/Vite), and testing.

## Phase 1: Foundation

Everything else depends on this. No features until the skeleton runs end-to-end.

### 1.1 Project Scaffolding
- [ ] Set up React + Vite + TypeScript in `web/`
- [ ] Configure Vite proxy to Axum backend (dev mode)
- [ ] Configure `rust-embed` to serve `web/dist/` (production mode)
- [ ] Set up Zustand store with initial slices (auth, ui)
- [ ] Set up shared types between frontend and backend (API contracts)
- [ ] Verify dev workflow: `cargo run` + `cd web && npm run dev` both work together

### 1.2 Database Layer
- [ ] Generate Diesel schema from existing migration (`diesel print-schema`)
- [ ] Create Diesel models for all tables (insertable + queryable structs)
- [ ] Implement CRUD operations for each table behind the `Db` wrapper
- [ ] Write unit tests for all CRUD operations (in-memory SQLite)

### 1.3 Plugin Infrastructure
- [ ] Implement plugin manager: scan `<dataDir>/plugins/`, load `.wasm` files
- [ ] Implement hook dispatcher: manifest parsing, ordered dispatch, verdict handling (allow/cancel/modify/skip)
- [ ] Implement sandbox enforcement: 128 MB memory limit, 2s timeout, no FS, no network
- [ ] Implement host functions: `peckboard_log`, `peckboard_get_config`
- [ ] Write unit tests: plugin load/unload, hook dispatch, timeout kill, memory kill, verdict chaining
- [ ] Write a test plugin (`.wasm`) that exercises the hook contract

### 1.4 Event Log
- [ ] Implement event append (with seq assignment) and query functions (list, list_since, tail, latest_seq)
- [ ] Implement event hooks (event.append.before/after)
- [ ] Write unit tests: append ordering, seq monotonicity, tail queries, hook dispatch on append

**Gate:** Backend compiles, migrations run, CRUD works, plugin system loads and dispatches, event log appends and queries. Frontend skeleton renders.

---

## Phase 2: Auth and Users

### 2.1 Backend Auth
- [ ] Implement Argon2 password hashing and verification
- [ ] Implement user CRUD (create, read, update, delete) with role enforcement
- [ ] Implement JWT token generation and validation
- [ ] Implement `auth_sessions` table operations (create, list by user, revoke, revoke all, purge expired)
- [ ] Implement auth middleware (extract JWT from Authorization header, validate, inject user context)
- [ ] Implement rate limiting (per-IP login attempts, linear delay ramp)
- [ ] Implement registration endpoint (POST /api/auth/register — disabled once an admin exists)
- [ ] Implement login endpoint (POST /api/auth/login)
- [ ] Implement logout endpoint (POST /api/auth/logout)
- [ ] Implement password change endpoint (POST /api/auth/change-password)
- [ ] Implement auth status endpoint (GET /api/auth/status — returns whether any users exist)
- [ ] Implement auth session list/revoke endpoints
- [ ] Wire all auth hooks (login, register, password change, session create/revoke — before/after/failed)
- [ ] Write unit tests: hashing, token lifecycle, middleware rejection, rate limiting
- [ ] Write integration tests: full login flow, registration, token expiry, session revocation

### 2.2 Frontend Auth
- [ ] Build LoginModal component
- [ ] Build RegisterModal component (shown on first visit when no users exist)
- [ ] Implement auth store slice (token storage in localStorage/sessionStorage, authed flag, login/logout actions)
- [ ] Implement `authedFetch` wrapper (attach JWT, handle 401 → show login modal)
- [ ] Implement auth status check on app load (GET /api/auth/status → registration or login)
- [ ] Write unit tests: token storage, 401 handling, auth state transitions

### 2.3 User Management
- [ ] Build user management page (admin only)
- [ ] User list with role badges
- [ ] Create user form (username, email, password, role)
- [ ] Edit user (change role, reset password)
- [ ] Delete user (with confirmation)
- [ ] Auth session viewer: list active sessions per user, revoke individual/all
- [ ] Wire user hooks (create/update/delete — before/after/failed)
- [ ] Write E2E test: register first user → login → create second user → manage sessions

**Gate:** Users can register (first boot), log in, manage sessions. Admin can manage other users. All auth hooks fire.

---

## Phase 3: Folders and Sessions

### 3.1 Folder Management
- [ ] Implement folder CRUD endpoints (POST/GET/DELETE /api/folders)
- [ ] Implement folder deletion flow: check for sessions → prompt user (delete sessions / move sessions / cancel)
- [ ] Wire folder hooks (create/delete/missing — before/after/failed)
- [ ] Build folder management page (list, create, delete with confirmation dialog)
- [ ] Write unit tests: CRUD, deletion with dependent sessions
- [ ] Write integration tests: create folder → create session in folder → delete folder flow

### 3.2 Sessions Backend
- [ ] Implement session CRUD endpoints (POST/GET/PATCH/DELETE /api/sessions)
- [ ] Implement session list (filter out worker sessions)
- [ ] Implement session clear endpoint
- [ ] Implement session read endpoint (append session-read event)
- [ ] Implement event replay endpoint (GET /api/sessions/:id/events with afterSeq + limit)
- [ ] Wire session hooks (create/update/delete/clear/message/read — before/after/failed)
- [ ] Write unit tests: CRUD, event replay pagination
- [ ] Write integration tests: create session → send message → replay events

### 3.3 Sessions Frontend
- [ ] Build session list (sidebar/drawer)
- [ ] Build new session modal (name + folder dropdown)
- [ ] Build session toolbar (model chip, rename, clear, delete)
- [ ] Build chat view container (wires event log to renderer)
- [ ] Implement session store slice (sessions list, active session, input drafts)
- [ ] Write unit tests: store slice state transitions

### 3.4 WebSocket
- [ ] Implement WS upgrade handler in Axum
- [ ] Implement auth handshake (first frame = JWT, 10s timeout, code 4001 on failure)
- [ ] Implement subscribe/unsubscribe/resume frames
- [ ] Implement event fan-out (broadcast new events to subscribed clients)
- [ ] Implement resume-from-seq replay (capped, with resume-too-far fallback)
- [ ] Implement mutating frame re-validation (re-check JWT before send/cancel/interrupt)
- [ ] Implement auth sweep (10s interval, close expired/revoked tokens)
- [ ] Wire WS hooks (connect/disconnect/message — before/after)
- [ ] Build WS client in frontend (connect, auth, resume, backoff reconnect with jitter)
- [ ] Implement WS store slice (connected flag, send/cancel/interrupt actions, resyncAll)
- [ ] Write unit tests: handshake, frame routing, sweep
- [ ] Write integration tests: connect → auth → subscribe → receive events → reconnect resume
- [ ] Write E2E test: two browser tabs see the same session events

**Gate:** Users can create folders, create sessions in a folder, and see a live chat view. WebSocket streams events in real time. Reconnect resumes cleanly.

---

## Phase 4: Claude CLI Integration

### 4.1 CLI Process Management
- [ ] Implement Claude CLI spawning (build argv: `claude -p`, `--output-format stream-json`, `--verbose`, `--model`, `--effort`, `--resume`, `--mcp-config`, `--permission-prompt-tool`)
- [ ] Implement stream-json parser: read stdout line by line, classify into event kinds (agent-start, agent-text, agent-tool-start, agent-tool-end, agent-end, control_request)
- [ ] Implement event appending: each parsed chunk → event appended to session log → broadcast via WS
- [ ] Implement process kill (SIGTERM, with timeout escalation to SIGKILL)
- [ ] Implement soft interrupt (write control_request interrupt on stdin)
- [ ] Implement `--resume` with conversation ID (read from event log tail)
- [ ] Implement mock/fake CLI provider for testing (echoes input, simulates streaming, tool use, crashes)
- [ ] Write unit tests: argv construction, stream-json parsing, event classification
- [ ] Write integration tests: spawn mock CLI → stream events → verify log

### 4.2 Session Messaging
- [ ] Implement send message flow: append user event → spawn or resume CLI → stream response events
- [ ] Implement cancel (kill process)
- [ ] Implement interrupt (soft abort)
- [ ] Implement idle process sweeper (kill processes idle beyond threshold, preserve session for resume)
- [ ] Wire idle sweeper hooks (sweep/kill — before/after)
- [ ] Write integration tests: send → receive → send again (resume) → cancel → resume

### 4.3 AskUserQuestion
- [ ] Implement control_request handling (parse AskUserQuestion from CLI stdout)
- [ ] Append question event to log
- [ ] Implement answer/reject via WS frames → write control_response on CLI stdin → append question-resolved event
- [ ] Implement pending question derivation from event log tail
- [ ] Build AskUserQuestion card component (radios/checkboxes, submit/dismiss)
- [ ] Write integration tests: CLI asks question → user answers → CLI receives answer

### 4.4 Chat UI
- [ ] Build event log renderer: walk events → display items (user bubbles, streaming assistant text, tool-use blocks, step headers, system notices, question cards)
- [ ] Implement live streaming display (new agent-text events append to current message in real time)
- [ ] Implement tool-use display (collapsible blocks with name, input, output)
- [ ] Implement agent status indicator (idle/working/tool-active/crashed — derived from log tail)
- [ ] Build input bar (auto-resize textarea, send button, mobile keyboard handling)
- [ ] Implement scroll anchor (stick-to-bottom, respect user scroll-up)
- [ ] Write E2E test: send message → see streaming response → see tool use → see final message

**Gate:** Users can chat with Claude (or mock CLI). Streaming responses, tool use, interrupts, questions all work. Idle processes are swept.

---

## Phase 5: Projects and Cards

### 5.1 Projects Backend
- [ ] Implement project CRUD endpoints
- [ ] Implement pause/resume endpoints
- [ ] Wire project hooks (create/update/delete/pause/resume — before/after/failed)
- [ ] Write unit tests: CRUD, status transitions
- [ ] Write integration tests: create → pause → resume → delete

### 5.2 Cards Backend
- [ ] Implement card CRUD endpoints (nested under /api/projects/:id/cards)
- [ ] Implement card edit policy enforcement (terminal read-only, backlog-only fields)
- [ ] Implement step advancement
- [ ] Implement block/unblock
- [ ] Wire card hooks (create/update/delete/step/done/wont_do/blocked/unblocked — before/after/failed)
- [ ] Write unit tests: CRUD, edit policy, step advancement
- [ ] Write integration tests: create card → advance step → block → unblock → done

### 5.3 Projects and Cards Frontend
- [ ] Build kanban board component (columns per pipeline step, drag-to-reorder within column)
- [ ] Build card component (title, priority, status indicators, blocked badge, worker status)
- [ ] Build card 3-dot menu (Session, Edit, Delete, Stop Worker, Restart Worker, Cancel as Won't Do)
- [ ] Build new project modal (name, folder dropdown, context, worker count, workflow, model, effort)
- [ ] Build add card modal (title, description, workflow, priority, model, effort)
- [ ] Build edit card modal (subject to edit policy — disabled fields when frozen)
- [ ] Implement project store slice (projects list, active project, cards, CRUD actions)
- [ ] Write E2E test: create project → add cards → drag between columns → view card session

**Gate:** Full kanban board working. Cards can be created, edited, moved through pipeline steps, blocked/unblocked, done/wont-do.

---

## Phase 6: Worker Orchestration

### 6.1 MCP Server
- [ ] Implement MCP stdio server (JSON-RPC over stdin/stdout)
- [ ] Implement MCP tools: complete_step, finish_card, wont_do_card, ask_user, create_card, list_cards, list_projects, list_workflows, write_report, attach_report_file, update_card, update_project, create_project, pause_project, resume_project, delete_card, move_card_to_done, move_card_to_wont_do
- [ ] Implement per-session MCP config file generation
- [ ] Implement MCP bearer token registry (issue, lookup, revoke by session)
- [ ] Wire MCP hooks (config write/delete, token issue/revoke, tool call, server start/stop — before/after/failed)
- [ ] Write unit tests: JSON-RPC parsing, tool dispatch, token scoping
- [ ] Write integration tests: spawn MCP server → call tools → verify side effects

### 6.2 Worker Lifecycle
- [ ] Implement worker prompt construction (project context + card title/description + workflow step instructions + handoff context)
- [ ] Implement spawn flow: pick unassigned cards → resolve model/effort → create session → issue MCP token → write MCP config → send initial prompt
- [ ] Implement checkAndSpawnWorkers with per-project spawn lock (coalesce, at most one queued)
- [ ] Implement handleWorkerDone: derive intent from log → act (advance step, finish, wont-do, ask-user, continue-retry)
- [ ] Implement handleWorkerError: schedule recovery prompt (5s delay) on same session
- [ ] Implement step advancement: kill CLI → respawn with --resume and new step instructions
- [ ] Implement continue-retry backoff (2s/4s/8s/16s/32s, max 5, then wont-do)
- [ ] Implement worker intent derivation (walk log tail for *-requested events)
- [ ] Wire worker hooks (spawn/prompt/done/error/recovery/stop/restart — before/after/failed)
- [ ] Write unit tests: prompt construction, intent derivation, backoff calculation
- [ ] Write integration tests (with mock CLI): spawn → run → complete_step → next step → finish

### 6.3 Money-Loop Defense
- [ ] Implement detectRetryLoop (walk log tail, count consecutive crashes, threshold at 3)
- [ ] Implement blocking: denied verdict → block card with reason, no agent-start appended
- [ ] Implement spawn-time exception blocking (first failure blocks)
- [ ] Write unit tests: crash counting, step-change halts walk, complete halts walk, threshold blocking
- [ ] Write integration tests: simulate 4 consecutive crashes → verify card blocked

### 6.4 Wake-from-Sleep
- [ ] Implement wake detector (poll Date.now every 10s, detect 3x gap)
- [ ] Implement grace window (30s post-wake: suppress retry/crash counter increments, force allow on detectRetryLoop)
- [ ] Implement idle sweeper skip on first post-wake sweep
- [ ] Implement mDNS republish on wake
- [ ] Wire wake hooks (detected, grace expired)
- [ ] Write unit tests: drift detection, grace window behavior

### 6.5 Worker Watchdog
- [ ] Implement sweepOrphanWorkers (60s cadence): orphan sessions, stale refs, dead-but-claimed workers, unclaimed pipeline cards
- [ ] Wire watchdog hooks (sweep/orphan/stale_ref/dead_worker/unclaimed — before/after)
- [ ] Write unit tests: all four watchdog cases
- [ ] Write integration tests: simulate orphan → verify teardown

**Gate:** Full autonomous worker pipeline working. Cards flow through steps, workers spawn and recover, money-loop defense blocks runaway spend, watchdog cleans up orphans.

---

## Phase 7: Reports

### 7.1 Reports Backend
- [ ] Implement report storage (write markdown to `<dataDir>/reports/<date>/<file>.md` with frontmatter)
- [ ] Implement report list, read, update endpoints
- [ ] Implement report download (raw .md) and folder zip endpoints
- [ ] Implement attachment storage (allowlisted extensions, 10 MB cap, nosniff headers)
- [ ] Implement discuss endpoint (create session with report as attachment)
- [ ] Implement name sanitization (no path separators, traversal guards)
- [ ] Wire report hooks (write/update/delete/attach — before/after/failed)
- [ ] Write unit tests: sanitization, frontmatter round-trip, traversal guards
- [ ] Write integration tests: write report → read → update → download → zip

### 7.2 Reports Frontend
- [ ] Build report browser (folder accordion, file list)
- [ ] Build report viewer (rendered markdown with DOMPurify)
- [ ] Build report editor (raw textarea with autosave)
- [ ] Implement report download and zip buttons
- [ ] Implement discuss button (opens new session with report attached)
- [ ] Implement reports store slice
- [ ] Write E2E test: worker writes report → user views → edits → downloads

**Gate:** Reports can be written by workers/sessions, viewed, edited, downloaded, and discussed.

---

## Phase 8: Session Attachments

### 8.1 Backend
- [ ] Implement attachment upload endpoint (base64 JSON → UUID-keyed file on disk)
- [ ] Implement attachment list/download/delete endpoints
- [ ] Implement attachment cascade delete on session delete/clear
- [ ] Implement extension allowlist and size cap enforcement
- [ ] Implement attachment path passing to CLI on send (append absolute paths to message)
- [ ] Wire attachment hooks (upload/delete — before/after/failed)
- [ ] Write unit tests: allowlist, size cap, UUID naming, cascade delete
- [ ] Write integration tests: upload → send message with attachment → verify CLI receives path

### 8.2 Frontend
- [ ] Build file upload in input bar (button, drag-drop, multi-select)
- [ ] Build pending attachment chips (preview before send)
- [ ] Build attachment viewer (blob URL popup for protected downloads)
- [ ] Write E2E test: upload file → send message → see attachment in chat

**Gate:** Users can upload files in chat and workers can access them.

---

## Phase 9: Notifications and Announcements

### 9.1 Push Notifications
- [ ] Implement VAPID key generation and persistence
- [ ] Implement push subscription CRUD endpoints
- [ ] Implement push send on: session completion, worker step completion, card terminal state
- [ ] Implement endpoint pruning (410/404 auto-remove)
- [ ] Wire push hooks (send/subscribe/unsubscribe — before/after/failed)
- [ ] Implement service worker registration in frontend
- [ ] Write integration tests: subscribe → trigger notification → verify push sent

### 9.2 Announcements
- [ ] Implement announcement CRUD (create, get current, dismiss with compare-and-clear)
- [ ] Implement WS broadcast on announcement change
- [ ] Wire announcement hooks (create/dismiss — before/after)
- [ ] Build announcement banner component (dismissible, sticky)
- [ ] Write unit tests: compare-and-clear race safety

### 9.3 Queued Messages
- [ ] Implement queue set/get/delete/deliver operations
- [ ] Implement auto-deliver on agent turn completion
- [ ] Wire queue hooks (set/deliver/delete — before/after)
- [ ] Implement queue UI (show pending follow-up, edit, cancel)
- [ ] Write unit tests: queue lifecycle, deliver timing

**Gate:** Push notifications, announcements, and queued messages all working.

---

## Phase 10: Supporting Features

### 10.1 Git Integration
- [ ] Implement repo scanning (walk folders for .git directories)
- [ ] Implement diff and commit log endpoints
- [ ] Build diff viewer component (diff2html)
- [ ] Build commit log component (expandable diffs)
- [ ] Write integration tests: scan → diff → commit log

### 10.2 TLS
- [ ] Implement self-signed cert generation (ECDSA P-256, rcgen)
- [ ] Implement auto-renewal (24h check, 30-day window)
- [ ] Implement user-provided cert passthrough
- [ ] Implement HTTPS listener alongside HTTP
- [ ] Write unit tests: cert generation, renewal window

### 10.3 mDNS
- [ ] Implement mDNS advertisement (`<name>.local` at HTTPS port)
- [ ] Implement name generation (adjective-animal-color + digit)
- [ ] Implement republish on wake
- [ ] Implement unpublish on shutdown
- [ ] Write unit tests: name generation, DNS-label validation

### 10.4 Keep-Awake
- [ ] Implement macOS `caffeinate` spawning with watchdog respawn
- [ ] Implement platform detection (supported on macOS/Windows only)
- [ ] Implement toggle endpoint and UI control
- [ ] Write unit tests: spawn/kill, watchdog behavior

### 10.5 Model Registry
- [ ] Implement alias seeding (opus/sonnet/haiku/default)
- [ ] Implement model discovery from CLI transcripts
- [ ] Implement model/effort resolution (card > step > project > config)
- [ ] Implement model sanitization (regex validation)
- [ ] Build model picker component
- [ ] Build effort picker component
- [ ] Write unit tests: resolution precedence, sanitization

### 10.6 Workflows
- [ ] Implement built-in workflow registry (task, research, breakdown, fast-develop, deep-develop)
- [ ] Implement workflow resolution for cards
- [ ] Build workflow picker component
- [ ] Write unit tests: step lookup, empty step skipping

### 10.7 Configuration
- [ ] Implement config loading (CLI args > env > config.json > defaults)
- [ ] Implement config get/put endpoints
- [ ] Implement first-run bootstrap (no-users check → show registration)
- [ ] Wire config hooks (update — before/after)
- [ ] Build options/settings page
- [ ] Write unit tests: precedence, validation

### 10.8 Theming
- [ ] Implement CSS custom properties (light/dark/auto)
- [ ] Implement primary hue picker
- [ ] Implement theme persistence (localStorage)
- [ ] Implement auto mode (prefers-color-scheme media query)

**Gate:** All supporting features working. Full feature set complete.

---

## Phase 11: Server Lifecycle and Hardening

### 11.1 Server Lifecycle
- [ ] Implement graceful shutdown (signal handler, drain connections, stop workers, close DB)
- [ ] Implement startup state repair (detect dangling agent-starts, synthesize agent-end{crashed})
- [ ] Implement session recovery (resume workers with --resume, detect missing folders)
- [ ] Wire server lifecycle hooks (started, shutdown before/after)
- [ ] Wire HTTP route hooks (request before/after/failed with per-plugin endpoint grants)
- [ ] Write integration tests: start → create state → kill → restart → verify recovery

### 11.2 Security Hardening
- [ ] Implement CSP headers
- [ ] Implement origin/CSRF protection
- [ ] Implement body size limits (20 MB JSON, 1 MB report body)
- [ ] Implement loopback gating for MCP internal routes
- [ ] Implement WS auth sweep
- [ ] Write security-focused integration tests: cross-origin rejection, oversized body rejection, expired token rejection

### 11.3 Comprehensive E2E Tests
- [ ] Set up Playwright for E2E testing
- [ ] Implement mock/fake CLI provider (configurable: echo, streaming delay, tool use simulation, crash simulation)
- [ ] E2E: First boot → register → create folder → create session → chat → see streaming response
- [ ] E2E: Create project → add cards → workers run → cards advance through pipeline → done
- [ ] E2E: Worker crashes → recovery → money-loop block after 4 crashes
- [ ] E2E: Two tabs → same session → both see events in sync
- [ ] E2E: Report written by worker → viewed in UI → edited → downloaded
- [ ] E2E: Mobile viewport → session drawer → chat → kanban
- [ ] E2E: Plugin loaded → hook fires → modifies response

**Gate:** Production-ready. All features working, tested, hardened.
