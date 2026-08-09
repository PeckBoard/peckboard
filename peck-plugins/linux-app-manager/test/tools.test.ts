import { beforeEach, describe, expect, it, vi } from "vitest";
import { installHost } from "./hostShim";
import { dispatch } from "../src/lib";
import { appStatus } from "../src/tools";
import { buildRecord, putTarget } from "../src/targets";

function parse(json: string): any {
  return JSON.parse(json);
}

const OK_EXEC = {
  exit_code: 0,
  stdout: "",
  stderr: "",
  stdout_truncated: false,
  stderr_truncated: false,
  timed_out: false,
};

describe("dispatch: hook routing", () => {
  beforeEach(() => installHost({}));

  it("skips unknown hooks", () => {
    expect(parse(dispatch("some.other.hook", {}))).toEqual({ verdict: "skip" });
  });

  it("cancels a malformed invoke payload", () => {
    expect(parse(dispatch("mcp.tool.invoke", null)).verdict).toBe("cancel");
    expect(parse(dispatch("mcp.tool.invoke", "nope")).verdict).toBe("cancel");
  });

  it("cancels an invoke for an unknown tool", () => {
    const v = parse(
      dispatch("mcp.tool.invoke", { tool: "not_a_tool", arguments: {} }),
    );
    expect(v.verdict).toBe("cancel");
    expect(v.reason).toMatch(/does not provide/);
  });

  it("allows app_targets and returns the local target", () => {
    const v = parse(
      dispatch("mcp.tool.invoke", { tool: "app_targets", arguments: {} }),
    );
    expect(v.verdict).toBe("allow");
    expect(v.payload.targets).toEqual([
      { id: "local", kind: "local", label: "Local (this host)" },
    ]);
  });
});

describe("catalog + target validation happens before any exec call", () => {
  it("app_install rejects an unknown app without executing anything", () => {
    const execAny = vi.fn();
    installHost({}, { execAny });
    const v = parse(
      dispatch("mcp.tool.invoke", {
        tool: "app_install",
        arguments: { app: "not-real", target: "local" },
      }),
    );
    expect(v.verdict).toBe("allow");
    expect(v.payload.error).toMatch(/unknown app/);
    expect(execAny).not.toHaveBeenCalled();
  });

  it("app_install rejects an unknown target without executing anything", () => {
    const execAny = vi.fn();
    installHost({}, { execAny });
    const v = parse(
      dispatch("mcp.tool.invoke", {
        tool: "app_install",
        arguments: { app: "git", target: "nope" },
      }),
    );
    expect(v.payload.error).toMatch(/unknown target/);
    expect(execAny).not.toHaveBeenCalled();
  });

  it("app_remove rejects an unknown app without executing anything", () => {
    const execAny = vi.fn();
    installHost({}, { execAny });
    const v = parse(
      dispatch("mcp.tool.invoke", {
        tool: "app_remove",
        arguments: { app: "not-real", target: "local" },
      }),
    );
    expect(v.payload.error).toMatch(/unknown app/);
    expect(execAny).not.toHaveBeenCalled();
  });
});

describe("target selection: local vs remote go through different host functions", () => {
  it("routes a local target through peckboard_exec_any", () => {
    const execAny = vi.fn(() => ({
      ...OK_EXEC,
      stdout: "git version 2.40.0\n",
    }));
    const sshExec = vi.fn();
    installHost({}, { execAny, sshExec });

    const res = appStatus({ app: "git", target: "local" });
    expect(res.installed).toBe(true);
    expect(execAny).toHaveBeenCalled();
    expect(sshExec).not.toHaveBeenCalled();
  });

  it("routes a remote target through peckboard_ssh_exec, authenticating with Auth::KeyRef", () => {
    const store = installHost({});
    putTarget(
      buildRecord(
        { hostname: "box", username: "root", key_id: "vault-1" },
        null,
        () => "t1",
      ),
    );

    const execAny = vi.fn();
    const sshExec = vi.fn((input: any) => {
      expect(input.auth).toEqual({ key_id: "vault-1" });
      expect(input.host).toBe("box");
      return {
        ok: true,
        ...OK_EXEC,
        stdout: "git version 2.40.0\n",
        server_fingerprint: "SHA256:x",
        started_at: "",
        finished_at: "",
        duration_ms: 1,
      };
    });
    installHost(store, { execAny, sshExec });

    const res = appStatus({ app: "git", target: "t1" });
    expect(res.installed).toBe(true);
    expect(sshExec).toHaveBeenCalled();
    expect(execAny).not.toHaveBeenCalled();
  });
});
