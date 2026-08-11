// The dashboard page is one big HTML string, so nothing type-checks it and no
// bundler parses its inline JavaScript. These tests are the syntax gate: a
// stray brace in page.ts used to reach the browser as a blank page.

import { describe, expect, it } from "vitest";
import { PAGE } from "../src/page";
import { injectRequest } from "../src/deeplink";

function scripts(html: string): string[] {
  return [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
}

describe("the page's inline script", () => {
  it("parses as JavaScript, with and without a deep-link request", () => {
    for (const req of [
      null,
      { apps: ["python3", "pip"], from: "graphify", target: "local" },
    ]) {
      const blocks = scripts(injectRequest(PAGE, req));
      expect(blocks.length).toBeGreaterThan(0);
      for (const block of blocks) {
        expect(() => new Function(block)).not.toThrow();
      }
    }
  });

  it("carries the request token exactly once, for injectRequest to fill", () => {
    expect(PAGE.split("__APP_MANAGER_REQUEST__")).toHaveLength(2);
  });

  it("has the request bar the deep link renders into", () => {
    expect(PAGE).toContain('id="reqBar"');
  });
});
