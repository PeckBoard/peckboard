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

Any session can run reviews too. `pr_review_start` takes a branch (plus optional base, folder, model and prompt; the folder defaults to the session's own) and returns at once; when the review finishes the session gets a message with the model's suggested verdict, counts, summary and top recommended changes, or the failure reason. `pr_review_list` and `pr_review_get` read progress and the full result; `pr_review_rerun`, `pr_review_cancel` and `pr_review_delete` manage reviews; `pr_review_list_folders` lists target folders; `pr_review_get_prompt` and `pr_review_set_prompt` read and replace the global prompt; `pr_review_triage` and `pr_review_publish` triage and post (the decision is a required argument).

## Triage and Publish

Once a review is done, each finding can be **ignored** (moved to a collapsed Ignored group and left out of what gets posted) or given an **edited comment** that replaces its wording. The model's verdict is shown only as a suggestion — the **Publish review** card asks for your decision (Approve, Request changes, or Comment) with nothing pre-selected, and severity never decides it. The card pre-fills the review body from the summary, why and ticket, and previews every comment that will be posted: findings with a file and line become inline comments, the rest go into the body, each prefixed with a Conventional Comments label (`issue (blocking):`, `issue:`, `suggestion:`, `nitpick:`, `praise:`).

**Start publish session** opens a session in the review's folder that finds the branch's open PR with `gh` and posts one GitHub review with your decision, body and comments. Comments on lines outside the diff move into the body; if GitHub refuses the decision itself (you cannot approve your own PR) it reports the error rather than switching to another decision. The card and the list row show whether it is posting, posted (with the PR link), or failed (with the reason and a Retry).

Two more tools (`pr_review_step`, `pr_review_submit`) are the private channel between a review session and the page, and `pr_review_publish_result` is the publish session's: steps and provider status flow through the first, the second validates the final result and returns readable errors until it is well-formed, and the third records whether the GitHub review was posted. Those sessions can use only their own tools — never start or manage other reviews. A watchdog nudges a review run that ends without submitting twice (a publish run once), then fails it with an explicit message.

<details markdown="1">
<summary>Hooks, permissions, and prerequisites</summary>

Hooks: `mcp.tool.invoke`, `http.request.before`, `http.request.authed`, `session.agent.ended`. Permissions: `provide_mcp_tools`, `contribute_sidebar`, `data_store`, `models_read`, `session_write`, `session_read`, `session_dispatch`, `session_orchestrate`, `user_authority`.

It needs at least one thinking-capable model for the review session, and Claude, Codex and Grok accounts for the full three-reviewer fan-out. The target folder must hold a git repo containing the branch. Publishing needs `gh` installed and authenticated for the repo, and an open PR for the branch. Requires Peckboard 0.1.90 or newer.

</details>
