---
title: OpenSearch
parent: Plugins
nav_order: 20
---

# OpenSearch

The OpenSearch plugin lets sessions query and manage an OpenSearch cluster — or any Elasticsearch-compatible cluster — over its REST API directly, with no separate MCP server process to run. Configure the cluster URL and credentials with `opensearch_configure` from a session or in the plugin's settings.

| Tool                    | What it does                                                         |
| ----------------------- | -------------------------------------------------------------------- |
| `opensearch_configure`  | Stores the cluster URL and credentials, verifying the connection     |
| `opensearch_status`     | Pings the cluster and reports name, version, and health              |
| `opensearch_indices`    | Lists indices with health, doc counts, shards, and disk size         |
| `opensearch_search`     | Searches an index with query DSL or a Lucene `q` string              |
| `opensearch_get_doc`    | Fetches one document by id                                           |
| `opensearch_index_doc`  | Stores a JSON document, creating the index if needed                 |
| `opensearch_delete_doc` | Deletes one document by id                                           |
| `opensearch_request`    | Escape hatch: any REST endpoint — mappings, `_bulk`, `_cat`, reindex |

<details markdown="1">
<summary>Permissions, and a naming note</summary>

Hooks: `mcp.tool.invoke`. Permissions: `provide_mcp_tools`, `http_request` — outbound HTTP including private and LAN targets, which reaching a self-hosted cluster requires.

The registry also carries a separate community **OpenSearch MCP server** entry (the OpenSearch Project's `opensearch-mcp-server-py`, run via `uvx` with `OPENSEARCH_URL` and credential environment variables) in the [MCP Server Catalog]({{ "/mcp-servers.html" | relative_url }}). Same name, different thing: this page describes the first-party WASM plugin, which needs no Python runtime.

</details>
