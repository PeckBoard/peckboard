---
title: Home
nav_order: 1
---

# PeckBoard

PeckBoard is a remote control panel that spawns Claude Code agents and orchestrates multi-agent work on a kanban board. You add a card describing a task, and a worker session — a Claude Code agent — picks it up, edits files in the project's folder, and moves the card across the board until it reaches Done. The whole system is one binary you run on your own machine and open in a browser.

![Kanban board showing cards spread across Backlog, In Progress, Review, and Done columns]({{ "/assets/screenshots/board.png" | relative_url }})

[Getting Started]({{ "/getting-started.html" | relative_url }}) walks through downloading or building the binary and creating a first project. [Core Concepts]({{ "/core-concepts.html" | relative_url }}) explains how projects, cards, workers, and dependencies fit together once the server is running.

[Experts]({{ "/experts.html" | relative_url }}) covers the long-lived sessions that workers consult before acting. [Architecture]({{ "/architecture.html" | relative_url }}) describes the server, database, and MCP layer that hold everything together, and [Configuration]({{ "/configuration.html" | relative_url }}) lists the command-line flags and where data lives.
