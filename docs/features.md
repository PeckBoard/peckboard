---
title: Feature Tour
nav_order: 8
---

# Feature Tour

[Core Concepts]({{ "/core-concepts.html" | relative_url }}) covers the board, cards, and sessions; this page tours everything else a working install gives you, grouped by where you meet it.

## Assistant {#voice-assistant}

![The Assistant panel docked over the sessions list, answering a relayed question from a worker session]({{ "/assets/screenshots/voice-assistant.png" | relative_url }})

Press **Listen** (the microphone in the navigation rail) and talk. The panel docks top-right and keeps the microphone on, so there is nothing to hold down; you can also type into it. One global Assistant session works across every folder:

- **Reads and manages your work** — sessions, cards, and projects in any folder: "what's running?", "add a card to the storefront board for the search bug".
- **Drives the UI** — "open the storefront project" opens it; the same goes for sessions and cards by name.
- **Routes work only when you ask** — it hands a task to another session when you tell it to, never on its own initiative.
- **Relays questions from other sessions** — when a session needs a decision, the assistant brings it to you one topic at a time and answers on your behalf. It waits for you to finish; it never interrupts you.
- **Instant barge-in** — start talking and it stops mid-sentence. Content you cut off is not replayed.
- **A thinking cue** — a soft sound from the moment you finish until the reply starts, plus a short filler ("One sec.") if the answer takes a moment.

Speech comes from [Kokoro](https://github.com/hexgrad/kokoro), a natural-sounding text-to-speech model that runs on your PeckBoard server — the model downloads on first use and no audio leaves your machine for synthesis. Replies are fully phonetic: the assistant marks the pronunciation of every spoken word, so project names and identifiers come out right. When Kokoro isn't available, the browser's built-in voice takes over.

Settings → Assistant holds the rest (the old `/settings/voice` link still works):

- **Speech** — voice picker (Kokoro voices and browser voices), speed, and recognition language.
- **Pronunciations** — an editable list of words and how to say them. You can also add one by voice: "pronounce Kubectl like cube control".
- **Assistant Prompt** — the assistant's system prompt, editable live, with version history, diffs, restore, and reset to default.
- **Conversation Mirror** (admins) — posts what you say and the Assistant's replies, never its thinking or tool output, to a Slack incoming webhook and/or a Discord channel webhook as the conversation happens. When no device has the Assistant panel open, turns are also collected into an email digest sent over your SMTP server. Code blocks are redacted by default, and secrets are masked on a best-effort basis, but Slack, Discord, and your mail provider keep the transcript. Each channel has a **Send test** button and shows when it last delivered.

### Browser Support

| What                       | Works in                                                                                                         |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| Listening (speech to text) | Chrome, Edge, and Safari, including iPhone and iPad. Not Firefox.                                                |
| Kokoro speech              | Any modern browser.                                                                                              |
| Browser-voice fallback     | Needs a system voice. On Linux, install one, e.g. `speech-dispatcher` with `espeak-ng`, and restart the browser. |

## In the Chat

Messages accept image attachments — paste or drop them in — and `@` mentions that reference other sessions, so "look at what @Fix cart rounding did" hands the agent a real pointer. When an agent needs a decision it asks with a question card (multiple choice or free text) that pauses its turn until you answer; questions queue up if you are away, and a pending question can also reach you as a notification. When a git operation under an agent needs credentials, an askpass dialog surfaces in the session tab and the secret goes to git without entering the transcript.

Every session shows its agent's live tool calls as structured blocks — a diff for an edit, a table for a search, a replay link for a browser run — its todo list, and, when the agent spawns subagents with `spawn_subagent`, each subagent's transcript nested under the parent. Chat sessions can be handed a named _system prompt_ from the prompt library (Settings → System Prompts), and every session and card carries an _effort_ level (low to max) alongside its model. Cost-aware model autoswitch, on by default for workers and opt-in for chats, drops a session to a cheaper model when a turn does not need the expensive one.

## Subagent Panes

When a session launches subagents — PeckBoard children via `spawn_subagent` or Claude-native Agent/Task subagents — each running one slides into its own live split pane beside the parent chat, and the panes tile into a grid as more arrive. A pane stays open exactly as long as its subagent runs, including background subagents that outlive the parent's turn, then closes. The **Subagent panes: Auto / Off** toggle in the session view controls this. A pane you close stays closed across reloads; running subagents without a pane stay one click away on a `+N` chip, and a finished subagent reopens from its tool card's _Show pane_. To group arbitrary sessions side by side, pick _New split view_ from the tab bar's `+ ▾` menu.

## Background Processes

Agents start long-running jobs — dev servers, builds, long test runs — with the `run_background` MCP tool instead of backgrounding them in a shell. It follows the same approval gate and exec rules as `run_command`, returns a task id immediately, and PeckBoard owns the child: it runs in its own process group, which is killed as a whole on `stop_background`, on timeout, when the session is deleted, and on server shutdown. Output goes to a capped log under the data directory. When the job finishes, fails, times out, or is stopped, a report with the last 40 lines lands in the originating session and wakes it, so the agent picks up where it left off. `background_status` and `list_background` let the agent check in meanwhile, and the **Background** chip in the chat toolbar opens a panel listing each task with its output tail and a Stop button.

## Plans

Ask a chat session for a plan and it lands as a versioned document you can read and comment on from the Plans view — each comment becomes feedback the model revises against. When the plan is right, the implement wizard turns it into cards on a project board, so the plan-to-work handoff is explicit instead of buried in chat history.

## The Shell Around the Work

Open sessions, projects, and views live in a tab strip that survives reloads; tabs pin, close from their context menu, and deep-link. The Folders page registers the directories work happens in, and each folder's repo browser lists every git repo and worktree inside it — with per-repo launches for plugins like [Graphify]({{ "/plugins/graphify.html" | relative_url }}) and [Project Planner]({{ "/plugins/project-planner.html" | relative_url }}). Keyboard shortcuts cover the common moves (press `?` for the list), and Settings → Appearance holds theme, accent color, text size, density, motion, and optional sounds.

## Usage and Cost

The Usage dashboard rolls up tokens and cost per day, model, project, and session, with trend charts and per-session drill-down to individual turns and operations. Providers that report subscription plan usage (Claude, for example) show it on their account rows, and each account can carry a budget. A project can also carry a spend budget that pauses it when exceeded.

## Accounts, Access, and Security

PeckBoard is multi-user: admins manage users and roles from Settings → Users, and each user can enable TOTP two-factor authentication and review or revoke their signed-in devices. Settings → Security (admin-only) governs what agents may do on the host: the Claude tool permission list, the approved-commands allowlist for `run_command`, and session hooks. Environment variables for agent processes are stored encrypted and unlock with a passphrase per browser session; agent variables — plain key-value context for prompts — live alongside them in Settings → Variables.

## Server Care

Settings → Server shows ports, the data directory, and in-app software updates — PeckBoard downloads its own new release and restarts into it. Settings → Data holds one-click backup archives (restorable with `--restore-from`), and retention settings that prune old events, temp sessions, and recordings on a schedule. HTTPS is on by default with a self-signed certificate you can replace with your own, and browser push notifications can reach you when a question is pending or a card lands. Crashes of the server itself are captured with an OOM-kill detector so the next start can tell you what happened.

## Remote-Control Agents

The `peckboard-agent` daemon enrolls another machine — a laptop, a build box, a Windows VM — under your PeckBoard, and the Agents view lists every enrolled machine with its live state. Sessions can then run commands, manage servers, take screenshots, and drive keyboard and mouse on that machine through the `remote_agent_*` tools, each capability switched on per machine in the agent's own configuration. [Remote Agent]({{ "/remote-agent.html" | relative_url }}) covers downloading, enrolling, and running the agent.

## Remote Access

The PeckBoard app for iPhone, Android, macOS, and Windows reaches your box from anywhere through a relay, with no port forwarding: Settings → Connections → Remote Access pairs each device with its own QR code or link. Connections go direct when the network allows and through the end-to-end encrypted relay otherwise. [Remote Access]({{ "/remote-access.html" | relative_url }}) covers setup, the [official relay]({{ "/remote-access/official-relay.html" | relative_url }}), and [running your own]({{ "/remote-access/self-hosted-relay.html" | relative_url }}).

## Repeating Tasks, Reports, and Workflows

These are covered in [Core Concepts]({{ "/core-concepts.html" | relative_url }}); the piece easy to miss is that workflows are editable — Settings → Workflows defines custom step sequences, and per-project workflow instructions tell workers how to behave at each step of the one the project uses.
