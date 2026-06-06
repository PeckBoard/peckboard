# WebSocket Protocol

WebSocket upgrades on the same HTTP/HTTPS ports. All frames are JSON.

## Client to Server

### Auth (required first frame)

`{ type: "auth", token: "<bearer>" }`

Must arrive within 10 seconds or connection closes with code 4001.

### Session Management

| Frame | Fields | Description |
|-------|--------|-------------|
| subscribe | sessionId | Subscribe to events for a session |
| unsubscribe | sessionId | Unsubscribe |
| resume | sessionId, lastSeq | Replay events since lastSeq |

### Mutating Frames

These re-validate the token before acting:

| Frame | Fields | Description |
|-------|--------|-------------|
| send | sessionId, text, attachmentIds? | Send message to agent |
| cancel | sessionId | Kill agent process (hard stop) |
| interrupt | sessionId | Soft interrupt (abort API call, keep process) |
| queue-set | sessionId, text | Queue follow-up message |
| queue-delete | sessionId | Clear queued message |
| answer-question | sessionId, answers | Answer AskUserQuestion prompt |
| reject-question | sessionId, text? | Dismiss question |

Worker sessions reject `send` frames as defense-in-depth.

## Server to Client

### Auth Responses

| Frame | Fields | Description |
|-------|--------|-------------|
| auth-ok | expiresAt | Auth succeeded |
| auth-required | error | Non-auth frame sent before auth |
| auth-failed | error | Invalid token |
| auth-expired | reason | Token revoked or expired (close follows with code 4001) |

### Event Streaming

| Frame | Fields | Description |
|-------|--------|-------------|
| event | sessionId, seq, ts, kind, data, replay? | New or replayed event |
| event-update | sessionId, seq, ts, kind, data | In-place event mutation |
| resume-complete | sessionId, count | Replay finished |
| resume-too-far | sessionId, replayCap | Gap too large; client must refetch via HTTP |

### State Broadcasts

| Frame | Fields | Description |
|-------|--------|-------------|
| sessions-updated | sessions, workerSessionIds, workerSessions | Full session list refresh |
| projects-updated | projects, partial? | Project list (partial=true means merge by id) |
| card-terminal | projectId, cardId, cardTitle, terminalStep | Card reached done/wont-do |
| queue-updated | sessionId, text | Queued message changed |
| announcement-updated | announcement | Global banner changed or cleared |
| model-changed | sessionId, model | Worker changed model |

### Interactive

| Frame | Fields | Description |
|-------|--------|-------------|
| question | sessionId, question | AskUserQuestion surfaced |
| question-resolved | sessionId, requestId, answers/rejected | Question answered or dismissed |
| interrupted | sessionId | Soft interrupt succeeded |
| error | error | Error message |

## Reconnect Protocol

1. Client opens new WebSocket, sends auth frame
2. Server replies auth-ok
3. Client sends `resume` for each previously-subscribed session with its `lastSeq`
4. Server replays events since lastSeq (capped at 500)
5. If gap exceeds cap, server sends `resume-too-far` and client refetches via `GET /api/sessions/:id/events`

Client-side backoff: exponential (1s × 2^attempts, max 30s) with ±25% jitter. Reset on successful auth-ok.

`lastSeq` per session persisted in sessionStorage to survive tab refreshes.
