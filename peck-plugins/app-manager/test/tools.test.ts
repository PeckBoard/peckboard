import { beforeEach, describe, expect, it, vi } from "vitest";
import { installHost } from "./hostShim";
import { dispatch } from "../src/lib";
import { appStatus } from "../src/tools";
import { buildCustomApp, getCustomApp, putCustomApp } from "../src/customApps";
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

  // The only write an agent has into this plugin's records. It exists because a
  // plugin cannot read a session transcript — and it is deliberately narrow.
  describe("app_record_details: how a session's findings get back", () => {
    function invoke(args: any) {
      return parse(
        dispatch("mcp.tool.invoke", {
          tool: "app_record_details",
          arguments: args,
        }),
      );
    }

    it("refuses a catalog app: its details are authored, not agent-written", () => {
      installHost({});
      const v = invoke({ app: "git", notes: "version control" });
      expect(v.payload.error).toMatch(/catalog app/);
    });

    it("refuses an unknown app", () => {
      installHost({});
      expect(invoke({ app: "nope" }).payload.error).toMatch(/unknown app/);
      expect(invoke({}).payload.error).toMatch(/app is required/);
    });

    it("fills the blanks, parks the command, and names all three outcomes", () => {
      installHost({});
      putCustomApp(buildCustomApp({ name: "Zellij", notes: "mine" }));

      const v = invoke({
        app: "zellij",
        notes: "theirs",
        homepage: "https://zellij.dev",
        install_command: "cargo install zellij",
      });
      expect(v.verdict).toBe("allow");
      expect(v.payload.applied).toEqual(["official website"]);
      expect(v.payload.suggested).toEqual(["install command"]);
      expect(v.payload.skipped).toEqual(["notes"]);
      expect(v.payload.still_missing).toContain("remove command");
      expect(v.payload.note).toContain("suggestion");

      const rec = getCustomApp("zellij")!;
      expect(rec.notes).toBe("mine");
      expect(rec.homepage).toBe("https://zellij.dev");
      // The command is NOT live — only a person accepting it makes it so.
      expect(rec.install_command).toBeUndefined();
      expect(rec.suggested_install_command).toBe("cargo install zellij");
    });

    it("returns a bad value as a tool error, leaving the record untouched", () => {
      installHost({});
      putCustomApp(buildCustomApp({ name: "Zellij" }));
      const v = invoke({ app: "zellij", binary: "zellij; rm -rf /" });
      expect(v.payload.error).toMatch(/valid command name/);
      expect(getCustomApp("zellij")?.binary).toBe("zellij");
    });
  });
});
