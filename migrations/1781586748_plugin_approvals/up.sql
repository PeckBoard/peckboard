-- Per-plugin hook-permission approvals for WASM plugins.
--
-- A loaded WASM plugin is INERT until an operator approves the set of
-- hooks it declares: none of its hooks fire, none of its `/plugin-api`
-- routes dispatch, and none of its ui_panels surface, until there is an
-- `approved` row here whose `hooks` matches what the plugin currently
-- declares. The decision is per-plugin (covers the whole declared hook
-- set) and persists across restarts.
--
-- `hooks` is the plugin's declared hook list, sorted and newline-joined
-- (the canonical form `decide` writes and load-time compares against).
-- Binding the grant to that exact set means swapping the .wasm for a
-- build that declares MORE/other hooks no longer matches, so the plugin
-- drops back to pending and must be re-approved — an old approval can't
-- be inherited to smuggle in new hooks.
--
-- This table is core-owned. Plugins cannot reach it: the only plugin
-- self-storage path is the namespaced `plugin_settings` table, so a
-- plugin can never approve itself.
CREATE TABLE IF NOT EXISTS plugin_approvals (
    plugin_id   TEXT NOT NULL PRIMARY KEY,
    hooks       TEXT NOT NULL,
    status      TEXT NOT NULL,
    decided_at  TEXT NOT NULL
);
