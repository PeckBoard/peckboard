// AI-session install flow: model-picker filtering (a non-thinking model is
// never offered), session-request/prompt construction, slim-event folding,
// job state transitions — including the abandoned-session case, which must
// land failed and never record a bogus empty delta as success — and the
// default account+model persistence.

import { beforeEach, describe, expect, it } from "vitest";
import { Store, installHost } from "./hostShim";

import { findApp } from "../src/catalog";
import {
  INSTALL_FOLDER_NAME,
  INSTALL_FOLDER_PATH,
  activityFromJob,
  buildInstallPrompt,
  buildInstallSessionRequest,
  buildSessionName,
  deriveSessionOutcome,
  describeEvent,
  foldSessionEvents,
  getDefaultInstallModel,
  pollSessionJob,
  requireOfferedModel,
  setDefaultInstallModel,
  startSessionInstall,
  thinkingModelChoices,
} from "../src/installSession";
import { JobRecord, getJob } from "../src/jobs";
import { LOCAL_TARGET } from "../src/targets";

const MODELS = [
  {
    id: "mock:plan-review",
    display_name: "Mock: plan review (thinking)",
    provider: "mock",
    account_id: null,
    thinking: true,
    tier: 3,
  },
  {
    id: "claude:claude-fable-5@acc_1",
    display_name: "Fable 5",
    provider: "claude",
    account_id: "acc_1",
    thinking: true,
    tier: 4,
  },
];

describe("model picker filtering", () => {
  it("never offers a non-thinking model, however it is labelled", () => {
    const raw = [
      ...MODELS,
      // Host must already filter these out; if one ever leaks through, the
      // strict plugin-side check still drops it.
      {
        id: "mock:happy-path",
        display_name: "Mock: happy path",
        thinking: false,
      },
      { id: "claude:claude-haiku", display_name: "Haiku thinking edition" },
      { id: "", thinking: true },
      null,
      "not-an-object",
    ];
    const out = thinkingModelChoices(raw);
    expect(out.map((m) => m.id)).toEqual([
      "mock:plan-review",
      "claude:claude-fable-5@acc_1",
    ]);
    expect(out.every((m) => m.thinking)).toBe(true);
  });

  it("refuses a model id that is not in the offered set", () => {
    const models = thinkingModelChoices(MODELS);
    expect(() => requireOfferedModel(models, "mock:happy-path")).toThrow(
      /not in the selectable catalog/,
    );
    expect(requireOfferedModel(models, "mock:plan-review").id).toBe(
      "mock:plan-review",
    );
  });
});

describe("session request + prompt construction", () => {
  it("builds a temp-session request on the picked model, with the shared install folder under authority", () => {
    const req = buildInstallSessionRequest(
      "Git",
      "claude:claude-fable-5@acc_1",
      true,
    );
    expect(req).toEqual({
      name: "Install Git",
      model: "claude:claude-fable-5@acc_1",
      is_temp: true,
      folder_path: INSTALL_FOLDER_PATH,
      folder_name: INSTALL_FOLDER_NAME,
    });
  });

  it("omits the folder override without authority (an MCP call stays in its caller's folder)", () => {
    const req = buildInstallSessionRequest("Git", "mock:plan-review", false);
    expect(req).toEqual({
      name: "Install Git",
      model: "mock:plan-review",
      is_temp: true,
    });
    expect(buildSessionName("Ollama")).toBe("Install Ollama");
  });

  it("instructs the agent to use sudo -A (askpass dialog) and to verify the install", () => {
    const git = findApp("git")!;
    const prompt = buildInstallPrompt(git, "sudo -A apt-get install -y git");
    expect(prompt).toContain("sudo -A <cmd>");
    expect(prompt).toContain("masked dialog");
    expect(prompt).toContain("Plain \`sudo\` will fail here (no TTY)");
    expect(prompt).toContain("Never put the password on a command line");
    expect(prompt).toContain("sudo -A apt-get install -y git");
    expect(prompt).toContain("report the installed version");
  });

  it("tells a manually added app's session to research it and stay on official sources", () => {
    const prompt = buildInstallPrompt(
      {
        id: "zellij",
        name: "Zellij",
        version: "'zellij' --version",
        custom: true,
        homepage: "https://zellij.dev",
        description: "terminal multiplexer",
      },
      null,
    );
    expect(prompt).toContain("search the web");
    expect(prompt).toContain("OFFICIAL SOURCES ONLY");
    expect(prompt).toContain("Never a third-party mirror");
    expect(prompt).toContain("checksum or signature");
    expect(prompt).toContain("STOP and say so");
    expect(prompt).toContain("ask in this session");
    // What the person supplied is context to verify, never taken as true.
    expect(prompt).toContain("https://zellij.dev");
    expect(prompt).toContain("confirm it really is the project's own");
    expect(prompt).toContain("terminal multiplexer");
    // And the standing rules still apply.
    expect(prompt).toContain("sudo -A <cmd>");
  });

  it("frames a person's own install command as a suggestion to check, not an instruction", () => {
    const prompt = buildInstallPrompt(
      {
        id: "zellij",
        name: "Zellij",
        version: "'zellij' --version",
        custom: true,
        description: "",
      },
      "cargo install zellij",
    );
    expect(prompt).toContain("suggestion to check, not an instruction");
    expect(prompt).toContain("cargo install zellij");
  });

  it("leaves a catalog app's prompt free of the research rules", () => {
    const prompt = buildInstallPrompt(
      findApp("git")!,
      "sudo -A apt-get install -y git",
    );
    expect(prompt).not.toContain("search the web");
    expect(prompt).not.toContain("OFFICIAL SOURCES ONLY");
  });
});

describe("slim-event folding", () => {
  it("maps kinds to honest tool-level lines and skips payload-less noise", () => {
    expect(describeEvent({ kind: "agent-tool-start", name: "Bash" })).toBe(
      "Tool: Bash",
    );
    expect(describeEvent({ kind: "agent-tool-end", name: null })).toBeNull();
    expect(describeEvent({ kind: "agent-usage", name: null })).toBeNull();
    expect(describeEvent({ kind: "question", name: null })).toMatch(
      /open the session/,
    );
  });

  it("latches ended on agent-end and clears a pending question on any later event", () => {
    const start = {
      last_seq: 0,
      activity: [],
      events_total: 0,
      question_open: false,
      ended: false,
    };
    const asked = foldSessionEvents(start, [
      { seq: 1, kind: "agent-start", name: null },
      { seq: 2, kind: "question", name: null },
    ]);
    expect(asked.question_open).toBe(true);
    expect(asked.ended).toBe(false);
    expect(asked.last_seq).toBe(2);

    const done = foldSessionEvents(asked, [
      { seq: 3, kind: "agent-tool-start", name: "Bash" },
      { seq: 4, kind: "agent-end", name: null },
    ]);
    expect(done.question_open).toBe(false);
    expect(done.ended).toBe(true);
    expect(done.activity).toContain("Tool: Bash");
    expect(done.events_total).toBe(4);
  });
});

describe("outcome derivation", () => {
  it("succeeds only when the run ended AND the detect probe finds the app", () => {
    expect(
      deriveSessionOutcome({
        appName: "Git",
        ended: true,
        sessionGone: false,
        probeInstalled: true,
      }).status,
    ).toBe("succeeded");
    expect(
      deriveSessionOutcome({
        appName: "Git",
        ended: true,
        sessionGone: false,
        probeInstalled: false,
      }).status,
    ).toBe("failed");
    expect(
      deriveSessionOutcome({
        appName: "Git",
        ended: false,
        sessionGone: false,
        probeInstalled: false,
      }).status,
    ).toBe("running");
  });

  it("marks an abandoned session failed with an explicit unknown note", () => {
    const out = deriveSessionOutcome({
      appName: "Git",
      ended: false,
      sessionGone: true,
      probeInstalled: true, // even a positive probe must not flip an unended run to success
    });
    expect(out.status).toBe("failed");
    expect(out.message).toMatch(/ended before completing/);
    expect(out.message).toMatch(/unknown/);
  });
});

describe("startSessionInstall + default persistence", () => {
  let store: Store;
  let execs: string[];
  let created: any[];
  let dispatched: any[];

  beforeEach(() => {
    store = {};
    execs = [];
    created = [];
    dispatched = [];
    installHost(store, {
      execAny: (input) => {
        execs.push(input.args[1]);
        return {
          exit_code: 0,
          stdout: "",
          stderr: "",
          stdout_truncated: false,
          stderr_truncated: false,
          timed_out: false,
        };
      },
      listModels: () => ({ models: MODELS }),
      createSession: (input) => {
        created.push(input);
        return { session: { id: "sess-1" } };
      },
      dispatchCapture: (input) => {
        dispatched.push(input);
        return { ok: true };
      },
      callerScope: () => ({ folder_id: null, authority: true }),
    });
  });

  it("brackets, creates the temp session, dispatches the sudo -A prompt, and persists the default", () => {
    const git = findApp("git")!;
    const res = startSessionInstall(
      LOCAL_TARGET,
      git,
      "mock:plan-review",
      "apt",
    );

    expect(res.status).toBe("running");
    expect(res.session_id).toBe("sess-1");

    // BEFORE snapshot ran on the target before the session dispatch.
    expect(
      execs.some((s) => s.includes("dpkg-query") && s.includes(".before.pkgs")),
    ).toBe(true);

    // Temp session on the picked model, in the shared install folder.
    expect(created).toHaveLength(1);
    expect(created[0]).toMatchObject({
      name: "Install Git",
      model: "mock:plan-review",
      is_temp: true,
      folder_path: INSTALL_FOLDER_PATH,
    });

    // The prompt carries the sudo -A askpass rule.
    expect(dispatched).toHaveLength(1);
    expect(dispatched[0].session_id).toBe("sess-1");
    expect(dispatched[0].prompt).toContain("sudo -A");

    // Chosen model becomes the default for next time.
    expect(getDefaultInstallModel()).toBe("mock:plan-review");

    const job = getJob(res.job_id)!;
    expect(job.kind).toBe("session");
    expect(job.session_id).toBe("sess-1");
    expect(job.pm).toBe("apt");
  });

  it("refuses a model outside the offered set before touching anything", () => {
    const git = findApp("git")!;
    expect(() =>
      startSessionInstall(LOCAL_TARGET, git, "mock:happy-path", "apt"),
    ).toThrow(/not in the selectable catalog/);
    expect(created).toHaveLength(0);
    expect(dispatched).toHaveLength(0);
  });

  it("round-trips the stored default", () => {
    expect(getDefaultInstallModel()).toBeNull();
    setDefaultInstallModel("claude:claude-fable-5@acc_1");
    expect(getDefaultInstallModel()).toBe("claude:claude-fable-5@acc_1");
  });
});

describe("pollSessionJob state transitions", () => {
  function sessionJob(over: Partial<JobRecord> = {}): JobRecord {
    return {
      id: "j1",
      target_id: "local",
      app_id: "git",
      action: "install",
      pid: 0,
      logfile: "/tmp/peckboard-lam-j1.log",
      status: "running",
      kind: "session",
      session_id: "sess-1",
      pm: "apt",
      method: "apt",
      last_seq: 0,
      ...over,
    };
  }

  it("folds new events and stays running while the session works", () => {
    const store: Store = {};
    installHost(store, {
      sessionEvents: () => ({
        events: [
          { seq: 1, kind: "agent-start", name: null },
          { seq: 2, kind: "agent-tool-start", name: "Bash" },
        ],
        latest_seq: 2,
      }),
      listSessionsBrief: () => ({ sessions: [{ session_id: "sess-1" }] }),
    });
    const { job } = pollSessionJob(LOCAL_TARGET, sessionJob());
    expect(job.status).toBe("running");
    expect(job.last_seq).toBe(2);
    expect(job.activity).toContain("Tool: Bash");
    // Persisted for the next poll.
    expect((store.jobs["j1"] as JobRecord).last_seq).toBe(2);
  });

  it("on agent-end takes the AFTER snapshot, probes, and succeeds when detected", () => {
    const store: Store = {};
    const execs: string[] = [];
    installHost(store, {
      execAny: (input) => {
        execs.push(input.args[1]);
        return {
          exit_code: 0, // detect probe succeeds → installed
          stdout: "",
          stderr: "",
          stdout_truncated: false,
          stderr_truncated: false,
          timed_out: false,
        };
      },
      sessionEvents: (input) =>
        input.after_seq === 0
          ? {
              events: [{ seq: 1, kind: "agent-end", name: null }],
              latest_seq: 1,
            }
          : { events: [], latest_seq: null },
      listSessionsBrief: () => ({ sessions: [{ session_id: "sess-1" }] }),
    });
    const { job } = pollSessionJob(LOCAL_TARGET, sessionJob());
    expect(job.status).toBe("succeeded");
    expect(job.message).toMatch(/now detected/);
    expect(execs.some((s) => s.includes(".after.pkgs"))).toBe(true);
  });

  it("fails when the run ended but the app is not detected", () => {
    const store: Store = {};
    installHost(store, {
      execAny: (input) => ({
        // after-snapshot writes fine; the detect probe exits non-zero
        exit_code: String(input.args[1]).includes(".after.pkgs") ? 0 : 1,
        stdout: "",
        stderr: "",
        stdout_truncated: false,
        stderr_truncated: false,
        timed_out: false,
      }),
      sessionEvents: () => ({
        events: [{ seq: 1, kind: "agent-end", name: null }],
        latest_seq: 1,
      }),
      listSessionsBrief: () => ({ sessions: [{ session_id: "sess-1" }] }),
    });
    const { job } = pollSessionJob(LOCAL_TARGET, sessionJob());
    expect(job.status).toBe("failed");
    expect(job.message).toMatch(/not detected/);
  });

  it("abandoned session (deleted before completing) → failed, snapshots cleaned, no success", () => {
    const store: Store = {};
    const execs: string[] = [];
    installHost(store, {
      execAny: (input) => {
        execs.push(input.args[1]);
        return {
          exit_code: 0,
          stdout: "",
          stderr: "",
          stdout_truncated: false,
          stderr_truncated: false,
          timed_out: false,
        };
      },
      // The temp session is gone: its events read as empty and it is absent
      // from the brief listing.
      sessionEvents: () => ({ events: [], latest_seq: null }),
      listSessionsBrief: () => ({ sessions: [] }),
    });
    const { job } = pollSessionJob(LOCAL_TARGET, sessionJob());
    expect(job.status).toBe("failed");
    expect(job.message).toMatch(/ended before completing/);
    // The orphaned bracket files are removed, and no AFTER snapshot is taken
    // (nothing may later read as a legitimate empty delta).
    expect(execs.some((s) => s.startsWith("rm -f"))).toBe(true);
    expect(execs.some((s) => s.includes("dpkg-query"))).toBe(false);
    expect((store.jobs["j1"] as JobRecord).status).toBe("failed");
  });

  it("a settled job is left untouched", () => {
    installHost({}, {});
    const done = sessionJob({ status: "succeeded" });
    const { job } = pollSessionJob(LOCAL_TARGET, done);
    expect(job).toBe(done);
  });
});
