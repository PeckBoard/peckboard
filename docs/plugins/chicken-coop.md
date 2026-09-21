---
title: Chicken Coop
parent: Plugins
nav_order: 12
---

# Chicken Coop

Chicken Coop draws every live session as a bird in a 3D chicken run: a hen per active card — out of the coop while working, pecking on tool activity, nesting during testing and review, walking home when the card lands — a rooster for chat sessions, a barred hen for repeating tasks, a bantam for temporary sessions, and chicks that follow their parent bird for subagents. A blocked card shows a marked hen.

![The Chicken Coop: fenced pens per project with birds mid-peck, the daily stats board, and the scatter-feed control]({{ "/assets/screenshots/plugins/chicken-coop.png" | relative_url }})

Each project gets a fenced pen, done cards lay eggs onto a daily stats board, and the run keeps a real-clock day and night cycle with optional procedural sound. Hover a bird for its name tag, click it for its session details, scatter feed to gather the flock, and switch camera modes to follow the action. Everything is three.js with no external assets, and rendering pauses when the tab is hidden.

<details markdown="1">
<summary>Hooks</summary>

Hooks: `http.request.before`, `http.request.authed`. It reads a per-session brief (kind, phase, activity) from core and touches nothing else — no MCP tools, no settings.

</details>
