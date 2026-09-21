---
title: Feature Tour
nav_order: 8
---

# Feature Tour

[Core Concepts]({{ "/core-concepts.html" | relative_url }}) covers the board, cards, and sessions; this page tours everything else a working install gives you, grouped by where you meet it.

## In the Chat

Messages accept image attachments — paste or drop them in — and `@` mentions that reference other sessions, so "look at what @Fix cart rounding did" hands the agent a real pointer. When an agent needs a decision it asks with a question card (multiple choice or free text) that pauses its turn until you answer; questions queue up if you are away, and a pending question can also reach you as a notification. When a git operation under an agent needs credentials, an askpass dialog surfaces in the session tab and the secret goes to git without entering the transcript.

Every session shows its agent's live tool calls as structured blocks — a diff for an edit, a table for a search, a replay link for a browser run — its todo list, and, when the agent spawns subagents with `spawn_subagent`, each subagent's transcript nested under the parent. Chat sessions can be handed a named _system prompt_ from the prompt library (Settings → System Prompts), and every session and card carries an _effort_ level (low to max) alongside its model. Cost-aware model autoswitch, on by default for workers and opt-in for chats, drops a session to a cheaper model when a turn does not need the expensive one.

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

The `peckboard-agent` daemon enrolls another machine — a laptop, a build box, a Windows VM — under your PeckBoard, and the Agents view lists every enrolled machine with its capabilities. Sessions can then run commands, take screenshots, and drive keyboard and mouse on that machine through the `remote_agent_*` tools, with every capability gated at enrollment. [Getting Started]({{ "/getting-started.html" | relative_url }}) covers downloading and enrolling the agent.

## Repeating Tasks, Reports, and Workflows

These are covered in [Core Concepts]({{ "/core-concepts.html" | relative_url }}); the piece easy to miss is that workflows are editable — Settings → Workflows defines custom step sequences, and per-project workflow instructions tell workers how to behave at each step of the one the project uses.
