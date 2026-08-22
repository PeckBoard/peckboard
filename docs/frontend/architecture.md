# Frontend Architecture

React 19 + Zustand SPA, built with Vite, embedded into the Rust binary and
served as static files (`src/frontend.rs` — `rust_embed` over `web/dist/`,
with an `index.html` SPA fallback for any unmatched path).

Bootstrap (`web/src/main.tsx`):

1. `initAppearance()` runs **before** the first render so the persisted
   theme + accent hue are applied on the first frame.
2. `<App />` renders inside a top-level `ErrorBoundary`.
3. `navigator.serviceWorker.register('/sw.js')` when supported.

## Stores (Zustand)

There is **no single combined store**. `web/src/store/` holds 16
independent `create<...>()` stores; components subscribe to whichever
they need.

| Store                    | File                | Holds                                                                                                                                                                                                                                                                                                                                                           |
| ------------------------ | ------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `useAuthStore`           | `auth.ts`           | `initialized`, `authenticated`, `user`; `checkAuth`, `login`, `logout`, `changePassword`. Also exports `authedFetch`.                                                                                                                                                                                                                                           |
| `useSessionsStore`       | `sessions.ts`       | `sessions`, `sessionsLoaded`, `sessionsNextCursor`/`sessionsLoadingMore` (cursor paging), `activeSessionId`, `eventsBySession`, per-session load/error flags, `inputDrafts`, `pendingUserMessages`, `processing`, `unreadSessions`; session CRUD + lifecycle actions (`clearSession`, `cancelSession`, `interruptSession`, `terminateAgent`, `cancelPreHatch`). |
| `useWsStore`             | `ws.ts`             | `eventsBySession`, `lastSeqBySession`, `subscribedSessions`; `connect`/`disconnect`/`subscribe`/`unsubscribe`/`resume` and the raw-event listener registry.                                                                                                                                                                                                     |
| `useUiStore`             | `ui.ts`             | `connected`, `sidebarOpen`, `skipBacklogConfirm`. That is the whole store — theme, menus and modals are **not** here.                                                                                                                                                                                                                                           |
| `useTabsStore`           | `tabs.ts`           | `tabs` (the top tab strip), `loaded`; `fetchTabs`, `openTab`, `closeTab`, `removeTabsForItem`, `moveTab`. Also exports `startTabsAutoSync()`.                                                                                                                                                                                                                   |
| `useProjectsStore`       | `projects.ts`       | `projects`, `activeProjectId`, `cards`, `cardReportsByCard`, `pendingQuestionsByProject`, loaded/error flags; project + card CRUD.                                                                                                                                                                                                                              |
| `useFoldersStore`        | `folders.ts`        | `folders`; fetch/create/delete.                                                                                                                                                                                                                                                                                                                                 |
| `useReportsStore`        | `reports.ts`        | `reports`, `loading`, `error`; `fetchReports`.                                                                                                                                                                                                                                                                                                                  |
| `useResourcesStore`      | `resources.ts`      | `workflows`, `models`, `providers`, `systemPrompts`.                                                                                                                                                                                                                                                                                                            |
| `useRepeatingTasksStore` | `repeatingTasks.ts` | `tasks`, `sessionsByTask`; CRUD, `runNow`, `applyChange` (WS-driven).                                                                                                                                                                                                                                                                                           |
| `useWorkerCommsStore`    | `workerComms.ts`    | `workersByProject`, `messagesByProject`.                                                                                                                                                                                                                                                                                                                        |
| `useUsageStore`          | `usage.ts`          | `costTable`, `dashboard`, `range`/`resolved`, `failedPanels`, `lastUpdated`.                                                                                                                                                                                                                                                                                    |
| `useUsersStore`          | `users.ts`          | `users`; admin user management.                                                                                                                                                                                                                                                                                                                                 |
| `useClaudeAccountsStore` | `claudeAccounts.ts` | `accounts`, `planUsage`; login start + account CRUD.                                                                                                                                                                                                                                                                                                            |
| `useGrokAccountsStore`   | `grokAccounts.ts`   | Same shape for Grok accounts.                                                                                                                                                                                                                                                                                                                                   |
| `useKimiAccountsStore`   | `kimiAccounts.ts`   | Same shape for Kimi accounts.                                                                                                                                                                                                                                                                                                                                   |

State that is deliberately _not_ in a store:

- **Theme + accent hue** — `util/appearance.ts`, persisted to localStorage
  (`peckboard_theme`, `peckboard_hue`) and applied to
  `document.documentElement`.
- **Notification sounds** — `util/sounds.ts`, persisted to localStorage
  (`peckboard_sounds`). Synthesized over Web Audio. `SoundsListener` in
  `App.tsx` plays live WS events; Settings → Sounds toggles each kind.
  Frequent chimes (tool, send, run start, queue) default off.
- **Context menus / dropdowns** — `hooks/useContextMenu.tsx` +
  `components/ContextMenuView.tsx`, local to the trigger.
- **View, active ids, modal visibility** — `useState` in `App.tsx`,
  seeded from the URL.
- **Todos** — derived from the session's events
  (`types/todo.ts` → `latestTodoSnapshot`), seeded by
  `GET /api/sessions/:id/todos`.

## Event Log Rendering Pipeline

Raw `Event[]` → `buildDisplayItems` / `createDisplayItemsFolder`
(`components/chat/events.ts`) → `DisplayItem[]` → rendered inline by
`ChatView`.

`DisplayItem` is a discriminated union covering `user`, `pre-hatch`,
`assistant`, `tool`, `file-diff`, `thinking`, `turn-usage`, `status`,
`system`, `step`, `agent-start`, `agent-crashed`, `handover-start`,
`handover`, `handover-aborted`, `interrupt`, `question`,
`question-resolved`, and an `unknown` fallback so unrecognized event
kinds render as a collapsed row instead of being dropped.

The fold (`foldEvent` over a `FoldState`):

- Coalesces consecutive assistant / thinking chunks into one streaming
  bubble; live buffers render as trailing rows without being committed,
  so the next chunk grows the same bubble.
- Pairs `tool_use` with its result, attaches `file-diff` payloads and
  tool images, and closes still-open tools on turn end.
- Coalesces consecutive identical system notices into one row with a
  `×N` count.

`createDisplayItemsFolder()` returns an **incremental** builder: it reuses
all prior work when the new event list is an append-only extension (the
common case — one WS event or token chunk at a time) and rebuilds from
scratch when the list shape changed (session switch, "Load older"
prepend, snapshot merge), detected via the first/last consumed event ids.
Item object identity is stable across calls, so `React.memo` rows skip
re-rendering untouched history. `buildDisplayItems(events)` is the
one-shot form, used by `SubagentTranscript` and `util/transcript.ts`.

`ChatView` virtualizes the resulting rows with `useVirtualizer`
(`@tanstack/react-virtual`, `estimateSize: () => 64`).

## WebSocket Layer

`store/ws.ts` owns a single socket to `/ws`.

- On `open`: sends `{type:'auth', token}`; on `auth_ok` it marks
  `connected` and re-sends `subscribe` (plus `resume` with the stored
  `last_seq`) for every tracked session.
- `lastSeqBySession` is persisted to sessionStorage
  (`peckboard_last_seq`) so a reload resumes rather than replays.
- Reconnect backoff is exponential with ±25% jitter, capped at 30s.
- A server `resync` frame (broadcast slot overflow) re-runs `resume` from
  the last-seen seq for the named session — or every subscribed session
  when unnamed — and dispatches `peckboard:resync` so listeners can
  refetch global state that has no replay log.
- Non-event frames are fanned out as `window` CustomEvents named
  `peckboard:<type>`: `announcement`, `queue`, `card-update`,
  `project-update`, `card-delete`, `worker-question`, `plugin-approval`,
  `repeating-task-changed`, `repeating-task-run`, `pm-decisions-changed`,
  `askpass-request`/`askpass-resolved`,
  `env-unlock-request`/`env-unlock-resolved`.
- `session-deleted` is handled in-store: it drops cached events and seqs
  and calls `useSessionsStore.applySessionDeleted(id)`.

## URL Routing

No React Router — the URL is parsed and serialized directly in `App.tsx`
by `parseRoute()` (`App.tsx:109`) and `buildPath()` (`App.tsx:181`).

The `View` union (`App.tsx:52`):
`sessions | repeatingTasks | projects | usage | folders | reports |
users | settings | pluginPage | plan`.

`SessionSub` (`App.tsx:94`) is ``'chat' | 'todos' | `plugin:${string}` ``
and is only meaningful for the `sessions` and `projects` views.

| Path                                                         | View                                           |
| ------------------------------------------------------------ | ---------------------------------------------- |
| `/`                                                          | sessions (list)                                |
| `/sessions/:id`                                              | session chat                                   |
| `/sessions/:id/todos`                                        | session todos                                  |
| `/sessions/:id/plugin/:itemId`                               | plugin full page, session-scoped               |
| `/projects/:id`                                              | kanban board                                   |
| `/projects/:id/todos`                                        | project todos                                  |
| `/projects/:id/plugin/:itemId`                               | plugin full page, project-scoped               |
| `/repeating-tasks`, `/repeating-tasks/:id`                   | repeating tasks                                |
| `/usage`                                                     | usage dashboard                                |
| `/folders`                                                   | folders page                                   |
| `/reports`, `/reports/:folder/:file`                         | report browser / viewer                        |
| `/plan/:id`                                                  | plan viewer                                    |
| `/settings`, `/settings/:sub`                                | settings, optionally deep-linked to a sub-page |
| `/plugins`, `/plugin-settings`, `/plugin-registry`, `/users` | legacy redirects into settings sub-pages       |
| `/plugin-page/:plugin/:itemId`                               | plugin full page, rail entry                   |

The settings view keeps the active sub-page id as a string
(`settingsSub`); `/settings/<id>` deep-links straight to that page. The
legacy paths redirect into the matching sub-page — `/users` lands on
Settings → Users, which replaced the standalone user-management view.

Unknown first segments fall back to `sessions`. For the reports view the
`activeId` is the encoded `<folder>/<file>` pair — the same id used as the
report tab's `item_id`.

## Component Hierarchy

`App.tsx` renders `div.shell`:

- `PluginApprovalPrompt` — prompts for any WASM plugin awaiting hook
  approval.
- `nav.rail` (`aria-label="Primary"`) — the navigation rail; the shell
  stacks and the rail becomes a horizontal top bar under 768px
  (`useMediaQuery` in `App.tsx`, layout in `styles/mobile.css`). Contains the brand
  mark, Sessions, plugin-contributed rail entries, Repeating Tasks,
  Projects, Reports, Usage, a separator, Folders, the
  connection status dot, and the avatar button whose menu is portaled to
  `document.body` (Settings, plugin UI panels, Change password, Sign out).
- `main.content`
  - `TabBar` — the tab strip, driven by `useTabsStore` and the tab-kind
    registry in `components/tabKinds.tsx`.
  - Announcement banner, `ConnectionBanner`, `AskpassDialog`,
    `EnvUnlockDialog`.
  - `div#view-panel` (`role="tabpanel"`) wrapping an `ErrorBoundary` and
    the view switch:
    - `sessions` → `ChatView` / `SessionTodosView` / `PluginFullPage`, or
      the session `List` under a `ListViewHeader`.
    - `projects` → `KanbanBoard` / `ProjectTodosView` /
      `PluginFullPage`, or `ProjectList`.
    - `repeatingTasks` → `RepeatingTasksView`
    - `usage` → `UsageDashboard` (charts in `components/usage/`)
    - `folders` → `FoldersPage` (`components/ManageFoldersModal.tsx`)
    - `reports` → `ReportBrowser` / `ReportView`
    - `plan` → `PlanView`
    - `settings` → `SettingsPage` (grouped sidebar with search; includes
      user management, formerly its own `users` view)
    - `pluginPage` → `PluginFullPage`
- Modals rendered as siblings: `NewSessionModal`, `NewProjectModal`,
  `ChangePasswordModal`, `PluginPanelModal`, `RenameModal`, and one
  `ConfirmDialog` per destructive action (delete session / delete project
  / clear session / terminate agent / delete repeating task).

When `!authenticated`, `App` short-circuits to `<LoginModal />` before
rendering the shell.

`ChatView` composes the virtualized log, `TodoPanel` and `InputBar`
(auto-resizing textarea, `useMentions` autocomplete, attachment upload
and paste-to-upload, send). Worker sessions get the same composer; what
changes is the toolbar — the context-pressure banner and several 3-dot
actions are hidden when `sessionDetail.is_worker` is set. A tool row that
spawned a subagent renders `SubagentTranscript` inside `ToolUseBlock`.

## Shared Primitives

Reuse these rather than hand-rolling a second copy:

- `Modal` — dialog shell (`useDialogFocus` for focus trapping).
- `ConfirmDialog` — destructive-action confirmation.
- `List` + `ListViewHeader` — list views with a 3-dot menu, right-click /
  long-press context menu, multi-select and bulk actions.
- `Dropdown` / `MenuButton` — menus and searchable pickers.
- `useContextMenu` (+ `ContextMenuView`) — right-click on desktop,
  long-press on touch; both share `useMenuKeyboard`.
- `ModelPicker` (searchable), `SystemPromptPicker` (wraps `ModelPicker`)
  and `WorkflowSelect` — pickers over the resource catalogues, all built
  on `Dropdown`.
- `SafeMarkdown`, `MermaidBlock`, `DiffBlock`, `ToolUseBlock` — content
  rendering.

## Theming

CSS custom properties in `web/src/index.css`, in three blocks that must be
kept in sync: `:root` (light), `:root[data-theme='dark']`, and an
`@media (prefers-color-scheme: dark)` block scoped to
`:root:not([data-theme='light']):not([data-theme='dark'])` for auto mode.

- `--primary-hue` (0–360) drives `--accent`, `--accent-hover`,
  `--accent-subtle`, `--accent-muted` and `--ring` via HSL.
- Layered backgrounds `--bg` → `--surface` → `--surface2` → `--surface3`;
  text layers `--text` → `--text2` → `--text3`. Contrast ratios for the
  muted/ring tokens are recorded inline in `index.css` — re-measure before
  changing them.
- `--chart-1`…`--chart-6` are a separate categorical ramp, deliberately
  not derived from `--primary-hue`; slot order is the colour-blind-safety
  mechanism and must not be reordered.
- Plus tokens for semantic colours, borders, shadows, radii, spacing,
  typography, transitions and layout (`--header-h`, `--sidebar-w`,
  `--safe-bottom`, `--app-height`).
- Applied by `util/appearance.ts`: `applyTheme` sets/removes
  `data-theme`, `applyHue` sets `--primary-hue` inline; both persist to
  localStorage. `util/themeColor.ts` syncs the `theme-color` meta tag.
- Per-area styles live in `web/src/styles/*.css` and `App.css`.

- Plugin iframes: `withPluginTheme` (`util/appearance.ts`) appends
  `?theme=light|dark` to every plugin iframe `src` when the stored theme
  is explicit, and nothing for auto. Plugin pages stamp the param on
  their own `<html data-theme>` (stamp wins) and otherwise follow
  `prefers-color-scheme` — the same three-block CSS structure as core, so
  host and iframe resolve to the same scheme. Reference implementation:
  `peck-plugins/graphify/page/style.js`.

## Mobile Patterns

- Touch: 48px minimum tap targets (`styles/mobile.css`), long-press
  context menus at `LONG_PRESS_MS = 450` (`hooks/useContextMenu.tsx`),
  with synthetic-click suppression after the press fires.
- Keyboard: `window.matchMedia('(pointer: coarse)')` in `InputBar` —
  Enter inserts a newline on touch devices, submits on desktop.
- Viewport: `--app-height` is kept in sync with `window.visualViewport`
  in `App.tsx` so the app shrinks above the on-screen keyboard instead of
  drifting; it defaults to `100dvh` for the pre-JS paint.
- Layout: under 768px the shell stacks, the rail becomes a horizontal top
  bar, and the tab strip below it replaces the old session/project
  sidebar. Sizing is fluid; safe-area insets are honoured on both bars.

## Auth Flow

1. On mount `App` calls `checkAuth()`, which probes
   `GET /api/auth/me` with the stored token.
2. No valid token → `App` renders `<LoginModal />` instead of the shell.
3. `login()` POSTs `/api/auth/login`, then stores the token in
   localStorage (Remember Me) or sessionStorage (tab-only).
4. Once `authenticated`, `App` runs `connect()` (WS), `fetchSessions()`,
   `fetchProjects()`, `fetchFolders()`, `fetchTabs()` and
   `startTabsAutoSync()`, and pulls `/api/announcements`.
5. `authedFetch` attaches the bearer token; any 401 clears the token and
   the auth store, dropping back to the login screen.
6. `changePassword()` — the server revokes all sessions and mints a fresh
   token, which is written back into the same storage tier the user
   originally chose, so the current tab stays signed in.
