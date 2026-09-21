---
title: Public API
parent: Plugins
nav_order: 10
---

# Public API

The Public API plugin exposes an API-key-authenticated REST surface under `/plugin-api/v1/` so external clients — a webhook, a script, another tool — can read projects and cards and create cards without a PeckBoard login. Keys are managed from an **API Keys** page in the user menu; each secret is shown exactly once at creation.

![The API Keys panel: existing keys with masked values and scopes, and the create form]({{ "/assets/screenshots/plugins/api-keys.png" | relative_url }})

Every request authenticates with `Authorization: Bearer <key>`. Keys carry a scope — `read`, `write`, or `admin` — and an admin key can manage keys over the API itself (`GET/POST /plugin-api/v1/keys`, `DELETE /plugin-api/v1/keys/:id`). The data routes cover `GET /plugin-api/v1/projects`, `GET /plugin-api/v1/cards`, `GET /plugin-api/v1/cards/:id`, and card creation; a revoked key fails immediately.

One security note worth knowing: the `/plugin-api/*` prefix sits outside PeckBoard's own login and CSRF protection by design — the plugin's key check is the only gate on that surface. Treat keys like passwords, scope them as narrowly as the caller allows, and revoke keys you no longer use.

<details markdown="1">
<summary>Hooks</summary>

The plugin serves the whole surface through the `http.request.before` hook and contributes no MCP tools — it is for callers _outside_ PeckBoard; sessions already have the full toolset.

</details>
