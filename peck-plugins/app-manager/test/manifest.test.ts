// The manifest is what core validates on load, and nothing else in this
// plugin imports it — so a typo here only shows up as a plugin that refuses
// to load. These tests are the guard: it parses, and every route the page
// actually calls is declared.

import { describe, expect, it } from "vitest";
import { manifestJson } from "../src/manifest";

const API = "/api/plugin-ui/app-manager";

describe("manifestJson", () => {
  const m = JSON.parse(manifestJson());

  it("declares every ui route the dashboard calls, including manual-app CRUD", () => {
    for (const route of [
      `GET ${API}/apps`,
      `GET ${API}/apps-custom`,
      `POST ${API}/apps-custom`,
      `POST ${API}/apps-custom-remove`,
      `POST ${API}/apps-custom-research`,
      `POST ${API}/apps-custom-suggestion`,
      `POST ${API}/remove`,
    ]) {
      expect(m.ui_routes).toContain(route);
    }
  });

  it("names its tools and asks for no permission beyond the session ones it already had", () => {
    expect(m.mcp_tools.map((t: any) => t.name)).toEqual([
      "app_targets",
      "app_list",
      "app_status",
      "app_install",
      "app_remove",
      "app_deps",
      "app_record_details",
    ]);
    // The detail-filling tool must say what it will not do: an agent's
    // command is a suggestion, not something the plugin arms.
    const details = m.mcp_tools.find(
      (t: any) => t.name === "app_record_details",
    );
    expect(details.description).toContain("SUGGESTION");
    expect(details.input_schema.required).toEqual(["app"]);
    // Web search happens inside the install session, using the agent's own
    // tools — the plugin gains no new permission for it.
    expect(m.permissions).not.toContain("web");
    expect(m.permissions).toContain("session_write");
  });

  it("says plainly that a manually added app's command is user-authored", () => {
    expect(m.description).toContain("ADDED BY HAND");
    expect(m.description).toContain("official sources");
    expect(m.description).toContain("user-authored shell");
  });
});
