---
title: Project Planner
parent: Plugins
nav_order: 23
---

# Project Planner

Project Planner builds a project definition by interview: a slideshow asks one pointed question per slide and writes each answer into the repo's definition files. It launches from a repo's row in the folder's repo browser, and each repo's interview runs independently.

![The Project Planner start slide: a model picker, an optional topic field, and the Begin the interview button]({{ "/assets/screenshots/plugins/project-planner.png" | relative_url }})

A dedicated temporary agent session generates each slide after the last. Every slide is exactly one question — fill-in-the-blank or multiple choice with two to five options, each carrying a one-sentence justification naming its trade-off — plus a short _why_ explaining what the question decides, and sometimes a small mermaid diagram picturing the problem. The interview establishes purpose and goal before anything else; only then does it move to users, stories, flows, architecture, technology, deployment, and monitoring. Topics too complex for one slide go into a pending-question queue the agent works through one slide at a time.

The definition is split so no question has to read all of it. `PROJECT_DEFINITION.md` at the repo root holds the purpose, the goals, and a generated index linking one file per section under `project-definition/` — users and stories, flows, architecture, technology, deployment and operations, open questions — each with a one-line summary. Every section file opens with a header saying it depends on `PROJECT_DEFINITION.md`. Each turn the agent gets only the root index, plus the one section the current answer belongs in; it reads any other section only when a question needs it. A definition written as a single file by an earlier version is split by its headings on the next start, with nothing dropped.

After every answer the agent writes exactly one requirement into that section, or amends the one the answer changed, and the slide's progress trail shows what changed. **View definition** lists every definition file — the root first, then each section, unwritten ones marked — and shows the one you pick; links in the index jump to their file. When the repo's code already answers a question, the slide proposes that answer with evidence naming where the code shows it, for one-click confirmation. A watchdog nudges a stalled generation twice, then fails the interview with an explicit message rather than leaving the slideshow hanging.

The five MCP tools (`project_planner_ask`, `project_planner_queue`, `project_planner_write_definition`, `project_planner_read_section`, `project_planner_finish`) are the private channel between the interview session and the slideshow — they refuse any caller except the folder's own planner session, and the slideshow never sees chat text.

<details markdown="1">
<summary>Hooks, permissions, and prerequisites</summary>

Hooks: `mcp.tool.invoke`, `http.request.before`, `http.request.authed`. Permissions: `provide_mcp_tools`, `data_store`, `models_read`, `session_write`, `session_dispatch`, `session_control`, `session_read`, `session_prompt_write`, `project_files_read`, `project_files_write`, `user_authority`, `contribute_sidebar`.

It needs at least one configured AI account with an available model (the highest tier is preselected — planning benefits from the strongest model), a folder containing a git repo, and write access to the repo root. Mermaid diagrams render via CDN when reachable and fall back to a styled text block offline. No settings — the model is picked per run on the start slide.

</details>
