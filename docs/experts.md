---
title: Experts
nav_order: 4
---

# Experts

An _expert_ is a long-running AI session that holds knowledge about one area and answers questions from other sessions. Workers — the agent sessions that complete cards on the board, described on [Core Concepts]({{ "/core-concepts.html" | relative_url }}) — ask experts before acting, so facts the codebase or the user has already settled do not get rediscovered or asked twice. PeckBoard has three kinds: knowledge experts, the question expert, and the PM expert.

![Experts view listing expert sessions grouped by scope, with Question, Knowledge, and PM badges]({{ "/assets/screenshots/experts.png" | relative_url }})

The Experts view lists every expert grouped by scope — global first, then per project — with a badge for its kind and a line stating which part of the codebase it covers.

## Knowledge Experts

A knowledge expert has read one part of the codebase, such as `src` or `web`, and answers questions about it. When a project's experts are set up, the codebase is split into a few areas and one expert reads each, so every expert holds a small area well instead of one holding everything poorly. A worker touching unfamiliar code asks the matching expert instead of re-reading that whole area itself:

> **Worker:** Where does the frontend keep its WebSocket state?
>
> **Web expert:** In a Zustand store at `web/src/store/ws.ts`. Components subscribe to that store rather than opening their own connections.

## The Question Expert

The question expert remembers the answers the user has given. Before a worker asks the user anything, it asks the question expert first; if a past answer covers the question, the user is never interrupted. When the user does answer a new question, that answer is fed back to the question expert, so each question only reaches the user once. There is one question expert per project and one global one for sessions outside any project.

## The PM Expert

The PM expert stores project-level decisions — direction and business logic the user has settled — so the project keeps moving the same way no matter which worker picks up a card. Workers check a planned change against recorded decisions before making it. When no recorded decision covers a question of direction, the PM expert escalates it to the user and records the outcome; workers never change a recorded decision themselves.

## How a Worker Uses Them

The pattern during a card is ask first, then act. A worker starting a card consults the knowledge expert for the area it is about to change, checks the question expert before bringing anything to the user, and checks PM decisions before any choice of direction. Consultations are asynchronous: the worker sends its question, keeps working, and the answer arrives as a message a moment later.

![Chat view of a session showing user messages, agent replies, and a collapsed tool call]({{ "/assets/screenshots/chat.png" | relative_url }})

Expert consultations appear in this same chat log as tool calls, and the expert's reply arrives as a follow-up message the agent folds into its next step.

<details markdown="1">
<summary>How experts are created and kept alive</summary>

The whole experts feature now lives in the **experts WASM plugin** (`peck-plugins/experts`); core holds no experts logic and just loads and dispatches to the plugin like any other. The behavior is unchanged for users.

Experts are ordinary sessions in the same SQLite database as everything else, tagged by the plugin's own per-session metadata (kind / area / scope) rather than a core "expert kind" column, so they survive server restarts.

Knowledge experts are created by the `spin_up_experts` tool. It partitions the project's top-level directories into size-balanced groups (adjacent directories are grouped so related topics share an expert, four groups by default), creates one expert session per group, and has each expert read its scope and write back a knowledge summary. Capture runs at most three experts at a time to bound cost.

The question and PM experts are durable sessions the plugin ensures and tracks, so they rehydrate after a restart and a repeated spin-up never clobbers what they have accumulated. PM decisions live in the plugin's own document store, not a core table.

`ask_expert` is asynchronous on both ends: the question is delivered into the expert's session, and the answer comes back to the asking session as an event on a later turn, so neither side blocks while the other thinks.

</details>
