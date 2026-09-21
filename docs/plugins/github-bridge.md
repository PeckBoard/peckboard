---
title: GitHub Bridge
parent: Plugins
nav_order: 14
---

# GitHub Bridge

GitHub Bridge syncs GitHub issues with PeckBoard kanban cards: import issues as cards, sync a card's state back to its issue, link pull requests to cards, and auto-close the GitHub issue when a card reaches a terminal step. Configure it with a GitHub token that has issue and pull-request access to the target repository.

| Tool               | What it does                                             |
| ------------------ | -------------------------------------------------------- |
| `gh_import_issues` | Imports GitHub issues into a project as kanban cards     |
| `gh_sync_card`     | Syncs a card's state back to its linked GitHub issue     |
| `gh_link_pr`       | Links a pull request to a card                           |
| `gh_status`        | Reports the bridge's connection and configuration status |

<details markdown="1">
<summary>Hooks</summary>

Hooks: `mcp.tool.invoke` and `card.step.after` — the latter is what closes the linked issue when a card lands on a terminal step. Document review's own [GitHub PR import]({{ "/review.html" | relative_url }}) is separate core functionality; this plugin covers the issues-to-cards direction.

</details>
