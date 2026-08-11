// The session that fills a manually added app's blanks in. What matters here
// is what it does NOT do: it never installs, it never claims a result the
// record doesn't actually hold, and a save is never blocked by it.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { installHost } from "./hostShim";
import {
  applyResearchDetails,
  buildCustomApp,
  getCustomApp,
  putCustomApp,
} from "../src/customApps";
import {
  DETAILS_TOOL,
  buildResearchPrompt,
  buildResearchSessionRequest,
  deriveResearchOutcome,
  maybeStartResearch,
  pollResearch,
  startResearch,
} from "../src/researchSession";

const MODELS = {
  models: [
    {
      id: "claude:opus@acct",
      display_name: "Opus",
      provider: "claude",
      account_id: "acct",
      thinking: true,
      tier: 3,
    },
  ],
};

function rec(over: any = {}) {
  return { ...buildCustomApp({ name: "Zellij" }), ...over };
}

describe("the research prompt", () => {
  beforeEach(() => installHost({}));

  it("names the blanks, forbids installing, and ends at the reporting tool", () => {
    const p = buildResearchPrompt(rec());
    expect(p).toContain("DO NOT INSTALL ANYTHING");
    expect(p).toContain(DETAILS_TOOL);
    expect(p).toContain("install command");
    expect(p).toContain("OFFICIAL SOURCES ONLY");
    expect(p).toContain("SUGGESTIONS");
  });

  it("hands over what the person wrote as a claim, not as truth", () => {
    const p = buildResearchPrompt(
      rec({ notes: "terminal multiplexer", homepage: "https://zellij.dev" }),
    );
    expect(p).toContain("to be verified");
    expect(p).toContain("confirm it really is the project's own");
  });

  it("tells the agent not to propose a command the person already typed", () => {
    const p = buildResearchPrompt(
      rec({ install_command: "cargo install zellij" }),
    );
    expect(p).toContain("do not propose one: cargo install zellij");
  });

  it("puts the session in the shared folder only for an authenticated request", () => {
    expect(buildResearchSessionRequest("Zellij", "m", true)).toMatchObject({
      name: "Research Zellij",
      is_temp: true,
      folder_path: "~/peckboard-installs/app-manager",
    });
    expect(
      buildResearchSessionRequest("Zellij", "m", false).folder_path,
    ).toBeUndefined();
  });
});

describe("what a finished run is reported as", () => {
  it("reports only what the record actually holds", () => {
    expect(
      deriveResearchOutcome({
        appName: "Zellij",
        ended: true,
        sessionGone: false,
        applied: [],
        suggestedCount: 0,
      }),
    ).toEqual({
      status: "done",
      message:
        "The research session finished without recording any details for Zellij.",
    });

    const both = deriveResearchOutcome({
      appName: "Zellij",
      ended: true,
      sessionGone: false,
      applied: ["notes"],
      suggestedCount: 1,
    });
    expect(both.status).toBe("done");
    expect(both.message).toContain("Filled in: notes.");
    expect(both.message).toContain("1 command is waiting");
  });

  it("calls a vanished session a failure that changed nothing", () => {
    const out = deriveResearchOutcome({
      appName: "Zellij",
      ended: false,
      sessionGone: true,
      applied: [],
      suggestedCount: 0,
    });
    expect(out.status).toBe("failed");
    expect(out.message).toContain("Nothing was changed");
  });

  it("stays running while the session is alive", () => {
    expect(
      deriveResearchOutcome({
        appName: "Zellij",
        ended: false,
        sessionGone: false,
        applied: [],
        suggestedCount: 0,
      }).status,
    ).toBe("running");
  });
});

describe("starting a run", () => {
  it("creates a temp session, dispatches the prompt and marks the record running", () => {
    const createSession = vi.fn(() => ({ session: { id: "s1" } }));
    const dispatchCapture = vi.fn(() => ({ ok: true }));
    installHost(
      {},
      { listModels: () => MODELS, createSession, dispatchCapture },
    );

    const started = startResearch(rec(), "claude:opus@acct");
    expect(started.research).toMatchObject({
      status: "running",
      session_id: "s1",
      model: "claude:opus@acct",
    });
    expect(createSession.mock.calls[0][0]).toMatchObject({ is_temp: true });
    expect(dispatchCapture.mock.calls[0][0].prompt).toContain(DETAILS_TOOL);
    expect(getCustomApp("zellij")?.research?.status).toBe("running");
  });

  it("refuses a model the catalog doesn't offer, before creating anything", () => {
    const createSession = vi.fn();
    installHost({}, { listModels: () => MODELS, createSession });
    expect(() => startResearch(rec(), "made:up")).toThrow(/selectable catalog/);
    expect(createSession).not.toHaveBeenCalled();
  });

  it("records a failed run when the prompt can't be dispatched", () => {
    installHost(
      {},
      {
        listModels: () => MODELS,
        createSession: () => ({ session: { id: "s1" } }),
        dispatchCapture: () => ({ error: "no route" }),
      },
    );
    expect(() => startResearch(rec(), "claude:opus@acct")).toThrow(
      /could not dispatch/,
    );
    expect(getCustomApp("zellij")?.research?.status).toBe("failed");
  });
});

describe("the save path never depends on it", () => {
  it("says why nothing started when no model has been chosen", () => {
    const createSession = vi.fn();
    installHost({}, { listModels: () => MODELS, createSession });
    const out = maybeStartResearch(rec());
    expect(out.rec.research).toBeUndefined();
    expect(out.note).toContain("No model has been chosen yet");
    expect(createSession).not.toHaveBeenCalled();
  });

  it("reports a failure to start as a note, not as a thrown save", () => {
    const store = installHost(
      {},
      {
        listModels: () => MODELS,
        createSession: () => ({ error: "session service is down" }),
      },
    );
    store.settings = { install_model: "claude:opus@acct" };
    const out = maybeStartResearch(rec());
    expect(out.rec.research).toBeUndefined();
    expect(out.note).toContain("could not be looked up");
  });

  it("does nothing for an app that has no blanks", () => {
    const createSession = vi.fn();
    const store = installHost({}, { listModels: () => MODELS, createSession });
    store.settings = { install_model: "claude:opus@acct" };
    const full = buildCustomApp({
      name: "Zellij",
      binary: "zellij",
      notes: "terminal multiplexer",
      homepage: "https://zellij.dev",
      install_command: "cargo install zellij",
      remove_command: "rm -f ~/.cargo/bin/zellij",
    });
    const out = maybeStartResearch(full);
    expect(out.note).toBeNull();
    expect(createSession).not.toHaveBeenCalled();
  });
});

describe("polling a run to a close", () => {
  it("settles on agent-end and reports what the tool actually wrote", () => {
    const store = installHost(
      {},
      {
        sessionEvents: () => ({
          events: [
            { seq: 1, kind: "agent-start", name: null },
            { seq: 2, kind: "agent-tool-start", name: "WebSearch" },
            { seq: 3, kind: "agent-end", name: null },
          ],
          latest_seq: 3,
        }),
        listSessionsBrief: () => ({ sessions: [{ session_id: "s1" }] }),
      },
    );
    const started = {
      ...rec(),
      research: {
        status: "running" as const,
        session_id: "s1",
        started_at: "2026-01-01T00:00:00Z",
        last_seq: 0,
      },
    };
    putCustomApp(started);
    // The findings land by MCP tool call while the run is in flight.
    putCustomApp(
      applyResearchDetails(started, {
        notes: "terminal multiplexer",
        install_command: "cargo install zellij",
      }).rec,
    );

    const settled = pollResearch(started);
    expect(settled.research?.status).toBe("done");
    expect(settled.research?.message).toContain("Filled in: notes.");
    expect(settled.research?.message).toContain("1 command is waiting");
    expect(settled.notes).toBe("terminal multiplexer");
    expect(store.custom_apps.zellij.research.status).toBe("done");
  });

  it("fails a run whose session vanished before it ended", () => {
    installHost(
      {},
      {
        sessionEvents: () => ({ events: [], latest_seq: null }),
        listSessionsBrief: () => ({ sessions: [] }),
      },
    );
    const started = {
      ...rec(),
      research: {
        status: "running" as const,
        session_id: "s1",
        started_at: "2026-01-01T00:00:00Z",
      },
    };
    putCustomApp(started);
    const settled = pollResearch(started);
    expect(settled.research?.status).toBe("failed");
    expect(settled.research?.message).toContain("Nothing was changed");
  });

  it("leaves a record with no running session alone", () => {
    const sessionEvents = vi.fn();
    installHost({}, { sessionEvents });
    const plain = rec();
    putCustomApp(plain);
    expect(pollResearch(plain).research).toBeUndefined();
    expect(sessionEvents).not.toHaveBeenCalled();
  });
});
