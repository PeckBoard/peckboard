---
title: Notifier
parent: Plugins
nav_order: 18
---

# Notifier

Notifier forwards PeckBoard lifecycle events to channels you already watch: [ntfy](https://ntfy.sh/), Telegram, Discord, and generic webhooks. It is purely reactive — no page, no tools — configure a channel in its settings and the events start arriving.

Five events are forwarded: a card reaching done, an agent crashing, a worker blocking on something it cannot resolve, a project pausing, and a question waiting for your answer. Each maps to a hook the plugin listens on (`card.step.after`, `session.agent.ended`, `worker.blocked`, `project.paused`, `question.pending`).

Per channel you supply the usual coordinates — an ntfy server and topic, a Telegram bot token and chat id, a Discord webhook URL, or any webhook endpoint. PeckBoard also has built-in [browser push notifications]({{ "/features.html" | relative_url }}); Notifier is for the channels that reach you when no browser is open.
