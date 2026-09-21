---
title: Diff Viewer
parent: Plugins
nav_order: 13
---

# Diff Viewer

Diff Viewer adds a page, reachable from every project and session's menu, that shows a side-by-side diff of every file differing from `origin/main` — modified files, new files, and images — for any git repo in the working folder. Files are editable in place, so a review that spots a typo can fix it with Save rather than a round-trip through an agent.

![The Diff Viewer: a repo's changed files listed on the left, one file open side by side against origin/main]({{ "/assets/screenshots/plugins/diff-viewer.png" | relative_url }})

A repo picker lists every git work tree under the folder, including repos nested inside subdirectories. Edits are written into the selected repo only, and a repo without an `origin/main` ref is reported as having no comparison base rather than shown as an empty diff.

<details markdown="1">
<summary>Hooks</summary>

Hooks: `http.request.before`, `http.request.authed`. No MCP tools and no settings — the page is the whole feature.

</details>
