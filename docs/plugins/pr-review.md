---
title: PR Review
parent: Plugins
nav_order: 21.5
---

# PR Review

PR Review reviews a branch with several models in parallel. Pick a branch and a folder from the **PR Reviews** sidebar page; each review runs in its own agent session, so any number run side by side.

A review session never checks out or edits your working tree. It reviews from a detached git worktree inside the repo (excluded through `.git/info/exclude` and removed when done), gathers the diff, commit log, PR body (via `gh` when available) and ticket once, then spawns three independent reviewer subagents — one Claude, one Codex, one Grok model, each the provider's most capable. Their findings are merged (same file, nearby lines, same root cause), marked as consensus or single-reviewer, re-verified in the code, and ranked. A provider with no model is skipped and recorded as a failed reviewer.

The list shows every review's status, current step (context → reviewing → merging → verifying → submitting), per-provider progress, findings by severity, and the verdict. Rows re-run, cancel, delete, or open the review session. A review's page shows the summary, why the change is needed, the linked ticket, then the recommended changes grouped by severity — blocker, major, minor, nit — each with file and line, rationale, a suggested fix, the reviewers who raised it, and a confidence. Praise follows, and refuted findings sit in a collapsed **Dropped findings** list.

The default global prompt is distilled from Anthropic's code-review and security-review prompts, Qodo PR-Agent, Google's engineering practices, and Conventional Comments: understand the change before judging it, report only issues the diff introduces with a concrete trigger, verify each claim in the code, cap nits, and give specific praise. Edit it from **Global prompt**; a review's own custom prompt is appended to it for that review only.

The two MCP tools (`pr_review_step`, `pr_review_submit`) are the private channel between a review session and the page: steps and provider status flow through the first, and the second validates the final result and returns readable errors until it is well-formed. A watchdog nudges a run that ends without submitting twice, then fails the review with an explicit message.

<details markdown="1">
<summary>Hooks, permissions, and prerequisites</summary>

Hooks: `mcp.tool.invoke`, `http.request.before`, `http.request.authed`, `session.agent.ended`. Permissions: `provide_mcp_tools`, `contribute_sidebar`, `data_store`, `models_read`, `session_write`, `session_read`, `session_dispatch`, `session_orchestrate`, `user_authority`.

It needs at least one thinking-capable model for the review session, and Claude, Codex and Grok accounts for the full three-reviewer fan-out. The target folder must hold a git repo containing the branch. Requires Peckboard 0.1.90 or newer.

</details>
