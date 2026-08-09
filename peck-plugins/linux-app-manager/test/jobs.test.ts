import { describe, expect, it } from "vitest";
import { installHost } from "./hostShim";
import { LOCAL_TARGET } from "../src/targets";
import {
  buildBackgroundScript,
  buildStatusScript,
  createJob,
  currentJobFor,
  deriveJobState,
  logPathFor,
  parseStatusOutput,
  pollJob,
} from "../src/jobs";

describe("logPathFor", () => {
  it("derives a stable, per-job path under /tmp", () => {
    expect(logPathFor("j5")).toBe("/tmp/peckboard-lam-j5.log");
  });
});

describe("buildBackgroundScript", () => {
  it("wraps a command with nohup, output redirection, and an exit sentinel", () => {
    expect(buildBackgroundScript("echo hi", "/tmp/x.log")).toBe(
      "nohup sh -c 'echo hi; echo PECKBOARD_EXIT:$?' > '/tmp/x.log' 2>&1 < /dev/null & echo $!",
    );
  });

  it("safely escapes a command containing single quotes", () => {
    expect(buildBackgroundScript("echo can't", "/tmp/x.log")).toBe(
      "nohup sh -c 'echo can'\\''t; echo PECKBOARD_EXIT:$?' > '/tmp/x.log' 2>&1 < /dev/null & echo $!",
    );
  });
});

describe("buildStatusScript", () => {
  it("checks pid liveness and tails the logfile", () => {
    expect(buildStatusScript(123, "/tmp/x.log")).toBe(
      "if kill -0 123 2>/dev/null; then echo PECKBOARD_RUNNING; else echo PECKBOARD_EXITED; fi; tail -n 200 '/tmp/x.log' 2>/dev/null",
    );
  });
});

describe("parseStatusOutput", () => {
  it("reports running with no exit code", () => {
    expect(parseStatusOutput("PECKBOARD_RUNNING\ninstalling...\n")).toEqual({
      running: true,
      tail: "installing...",
      exitCode: null,
    });
  });

  it("reports a successful exit", () => {
    expect(
      parseStatusOutput("PECKBOARD_EXITED\nline1\nline2\nPECKBOARD_EXIT:0"),
    ).toEqual({
      running: false,
      tail: "line1\nline2",
      exitCode: 0,
    });
  });

  it("reports a failed exit", () => {
    expect(
      parseStatusOutput(
        "PECKBOARD_EXITED\nE: could not find package\nPECKBOARD_EXIT:100",
      ),
    ).toEqual({
      running: false,
      tail: "E: could not find package",
      exitCode: 100,
    });
  });

  it("has no exit sentinel when the process crashed without recording one", () => {
    expect(
      parseStatusOutput("PECKBOARD_EXITED\npartial output before a crash"),
    ).toEqual({
      running: false,
      tail: "partial output before a crash",
      exitCode: null,
    });
  });
});

describe("deriveJobState", () => {
  it("stays running while the process is alive", () => {
    expect(deriveJobState({ running: true, tail: "", exitCode: null })).toBe(
      "running",
    );
  });

  it("succeeds on a 0 exit code", () => {
    expect(deriveJobState({ running: false, tail: "", exitCode: 0 })).toBe(
      "succeeded",
    );
  });

  it("fails on a non-zero exit code", () => {
    expect(deriveJobState({ running: false, tail: "", exitCode: 1 })).toBe(
      "failed",
    );
  });

  it("fails when the process is gone with no exit sentinel (crash)", () => {
    expect(deriveJobState({ running: false, tail: "", exitCode: null })).toBe(
      "failed",
    );
  });
});

describe("job lifecycle (store-backed)", () => {
  it("creates a running job, then transitions to succeeded once the sentinel appears", () => {
    installHost(
      {},
      {
        execAny: () => ({
          exit_code: 0,
          stdout: "PECKBOARD_EXITED\ndone\nPECKBOARD_EXIT:0",
          stderr: "",
          stdout_truncated: false,
          stderr_truncated: false,
          timed_out: false,
        }),
      },
    );

    const job = createJob("local", "ripgrep", "install");
    expect(job.status).toBe("running");
    expect(currentJobFor("local", "ripgrep")?.id).toBe(job.id);

    const { job: polled, tail } = pollJob(LOCAL_TARGET, job);
    expect(polled.status).toBe("succeeded");
    expect(polled.exit_code).toBe(0);
    expect(tail).toBe("done");
    expect(currentJobFor("local", "ripgrep")?.status).toBe("succeeded");
  });

  it("transitions to failed when the process exits non-zero", () => {
    installHost(
      {},
      {
        execAny: () => ({
          exit_code: 0,
          stdout: "PECKBOARD_EXITED\nE: permission denied\nPECKBOARD_EXIT:1",
          stderr: "",
          stdout_truncated: false,
          stderr_truncated: false,
          timed_out: false,
        }),
      },
    );

    const job = createJob("local", "docker", "install");
    const { job: polled } = pollJob(LOCAL_TARGET, job);
    expect(polled.status).toBe("failed");
    expect(polled.exit_code).toBe(1);
  });

  it("stays running while the pid is still alive", () => {
    installHost(
      {},
      {
        execAny: () => ({
          exit_code: 0,
          stdout: "PECKBOARD_RUNNING\nstill going\n",
          stderr: "",
          stdout_truncated: false,
          stderr_truncated: false,
          timed_out: false,
        }),
      },
    );

    const job = createJob("local", "ollama", "install");
    const { job: polled } = pollJob(LOCAL_TARGET, job);
    expect(polled.status).toBe("running");
  });

  it("does not re-derive state for an already-finished job, but still refreshes the tail", () => {
    installHost(
      {},
      {
        execAny: () => ({
          exit_code: 0,
          stdout:
            "PECKBOARD_EXITED\nmore output since last poll\nPECKBOARD_EXIT:0",
          stderr: "",
          stdout_truncated: false,
          stderr_truncated: false,
          timed_out: false,
        }),
      },
    );

    const job = createJob("local", "git", "remove");
    job.status = "failed";
    const { job: polled, tail } = pollJob(LOCAL_TARGET, job);
    expect(polled.status).toBe("failed");
    expect(tail).toBe("more output since last poll");
  });
});
