# Data Models

## Database

Single SQLite file at `<dataDir>/peckboard.db`. Schema managed by Diesel ORM with migrations embedded in the binary at compile time. Migrations run automatically on startup via `run_pending_migrations()`.

WAL mode enabled for concurrent read performance. Foreign keys enforced.

---

## Tables

### `folders`

Configured working directories. Managed via the folder management page. Sessions and projects reference a folder by ID.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| id | TEXT | PK | UUID |
| name | TEXT | NOT NULL | Display name (e.g. "My App") |
| path | TEXT | NOT NULL, UNIQUE | Absolute filesystem path |
| created_at | TEXT | NOT NULL | ISO timestamp |

When creating a new session, the user picks a folder from a dropdown of configured folders.

**Deletion behavior:** When deleting a folder, the user is prompted with three options:
1. **Delete all sessions** in this folder, then delete the folder
2. **Move sessions** to a different folder (excluding the one being deleted), then delete the folder
3. **Cancel** — abort the deletion

### `sessions`

Chat sessions — both interactive (user at keyboard) and worker (autonomous agent on a card).

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| id | TEXT | PK | UUID |
| name | TEXT | NOT NULL | Display name |
| folder_id | TEXT | NOT NULL, FK → folders.id | Working directory for this session |
| model | TEXT | | Model override (nullable) |
| effort | TEXT | | Effort override (nullable) |
| is_worker | BOOLEAN | NOT NULL, DEFAULT FALSE | True if this is a card's worker session. Worker sessions do not appear in the main session list — they are accessed via the card's 3-dot menu → "Session" |
| project_id | TEXT | FK → projects.id | Set for worker sessions |
| card_id | TEXT | FK → cards.id | Set for worker sessions |
| conversation_id | TEXT | | Claude CLI conversation ID for --resume |
| created_at | TEXT | NOT NULL | ISO timestamp |
| last_activity | TEXT | NOT NULL | ISO timestamp |

**Recovery:** When resuming a session whose `folder_id` references a folder that no longer exists (deleted out-of-band or DB inconsistency), a `folder-missing` event is appended to the session's event log.

### `projects`

A project groups cards and workers around a shared codebase.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| id | TEXT | PK | UUID |
| name | TEXT | NOT NULL | Display name |
| context | TEXT | NOT NULL, DEFAULT '' | Context given to every worker |
| folder_id | TEXT | NOT NULL, FK → folders.id | Working directory for this project |
| worker_count | INTEGER | NOT NULL, DEFAULT 1 | Max parallel workers |
| status | TEXT | NOT NULL, DEFAULT 'active' | 'active' or 'paused' |
| default_workflow | TEXT | | Pre-selected workflow slug for new cards |
| model | TEXT | | Project-level model override |
| effort | TEXT | | Project-level effort override |
| parallel_instructions | BOOLEAN | NOT NULL, DEFAULT FALSE | Append git-worktree instructions |
| created_at | TEXT | NOT NULL | ISO timestamp |
| last_accessed_at | TEXT | NOT NULL | ISO timestamp |

### `cards`

A card is one unit of work on a project's kanban board.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| id | TEXT | PK | UUID |
| project_id | TEXT | NOT NULL, FK → projects.id | Parent project |
| title | TEXT | NOT NULL | |
| description | TEXT | NOT NULL, DEFAULT '' | |
| step | TEXT | NOT NULL, DEFAULT 'backlog' | Pipeline step (backlog/execution/validation/acceptance/wont-do/done) |
| priority | INTEGER | NOT NULL, DEFAULT 3 | 0 (highest) to 5 (lowest) |
| workflow | TEXT | | Workflow slug |
| model | TEXT | | Card-level model override (highest priority) |
| effort | TEXT | | Card-level effort override |
| worker_session_id | TEXT | FK → sessions.id | Currently active worker |
| last_worker_session_id | TEXT | FK → sessions.id | Most recent completed worker |
| handoff_context | TEXT | | Context passed between pipeline steps |
| blocked | BOOLEAN | NOT NULL, DEFAULT FALSE | Parked / not ready |
| block_reason | TEXT | | Human-readable reason shown on kanban card |
| created_at | TEXT | NOT NULL | ISO timestamp |
| updated_at | TEXT | NOT NULL | ISO timestamp |

**Edit policy:**
- Terminal cards (done/wont-do): fully read-only except `step` (for reopen via drag)
- Backlog-only fields (description, workflow): frozen once the card leaves backlog
- Other fields (title, priority, model, effort, blocked, block_reason): editable in any non-terminal state

### `events`

Per-session event log. Append-only. The `data` column is JSON text because event payloads are polymorphic by `kind`.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| id | TEXT | PK | UUID |
| session_id | TEXT | NOT NULL, FK → sessions.id | Owning session |
| seq | INTEGER | NOT NULL | Monotonic per session |
| ts | INTEGER | NOT NULL | Milliseconds since epoch (server-stamped) |
| kind | TEXT | NOT NULL | Event kind discriminator |
| data | TEXT | NOT NULL, DEFAULT '{}' | JSON payload (shape depends on kind) |

UNIQUE constraint on `(session_id, seq)`. Index on `(session_id, seq)`.

See [event-log.md](event-log.md) for event kinds and their data shapes.

### `users`

User accounts. The first user created is automatically an admin.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| id | TEXT | PK | UUID |
| username | TEXT | NOT NULL, UNIQUE | Login name |
| email | TEXT | UNIQUE | Optional email address |
| password_hash | TEXT | NOT NULL | Argon2 hash |
| role | TEXT | NOT NULL, DEFAULT 'user' | 'admin' or 'user' |
| created_at | TEXT | NOT NULL | ISO timestamp |
| updated_at | TEXT | NOT NULL | ISO timestamp |

On first boot with no users, the app shows a registration page. The first registered user is assigned role `admin`. Admin users can manage other users via the user management page.

### `auth_sessions`

JWT tokens stored server-side so they can be listed, expired, and revoked per user.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| id | TEXT | PK | UUID (matches JWT `jti` claim) |
| user_id | TEXT | NOT NULL, FK → users.id | Owning user |
| token_hash | TEXT | NOT NULL | SHA-256 of the JWT (for lookup without storing raw) |
| created_at | INTEGER | NOT NULL | Milliseconds since epoch |
| expires_at | INTEGER | NOT NULL | Milliseconds since epoch |
| last_used_at | INTEGER | | Milliseconds since epoch (updated on activity) |
| user_agent | TEXT | | Client user-agent string |
| ip_address | TEXT | | Client IP at creation |

Users can view their active auth sessions and revoke individual ones or all others. Admins can revoke any user's sessions.

### `push_subscriptions`

Web push notification subscriptions.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| endpoint | TEXT | PK | Push service endpoint URL |
| p256dh | TEXT | NOT NULL | Client public key |
| auth_key | TEXT | NOT NULL | Client auth secret |
| created_at | TEXT | NOT NULL | ISO timestamp |

### `queued_messages`

Follow-up messages queued while the agent is mid-turn.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| session_id | TEXT | PK, FK → sessions.id | One queued message per session |
| text | TEXT | NOT NULL | Message content |
| queued_at | TEXT | NOT NULL | ISO timestamp |

### `announcements`

Global sticky banner (e.g. auth errors).

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| id | TEXT | PK | UUID |
| kind | TEXT | NOT NULL | 'auth-error' or 'info' |
| title | TEXT | NOT NULL | |
| message | TEXT | NOT NULL | |
| detail | TEXT | | Optional extra info |
| created_at | TEXT | NOT NULL | ISO timestamp |

---

## Migrations

Managed by Diesel ORM. Migration SQL files live in `migrations/` in the source repo. At compile time, `embed_migrations!()` bakes them into the binary. On startup, `run_pending_migrations()` applies any unapplied migrations automatically.

No external migration tool or SQL files are needed at runtime — the binary is self-contained.

---

## Model Resolution

Effective model for a worker spawn (highest wins):
1. `cards.model`
2. Workflow step's `model`
3. `projects.model`
4. Config `defaultProjectModel`
5. CLI `default` alias (fallback)

Same four-tier precedence for effort.

---

## Workflow

Workflows are built-in (compiled into the binary, not stored in the database). Each workflow defines instructions for pipeline steps (execution, validation, acceptance) with optional per-step model/effort overrides. A card's `workflow` column references a workflow by slug.

Built-in workflows: `task`, `research`, `breakdown`, `fast-develop-software`, `deep-develop-software`.
