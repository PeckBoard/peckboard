---
title: How Skills Work
parent: Skills
nav_order: 1
---

# How Skills Work

A _skill_ is a reusable set of instructions an agent loads when a task calls for it — a pull-request review checklist, a release routine, a way of turning a bug report into a failing test. You write a skill once, then hand it to any session instead of pasting the same instructions into every prompt. PeckBoard keeps skills in three places: on your own instance, in a private git repository your team shares, and in the [public catalog](skills.html).

```mermaid
graph LR
  L[Local skills] --> S[Session]
  T[Team repo] -->|sync| S
  P[Public catalog] -->|subscribe| S
```

## The Skill Format

Every skill is a `SKILL.md` file: a short YAML header followed by ordinary Markdown. The format is compatible with Claude-style skills, so a skill written for PeckBoard works with other agents too.

```markdown
---
name: pr-review-checklist
description: Review a pull request and report findings ranked by severity.
version: 1.1.0
tags: [review, git]
groups: [pr-review]
---

# Pull Request Review Checklist

The instructions the agent follows...
```

The `name` is kebab-case (lowercase letters, digits, and dashes, up to 64 characters). The `description` is one line — agents read it to decide whether the skill applies, so say _when_ to use it. `tags` help search, and `groups` puts the skill into named bundles.

## Local, Team, and Public Skills

**Local** skills live in your instance's database. Create one under Settings → Skills → Installed with **+ New skill**, or ask an agent to make one for you. They never leave your machine unless you [publish](#publishing-a-skill) them.

**Team** skills come from a private git repository an admin adds under Settings → Skills → Sources with **+ Team repository**. PeckBoard clones it with the server's own git access (through the API a source can instead use a saved SSH key or a token from an environment variable), syncs it when you choose **Sync now**, and can publish a local skill into it as a commit — so everyone on the team works from the same reviewed set.

**Public** skills come from the [PeckBoard/skills](https://github.com/PeckBoard/skills) repository. Every instance has it as a source out of the box; browse it under Settings → Skills → Browse, or [search it here](skills.html).

## Groups

A _group_ bundles skills that are used together — "PR review skills" holds the review checklist, the security pass, and the commit-message guide. Give a session a group and it gets every skill in it, including skills added to the group later. Manage groups under Settings → Skills → Groups; team and public repos define their own groups in `groups/<slug>.json`.

## Versions

Skills are versioned with [semantic versions](https://semver.org). Editing a skill never changes an existing version: saving creates a new one (a patch, minor, or major bump), and every earlier version stays readable — open a skill to read any version or diff two of them. A published version in a team or public repo is immutable for the same reason — someone may have pinned it.

When you subscribe to a skill you choose either **Latest**, which moves to the newest version each time the source syncs, or a **pinned** version, which stays exactly as it is until you change it. Pick the version in the skill's version selector before you click **Subscribe**; to change it later, open the skill on the Installed tab and pick from **Version in use**.

## Subscribing

Subscribing copies a team or public skill onto your instance so sessions can use it. On Settings → Skills → Browse, search the catalog, open a skill, and click **Subscribe**. From the [catalog page](skills.html), save your instance's address once and use the **Subscribe in PeckBoard** link on any skill — it opens the Skills settings on your instance with that skill ready to subscribe. Unsubscribing removes the copy; sessions that used it lose it the next time their agent starts.

## Publishing a Skill

Only local skills are published. On the Installed tab, choose **Publish…** from a skill's menu (or open the skill and click **Publish…**), pick where it goes, and click **Publish**. The newest version is published, and the [guard](#no-proprietary-content) checks it first.

- **A team repository** — PeckBoard writes `skills/<name>/<version>/SKILL.md` (and the skill's groups) into its clone and pushes a commit. Teammates subscribed to the repo get it when they sync.
- **Peckboard Public** — one click. The public catalog is hosted at [skills.peckboard.com](https://skills.peckboard.com), and PeckBoard publishes the skill there straight away and links to it; it shows up in everyone's catalog within a minute.

On Peckboard Public, a skill's name belongs to the PeckBoard install that first published it: each install signs its publishes with its own identity key (the same key remote access uses, kept in `remote_access_identity` in the data directory), so only that install can publish new versions of the name. If another install already owns the name, rename your skill to publish it. Keep that file in your backups — an install that loses it can't publish to its names again.

Published versions never change, on Peckboard Public or in a team repository. To publish changes, edit the skill so it gets a new version, then publish again.

## Forks and Merge Requests

You can't publish to a public skill someone else owns, but you can **fork** it: make your own copy, change it, and ask its author to take your changes.

1. **Fork and edit.** On Settings → Skills → Browse, open a public skill and click **Fork and edit**. PeckBoard makes the fork on Peckboard Public and opens it in the skill editor. A fork is named `<skill>@<handle>` — for example `pr-review-checklist@alice` — and shows up on the Installed tab like any local skill. Publishing it publishes to the fork, never to the original.
2. **Sync from original.** When the original gets new versions, open your fork and click **Sync from original** to bring them in. Publish your own edits to the fork first. If the original's changes clash with yours, nothing changes and PeckBoard tells you so.
3. **Ask the author to merge.** Open your fork and click **Ask the author to merge…**, give it a title and a short description, and send it. Edits not yet on the fork are published to it first.

Your **handle** is the name your forks carry. Set it under Settings → Skills → Merge requests (2 to 32 lowercase letters, digits, or dashes); until you do, Peckboard Public gives your install one like `k-1a2b3c4d`.

The **Merge requests** tab lists **Incoming** requests (into skills you published) and **Outgoing** ones (from your forks), filtered by state: open, merged, closed, or all. A number on the tab shows how many incoming requests are open; PeckBoard checks every five minutes and sends a notification when a new one arrives. Open a request to read its changes as a diff, comment on it, and — for an incoming one — **Merge** it. Leave **Publish a new version** on to publish the merged text straight away as a patch, minor, or major version; turn it off to merge now and publish later. A request that says **Needs an update from the fork** no longer merges cleanly: its author has to sync the fork from the original and publish again. **Close request** turns it down without merging.

## History

A public skill keeps every change. Open it on the Browse tab and switch to **History** to see each change with who made it and which version it published, and compare any two versions or changes as a diff.

## Skills in a Session

Each session has its own set of skills. Open the session's menu, choose **Skills**, and pick skills and groups from the searchable list. An agent that creates a session can give it skills and groups from the start. Changes take effect the next time the session's agent starts.

Every agent sees a short **Available Skills** list (name, version, and description) in its instructions, and loads an assigned skill's full text with the `skill_read` tool when the task calls for it. Claude sessions also get the skills as native Claude Code skills, named `peckboard-skills:<name>`: PeckBoard writes them into a plugin folder for that one session and starts Claude with `--plugin-dir`, so one session's skills never show up in another.

Agents can manage skills too, through the PeckBoard MCP server:

- Skills: `skill_list`, `skill_search`, `skill_get`, `skill_create`, `skill_update`, `skill_delete`
- Groups: `skill_group_list`, `skill_group_create`, `skill_group_update`, `skill_group_delete`
- Subscriptions: `skill_subscribe`, `skill_unsubscribe`, `skill_set_version`
- Sessions: `session_set_skills`, `session_get_skills`, `skill_read`
- Sources and publishing: `skill_source_list`, `skill_source_sync`, `skill_publish`
- Team memory: `team_memory_list`, `team_memory_get`, `team_memory_add`, `team_memory_update`, `team_memory_remove`
- Forks and history on Peckboard Public: `skill_fork`, `skill_sync_fork`, `skill_history`, `skill_diff` (`skill_update` with `publish: true` publishes a new version of a skill or fork you own; a fork publishes by default)
- Merge requests: `skill_merge_request_create`, `skill_merge_request_list` (`role`: incoming or outgoing; `state`), `skill_merge_request_get`, `skill_merge_request_comment`, `skill_merge_request_merge` (`publish`, `bump` or `version`), `skill_merge_request_close`

The tools that change something on Peckboard Public follow the same rules as publishing: an admin or the user who added the skill involved, and never from a worker session.

## Team Repositories

A team repo is a private git repository with this layout — the same as the public catalog, plus a `memory/` folder:

```
skills/<name>/<version>/SKILL.md   one folder per version
groups/<slug>.json                 {"slug", "name", "description", "skills": [...]}
memory/<slug>.md                   shared team notes (frontmatter: title, updated)
index.json                         generated list of skills, versions, and groups
```

`memory/` holds short shared notes the whole team's agents can read and update — conventions, decisions, where things live — managed under Settings → Skills → Team memory or with the `team_memory_*` tools. Publishing a skill writes a new version folder and pushes a commit; notes are committed the same way, so the repository's history is the audit log.

## No Proprietary Content

The public catalog is public. Before PeckBoard publishes a skill or a team note, a guard scans it for content that must not leave your organisation — private keys, API tokens (AWS, GitHub, Anthropic, OpenAI, Slack), secret-looking values, email addresses, private IP addresses, internal hostnames (`.internal`, `.local`, `.corp`, `.lan`), absolute home-directory paths, and the values of secrets configured on your instance — and refuses, listing each finding with its line (or **Metadata** when it is in the name, description, tags, or groups). Peckboard Public runs the same scan again on every skill it receives and refuses it the same way.

The scan is a safety net, not a guarantee. Never contribute company-internal procedures, customer names, code, or documents to the public catalog; keep them in a team repo.
