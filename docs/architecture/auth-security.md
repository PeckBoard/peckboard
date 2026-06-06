# Auth and Security

## Users

- Multi-user system with `admin` and `user` roles
- On first boot with no users, the app shows a registration page
- The first registered user is automatically assigned the `admin` role
- Admins can create, edit, and delete users via the user management page
- Passwords hashed with Argon2
- Verification uses timing-safe comparison

## JWT Token Lifecycle

- Authentication issues a JWT token containing user ID, role, and expiry
- JWTs are stored server-side in the `auth_sessions` table (SHA-256 hash of the token, never raw)
- Server-side storage enables: listing active sessions, per-session revocation, and forced expiry
- Each auth session tracks user_id, creation time, expiry, last activity, user-agent, and IP
- Users can view their active auth sessions and revoke individual ones or all others
- Admins can revoke any user's sessions
- Changing password revokes all of that user's auth sessions
- Expired sessions purged on startup and opportunistically during validation

## HTTP Auth Middleware

- JWT extracted from `Authorization: Bearer <token>` header
- Validated: signature check, expiry, existence in `auth_sessions` table (revocation check)
- **Public routes** (no auth required):
  - `GET /api/auth/status` — check if any users exist (drives registration vs login page)
  - `POST /api/auth/register` — first user registration (disabled once an admin exists)
  - `POST /api/auth/login` — credential submission
  - `/api/internal/mcp/*` — separate worker token auth + loopback gating
- All other `/api/*` routes require valid JWT
- 401 response includes `WWW-Authenticate: Bearer realm="peckboard"`

## WebSocket Auth Handshake

- First frame must be `{type:"auth", token:"..."}` within 10 seconds
- Failure or timeout closes connection with code 4001
- Mutating frames (send, cancel, interrupt, queue ops, answer/reject question) re-validate the token before acting
- Background sweep (10s interval) checks all authed sockets for expired/revoked tokens; closes with 4001

## Origin / CSRF Protection

- Origin header compared against Host header (case-insensitive)
- Absent Origin treated as same-origin (same-origin requests, curl, MCP subprocess)
- Mismatched Origin returns 403
- No CORS headers set — browsers won't expose responses cross-origin
- `/api/internal/mcp/*` exempted from Origin check but requires loopback peer (127.0.0.1/::1)

## Rate Limiting

| Bucket | Limit | Applies to |
|--------|-------|------------|
| create | 60/min | POST sessions, projects, cards, report edits, session reads |
| attachment | 20/min | POST session attachments |
| login | 5/min | POST /api/auth/login |
| register | 5/min | POST /api/auth/register |
| mcpCreateCard | 60/min | POST /api/internal/mcp/create_card |

Per-IP failed attempt tracker adds linear delay ramp (0ms for first 2 failures, then 500ms × (count-2), capped at 5s). Memory-only, cleared on restart.

## Worker MCP Token Scoping

- Two context types: Worker (scoped to projectId + sessionId) and Session (sessionId only)
- 24-byte hex tokens, stored by SHA-256 hash as key (constant-time lookup)
- Issued on worker spawn, revoked on worker teardown
- Worker tokens can only call card-level MCP tools; session tokens call session-level tools

## TLS Certificate Management

- If `certPath` + `keyPath` both provided: used as-is, no auto-renewal
- Otherwise: self-signed cert generated under `<dataDir>/certs/`
- ECDSA P-256, SHA-256 signed, 365-day validity
- SANs: commonName, localhost, 127.0.0.1, ::1
- Auto-renewal check every 24h; regenerates within 30-day window of expiry
- Private key written with mode 0o600

## CSP Headers

- `script-src: 'self'` — no inline scripts, no eval
- `style-src: 'self'` — external stylesheets same-origin only
- `style-src-attr: 'unsafe-inline'` — React inline styles allowed
- `img-src: 'self' data: blob:` — attachments may arrive as blob/data URLs
- `connect-src: 'self'` — XHR and WebSocket same-origin
- `frame-ancestors: 'none'` — no iframe embedding
- `object-src: 'none'` — no plugins
- No `upgrade-insecure-requests` (HTTP intentionally supported for localhost)

## Body Size Limits

- JSON body: 20 MB (accommodates base64-encoded attachments up to ~10 MB raw)
- Oversized requests return clean 413 JSON error
- Report body edits capped at 1 MB
