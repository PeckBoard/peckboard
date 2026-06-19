# Frontend Architecture

React + Zustand SPA, built with Vite, served as static files from the backend.

## Store Structure (Zustand)

10 slices combined into a single store:

### AuthSlice
- `authed` flag, `loginError`, `loginPending`
- `login(password, rememberMe)`, `logout()`, `changePassword()`, `logoutOtherSessions()`
- Token stored in localStorage (Remember Me) or sessionStorage (tab-only)

### SessionSlice
- `sessions[]`, `activeSessionId`
- `eventsBySession[sid]` — raw event log per session
- `lastSeqBySession[sid]` — for WS resume (persisted to sessionStorage)
- `inputDrafts[sid]` — unsent text (persisted to localStorage)
- `pendingAttachments[sid]` — uploaded but not sent
- `pendingQuestions[sid]` — AskUserQuestion prompts
- `sessionTodos[sid]` — latest TodoWrite snapshot
- `processing` — memoized set of session IDs with open agent-start
- `workerSessionInfo` — worker metadata from server broadcasts

### WsSlice
- `ws` socket, `connected` flag
- Reconnect backoff (exponential with ±25% jitter, max 30s)
- Event append/update operations
- `sendMessage`, `cancelMessage`, `interruptMessage`, `terminateSession`
- `answerQuestion`, `rejectQuestion`
- `resyncAll()` — full state bootstrap after reconnect

### ProjectSlice
- `projects[]`, `activeProjectId`, `activeCardId`
- `projectCards`, `projectSteps`, `workflows`, `modelRegistry`
- CRUD for projects and cards
- Card kanban updates

### UiSlice
- `view` — active tab: chat/diffs/commits/projects/reports/docs
- `drawerOpen` — session drawer visibility
- Modal flags: newSession, addCard, editProject, editCard, renameSession, options
- `theme` (auto/light/dark), `primaryHue` (0-360 for accent color)
- Sound/notification toggles (6 toggles for session/worker/card/send/tab-switch)
- `notification`, `confirmDialog`, `announcement`, `statusLine`, `keepAwake`

### UiControllerSlice
- `activePopoverId` — single popover exclusivity
- `contextMenu` state
- `claimPopover`/`releasePopover`

### GitSlice
- `repos[]`, `selectedRepo`, `currentDiff`, `commits`, `commitDiff`

### ReportsSlice
- `reports[]`, `attachments[]`, `selectedReportFolder`, `activeReport`
- Report fetch/open/download/edit operations

### DocsSlice
- `docsTree`, `activeDocsPath`, `activeDocsMarkdown`, `docsHistory`

### ConfigSlice
- `projectsDir`, `defaultSessionEffort`, `defaultProjectEffort`

## Event Log Rendering Pipeline

Raw events → `buildDisplayItems(events)` → DisplayItem[] → `EventLogRenderer`

`buildDisplayItems`:
- Groups consecutive `agent` chunks into streaming turns
- Extracts ChatMessages from user/agent/system events
- Marks step-change, agent-start, agent-end lifecycle events
- Output is memoized so re-renders don't rebuild

`EventLogRenderer`:
- Virtualized with @tanstack/react-virtual
- Renders message bodies (AssistantBody, UserBody, SystemBody, SegmentsBody)
- Renders WorkerStepHeader for step-change events with repeat counters
- Project/card chips are live references (current name, not snapshot)
- TodoWrite integration: synced from raw tool_use events in real time

## URL Routing

No React Router — URL is parsed/serialized directly in App.tsx:

- `/` — welcome
- `/sessions/:id` — chat view
- `/sessions/:id/diffs|commits|reports` — session-scoped views
- `/projects/:projectId` — kanban board
- `/projects/:projectId/cards/:cardId/session` — card-addressed session
- `/reports`, `/docs` — app-level views

## Component Hierarchy

- **App.tsx** — URL routing, auth bootstrap, theme application
- **Header** — tab bar with roving-tabindex navigation
- **SessionDrawer** — slide-in sidebar with session/project list
- **ChatView** — event log + InputBar (read-only for card sessions, interactive for chat sessions)
- **KanbanBoard** — cards in pipeline columns; each card's 3-dot menu has a "Session" action to view its worker session
- **ReportBrowser** — folder accordion + ReportViewer
- **InputBar** — auto-resize textarea, file upload, mic button, send
- **Modals** — LoginModal, NewSessionModal, NewProjectModal, AddCardModal, OptionsModal, ConfirmDialog
- **Global** — AnnouncementBanner, Notification toast, StatusLine, ContextMenu

## Theming

CSS custom properties on `:root` (light) with `:root[data-theme="dark"]` overrides.

- `--primary-hue` (0-360) drives accent color
- `--accent` derived from hue via HSL
- Layered backgrounds: `--bg` > `--surface` > `--surface2` > `--surface3`
- Text layers: `--text` (primary) > `--text2` > `--text3`
- Dark mode: IntelliJ Darcula-inspired indigo/violet palette
- Auto mode: respects `prefers-color-scheme` media query
- Persisted to localStorage

## Mobile Patterns

- Touch: 48px+ tap targets, long-press context menus (500ms), synthetic click suppression
- Keyboard: `pointer: coarse` detection — Enter=newline on mobile, Enter=submit on desktop
- Viewport: `position: fixed` app prevents iOS Safari keyboard drift; `--app-height` from `visualViewport.height`
- Layout: SessionDrawer max-width 85vw, fluid sizing, no fixed breakpoints

## Shared Code

`@shared/util/*` path alias maps to backend `src/util/` — shared validation rules:
- cardEditPolicy, passwordPolicy, effectiveModel, effectiveEffort
- pipelineSteps, workflows, effortLevels

These modules have zero Node.js dependencies so they run in both environments.

## Auth Flow

1. On load: check localStorage/sessionStorage for valid token
2. If expired or missing: show LoginModal
3. On login: POST /api/auth/login → store token → `connectWs()` → `resyncAll()`
4. On 401 from any API call: clear token → show LoginModal
5. On password change: server revokes all tokens, issues fresh one → reconnect WS
