---
title: Pre-hatcher
parent: Plugins
nav_order: 22
---

# Pre-hatcher

Pre-hatcher offers to enrich a chat message with repository context before your main model sees it. Each message you send gets a small opt-in card — no AI is involved until you accept. Declining sends the message unchanged; accepting spawns a temporary research session on a cheap model that reads the repository with your chat transcript in view, then proposes an expanded version of your message on a second card for approval. You always see and approve the final text.

![Settings → Chat & Models with the pre-hatcher model setting, alongside the default model and caveman mode]({{ "/assets/screenshots/plugins/pre-hatcher.png" | relative_url }})

The research session runs on the provider's cheapest priced model unless you pick one in Settings → Chat & Models → Pre-hatcher Model, under a configurable library system prompt (default "fable 5") — both are core settings, not plugin-form settings. Its actions stream live into the parked message with a Cancel button, and core enforces a read-only tool allowlist on it at dispatch, so the research pass can search and read but never edit. If the request is ambiguous, the research session may ask you one clarifying question first; the answer is folded into the proposal. The delivered message carries an "enriched by the pre-hatcher" badge with the original text behind a disclosure.

Only plain text messages in chat sessions are intercepted. Worker and expert sessions, messages with attachments, and the plugin's own research sessions always pass through untouched. Its one MCP tool, `pre_hatch_result`, is the research session's hand-off back to the plugin — not something other sessions call.

<details markdown="1">
<summary>Hooks</summary>

Hooks: `session.message.before`, `session.prehatch.answer`, `session.prehatch.cancel`, `mcp.tool.invoke`. It needs at least one provider with a priced model catalog for the cheapest-model auto-pick.

</details>
