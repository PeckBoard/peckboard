# HTTP API Routes

All `/api/*` routes require bearer token auth unless noted. Global middleware: Helmet CSP, 20MB JSON body limit, Origin gate, auth gate.

## Auth

| Method | Path                      | Auth   | Rate Limit | Description                                                                    |
| ------ | ------------------------- | ------ | ---------- | ------------------------------------------------------------------------------ |
| GET    | /api/auth/status          | Public | -          | Returns `{ passwordSet: boolean }`                                             |
| POST   | /api/auth/login           | Public | 5/min      | Body: `{ password, rememberMe? }`. Returns `{ token, expiresAt }`              |
| POST   | /api/auth/logout          | Bearer | -          | Revokes calling token                                                          |
| POST   | /api/auth/change-password | Bearer | 5/min      | Body: `{ currentPassword, newPassword }`. Revokes all tokens, issues fresh one |
| POST   | /api/auth/logout-others   | Bearer | -          | Revokes all tokens except caller's. Returns `{ removed }`                      |

## Sessions

| Method | Path                                | Rate Limit | Description                                                                   |
| ------ | ----------------------------------- | ---------- | ----------------------------------------------------------------------------- |
| GET    | /api/sessions                       | -          | List plain sessions (excludes workers). Enriched with conversationId + status |
| POST   | /api/sessions                       | 60/min     | Create session. Body: `{ name, dir, model?, effort?, conversationId? }`       |
| GET    | /api/sessions/:id                   | -          | Get single session                                                            |
| PATCH  | /api/sessions/:id                   | -          | Update name/model/effort. Model/effort change kills live process              |
| DELETE | /api/sessions/:id                   | -          | Kill process, delete session + attachments + queue entry                      |
| GET    | /api/sessions/:id/events            | -          | Query params: `afterSeq`, `limit` (1-5000). Returns events oldest-to-newest   |
| GET    | /api/sessions/:id/messages          | -          | Legacy chat messages. Query: `offset`, `limit`                                |
| POST   | /api/sessions/:id/messages          | -          | Append chat messages. Body: `{ messages: ChatMessage[] }`                     |
| POST   | /api/sessions/:id/clear             | -          | Kill process, reset conversation, delete attachments                          |
| POST   | /api/sessions/:id/read              | 60/min     | Append session-read event (syncs unread state across devices)                 |
| GET    | /api/worker-sessions                | -          | Current worker session mapping by project                                     |
| GET    | /api/existing-sessions              | -          | List Claude CLI sessions from `~/.claude`. Query: `dir?`                      |
| GET    | /api/existing-sessions/:id/messages | -          | Extract messages from existing CLI session JSONL                              |
| GET    | /api/queued                         | -          | List all queued follow-up messages                                            |

## Session Attachments

| Method | Path                               | Rate Limit | Description                                                    |
| ------ | ---------------------------------- | ---------- | -------------------------------------------------------------- |
| GET    | /api/sessions/:id/attachments      | -          | List attachments for session                                   |
| POST   | /api/sessions/:id/attachments      | 20/min     | Upload. Body: `{ fileName, contentBase64 }`                    |
| DELETE | /api/sessions/:id/attachments/:aid | -          | Delete single attachment                                       |
| GET    | /api/sessions/:id/attachments/:aid | -          | Download attachment (Content-Disposition: attachment, nosniff) |

## Projects

| Method | Path                     | Rate Limit | Description                                                                                                                             |
| ------ | ------------------------ | ---------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| GET    | /api/projects            | -          | List projects with steps and card counts                                                                                                |
| POST   | /api/projects            | 60/min     | Create project. Body: `{ name, projectContext, workingDir, workerCount, defaultWorkflowSlug?, parallelInstructions?, model?, effort? }` |
| GET    | /api/projects/:id        | -          | Get project with steps, cards, worker status                                                                                            |
| PUT    | /api/projects/:id        | -          | Update project fields                                                                                                                   |
| DELETE | /api/projects/:id        | -          | Pause, kill all workers, delete project                                                                                                 |
| POST   | /api/projects/:id/pause  | -          | Pause workers                                                                                                                           |
| POST   | /api/projects/:id/resume | -          | Resume workers                                                                                                                          |

## Cards

| Method | Path                                        | Rate Limit | Description                                                                                                     |
| ------ | ------------------------------------------- | ---------- | --------------------------------------------------------------------------------------------------------------- |
| GET    | /api/projects/:id/cards                     | -          | List cards for project                                                                                          |
| POST   | /api/projects/:id/cards                     | 60/min     | Create card. Body: `{ title, description?, workflowSlug?, priority?, model?, effort?, blocked?, blockReason? }` |
| PUT    | /api/projects/:id/cards/:tid                | -          | Update card (subject to edit policy)                                                                            |
| DELETE | /api/projects/:id/cards/:tid                | -          | Delete card                                                                                                     |
| POST   | /api/projects/:id/cards/:tid/stop           | -          | Stop worker                                                                                                     |
| POST   | /api/projects/:id/cards/:tid/cancel-wont-do | -          | Cancel worker and mark won't-do                                                                                 |
| POST   | /api/projects/:id/cards/:tid/restart        | -          | Restart worker                                                                                                  |
| GET    | /api/projects/:id/steps                     | -          | List pipeline steps                                                                                             |

## Reports

| Method | Path                                   | Rate Limit | Description                                                                 |
| ------ | -------------------------------------- | ---------- | --------------------------------------------------------------------------- |
| GET    | /api/reports                           | -          | List all report metadata                                                    |
| GET    | /api/reports/attachments               | -          | List all report attachments                                                 |
| GET    | /api/reports/:folder/:file             | -          | Get report with parsed frontmatter + body                                   |
| PUT    | /api/reports/:folder/:file             | 60/min     | Update report body. Body: `{ markdown }`. Frontmatter preserved             |
| GET    | /api/reports/:folder/:file/download    | -          | Download raw .md file                                                       |
| GET    | /api/reports/:folder/attachments/:file | -          | Download attachment (Content-Disposition: attachment, nosniff)              |
| GET    | /api/reports/:folder/zip               | -          | Download folder as .zip                                                     |
| POST   | /api/reports/:folder/:file/discuss     | -          | Create session with report as attachment. Returns `{ session, attachment }` |

## Config and Misc

| Method | Path                      | Description                                                     |
| ------ | ------------------------- | --------------------------------------------------------------- |
| GET    | /api/config               | Get defaults (models, effort, projectsDir)                      |
| PUT    | /api/config               | Update defaults                                                 |
| GET    | /api/models               | Get model registry (aliases, discovered IDs, Bedrock detection) |
| GET    | /api/workflows            | List built-in workflows                                         |
| GET    | /api/announcement         | Get current announcement or null                                |
| POST   | /api/announcement/dismiss | Dismiss by id (compare-and-clear)                               |
| GET    | /api/statusline           | Get delegated status line text (cached 15s)                     |
| GET    | /api/keep-awake           | Get sleep-blocker status                                        |
| PUT    | /api/keep-awake           | Toggle sleep-blocker. Body: `{ enabled }`                       |

## Push Notifications

| Method | Path                | Description                                                         |
| ------ | ------------------- | ------------------------------------------------------------------- |
| GET    | /api/push/vapid-key | Get VAPID public key                                                |
| POST   | /api/push/subscribe | Add push subscription. Body: `{ endpoint, keys: { p256dh, auth } }` |
| DELETE | /api/push/subscribe | Remove subscription. Body: `{ endpoint }`                           |
| POST   | /api/push/mark-read | Mark notification read. Body: `{ endpoint }`                        |

## MCP Internal Routes (/api/internal/mcp/\*)

Auth: Worker token (not session token). Loopback peer required.

| Method | Path                 | Token Role  | Description                                            |
| ------ | -------------------- | ----------- | ------------------------------------------------------ |
| POST   | create_card          | Any         | Create card. Workers default to own projectId          |
| GET    | list_projects        | Any         | List all projects                                      |
| GET    | list_workflows       | Any         | List built-in workflows                                |
| GET    | list_cards           | Any         | List cards with filters. Workers scoped to own project |
| POST   | complete_step        | Worker only | Signal step completion                                 |
| POST   | finish_card          | Worker only | Skip remaining steps, mark done                        |
| POST   | wont_do_card         | Worker only | Park in won't-do column                                |
| POST   | ask_user             | Worker only | Block card with question                               |
| POST   | create_project       | Any         | Create project with initial cards                      |
| PUT    | update_card          | Any         | Update card fields                                     |
| PUT    | update_project       | Any         | Update project fields                                  |
| POST   | pause_project        | Any         | Pause project                                          |
| POST   | resume_project       | Any         | Resume project                                         |
| DELETE | delete_card          | Any         | Delete card                                            |
| POST   | move_card_to_done    | Any         | Move card to done                                      |
| POST   | move_card_to_wont_do | Any         | Move card to won't-do                                  |
| POST   | write_report         | Any         | Write markdown report to disk                          |
| POST   | attach_report_file   | Any         | Write binary attachment to disk                        |
