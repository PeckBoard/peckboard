// The page (src/page.ts) is a static HTML string and cannot import anything,
// so every display decision it makes is shaped here first. These tests pin the
// shapes the page renders verbatim: badges, action labels, job headlines, and
// the error prose that replaces raw stderr / JSON.

import { describe, expect, it } from "vitest";
import { CatalogApp, findApp } from "../src/catalog";
import { JobRecord } from "../src/jobs";
import { InstallRecord } from "../src/provenance";
import { LOCAL_TARGET } from "../src/targets";
import {
  appRowView,
  distroView,
  friendlyError,
  jobView,
  targetView,
} from "../src/view";

const git = findApp("git")!;
const claude = findApp("claude")!;

function job(over: Partial<JobRecord>): JobRecord {
  return {
    id: "j1",
    target_id: "local",
    app_id: "git",
    action: "install",
    pid: 42,
    logfile: "/tmp/x.log",
    status: "running",
    ...over,
  };
}

describe("targetView", () => {
  it("describes the local target without host details", () => {
    expect(targetView(LOCAL_TARGET)).toMatchObject({
      id: "local",
      kind: "local",
      detail: "this Peckboard host",
    });
  });

  it("describes a remote target as user@host:port and carries the key id only", () => {
    const v = targetView({
      id: "t1",
      kind: "remote",
      label: "build box",
      hostname: "10.0.0.5",
      port: 2222,
      username: "ubuntu",
      key_id: "k1",
    });
    expect(v.detail).toBe("ubuntu@10.0.0.5:2222");
    expect(v.key_id).toBe("k1");
    expect(JSON.stringify(v)).not.toContain("PRIVATE KEY");
  });
});

describe("distroView", () => {
  it("summarises a recognised distro with its package manager", () => {
    const v = distroView({ id: "ubuntu", idLike: ["debian"], pm: "apt" }, null);
    expect(v.supported).toBe(true);
    expect(v.package_manager).toBe("apt");
    expect(v.summary).toContain("apt");
    expect(v.refusal).toBeNull();
  });

  it("stays supported but flags a distro with no known package manager", () => {
    const v = distroView({ id: "nixos", idLike: [], pm: null }, null);
    expect(v.supported).toBe(true);
    expect(v.package_manager).toBeNull();
    expect(v.summary).toContain("no supported package manager");
  });

  it("refuses a target that is not a readable Linux host, in prose", () => {
    const v = distroView(
      null,
      "could not read /etc/os-release on target 'local'; this plugin only supports Linux targets",
    );
    expect(v.supported).toBe(false);
    expect(v.refusal).toContain(
      "could not identify this target as a Linux host",
    );
    expect(v.refusal).not.toContain("{");
  });
});

describe("appRowView", () => {
  it("offers Install for a missing app", () => {
    const r = appRowView(git, { installed: false, version: null }, "apt", null);
    expect(r.state_label).toBe("Not installed");
    expect(r.action).toBe("install");
    expect(r.action_label).toBe("Install");
    expect(r.actionable).toBe(true);
    expect(r.blocked_reason).toBeNull();
  });

  it("offers Remove, with the version, for an installed app", () => {
    const r = appRowView(
      git,
      { installed: true, version: "git version 2.40.0" },
      "apt",
      null,
    );
    expect(r.state_label).toBe("Installed");
    expect(r.action).toBe("remove");
    expect(r.version).toBe("git version 2.40.0");
  });

  it("blocks the action with a reason when the distro has no recipe", () => {
    const r = appRowView(git, { installed: false, version: null }, null, null);
    expect(r.actionable).toBe(false);
    expect(r.blocked_reason).toContain("No install recipe");
  });

  it("keeps a vendor-script app actionable even with no package manager", () => {
    const r = appRowView(
      claude,
      { installed: false, version: null },
      null,
      null,
    );
    expect(r.actionable).toBe(true);
  });

  it("carries an attached job through to the row", () => {
    const r = appRowView(
      git,
      { installed: false, version: null },
      "apt",
      job({ status: "running" }),
    );
    expect(r.job?.tone).toBe("busy");
  });
  describe("appRowView for a manually added app", () => {
    // Built the same way customApps.toCatalogApp does, without touching a host.
    const manual: CatalogApp = {
      id: "zellij",
      name: "Zellij",
      description: "Manually added — terminal multiplexer.",
      custom: true,
      binary: "zellij",
      homepage: "https://zellij.dev",
      detect: "command -v 'zellij'",
      version: "'zellij' --version",
      install: {},
      remove: {},
    };
    const withCommands: CatalogApp = {
      ...manual,
      install: { vendor: "sudo -A apt-get install -y zellij" },
      remove: { vendor: "sudo -A apt-get remove -y zellij" },
    };

    it("is installable on the local host with no command at all (the session works it out)", () => {
      const r = appRowView(
        manual,
        { installed: false, version: null },
        "apt",
        null,
        null,
        "local",
      );
      expect(r.custom).toBe(true);
      expect(r.homepage).toBe("https://zellij.dev");
      expect(r.actionable).toBe(true);
      expect(r.blocked_reason).toBeNull();
      expect(r.action_command).toBeNull();
      expect(r.forgettable).toBe(true);
      expect(r.deps_note).toContain("aren't resolved");
    });

    it("is blocked on a remote target until it has an install command, and says why", () => {
      const r = appRowView(
        manual,
        { installed: false, version: null },
        "apt",
        null,
        null,
        "remote",
      );
      expect(r.actionable).toBe(false);
      expect(r.blocked_reason).toContain("only runs on the local host");
      const armed = appRowView(
        withCommands,
        { installed: false, version: null },
        "apt",
        null,
        null,
        "remote",
      );
      expect(armed.actionable).toBe(true);
      expect(armed.action_command).toBe("sudo -A apt-get install -y zellij");
    });

    it("offers no removal without a remove command, and points at Forget", () => {
      const r = appRowView(
        manual,
        { installed: true, version: "0.40.1" },
        "apt",
        null,
        null,
        "local",
      );
      expect(r.action).toBe("remove");
      expect(r.actionable).toBe(false);
      expect(r.blocked_reason).toContain("never guesses");
      expect(r.blocked_reason).toContain("Forget");
      expect(r.forgettable).toBe(true);

      const armed = appRowView(
        withCommands,
        { installed: true, version: "0.40.1" },
        "apt",
        null,
        null,
        "local",
      );
      expect(armed.actionable).toBe(true);
      expect(armed.action_command).toBe("sudo -A apt-get remove -y zellij");
    });

    it("never claims a vendor script installed it — it reports what the package DB saw", () => {
      const r = appRowView(
        manual,
        { installed: true, version: "0.40.1" },
        "apt",
        null,
        {
          job_id: "j9",
          target_id: "local",
          app_id: "zellij",
          installed_at: "2026-08-11T00:00:00.000Z",
          method: "vendor",
          tracking: "tracked",
          primary: null,
          added: [{ name: "zellij", version: "0.40.1" }],
          changed: [],
        },
        "local",
      );
      expect(r.provenance_note).toContain("Manually added app");
      expect(r.provenance_note).not.toContain("vendor script");
      expect(r.added_packages).toHaveLength(1);
    });
  });
});

describe("appRowView provenance", () => {
  const installedGit = { installed: true, version: "git version 2.43.0" };

  function installRecord(over: Partial<InstallRecord> = {}): InstallRecord {
    return {
      job_id: "j1",
      target_id: "local",
      app_id: "git",
      installed_at: "2026-08-09T00:00:00.000Z",
      method: "apt",
      tracking: "tracked",
      primary: { name: "git", version: "1:2.43.0-1" },
      added: [
        { name: "git-man", version: "1:2.43.0-1" },
        { name: "liberror-perl", version: "0.17029-2" },
      ],
      changed: [],
      ...over,
    };
  }

  it("notes the package-DB version and lists what came with the app", () => {
    const r = appRowView(git, installedGit, "apt", null, installRecord());
    // The probe stays authoritative for the binary; the package version is
    // noted alongside it.
    expect(r.version).toBe("git version 2.43.0");
    expect(r.package_version).toBe("1:2.43.0-1");
    expect(r.added_label).toBe("Installed with Git");
    expect(r.added_packages).toEqual([
      { name: "git-man", version: "1:2.43.0-1" },
      { name: "liberror-perl", version: "0.17029-2" },
    ]);
    expect(r.provenance_note).toBeNull();
  });

  it("renders a vendor install as explicitly untracked, never as 'no dependencies'", () => {
    const r = appRowView(
      claude,
      { installed: true, version: "1.2.3" },
      "apt",
      null,
      installRecord({
        app_id: "claude",
        method: "vendor",
        primary: null,
        added: [],
      }),
    );
    expect(r.provenance_note).toMatch(/not tracked by the package manager/);
    expect(r.added_packages).toEqual([]);
    expect(r.added_label).toBeNull();
    expect(r.package_version).toBeNull();
  });

  it("surfaces a failed snapshot bracket as unknown, in prose", () => {
    const r = appRowView(
      git,
      installedGit,
      "apt",
      null,
      installRecord({
        tracking: "unknown",
        note: "The package-database snapshot failed on the target, so what this install added is unknown.",
        primary: null,
        added: [],
      }),
    );
    expect(r.provenance_note).toMatch(/unknown/);
    expect(r.added_packages).toEqual([]);
  });

  it("suppresses stale provenance for an app that is no longer installed", () => {
    const r = appRowView(
      git,
      { installed: false, version: null },
      "apt",
      null,
      installRecord(),
    );
    expect(r.package_version).toBeNull();
    expect(r.provenance_note).toBeNull();
    expect(r.added_packages).toEqual([]);
  });
});
describe("jobView", () => {
  it("labels a running install", () => {
    const v = jobView(job({ status: "running" }), "reading package lists");
    expect(v.label).toBe("Installing…");
    expect(v.tone).toBe("busy");
    expect(v.log_tail).toBe("reading package lists");
  });

  it("labels a finished remove", () => {
    const v = jobView(
      job({ action: "remove", status: "succeeded", exit_code: 0 }),
      "",
    );
    expect(v.label).toBe("Remove succeeded");
    expect(v.tone).toBe("ok");
  });

  it("labels a failure with its exit code", () => {
    const v = jobView(job({ status: "failed", exit_code: 100 }), "");
    expect(v.label).toBe("Install failed (exit 100)");
    expect(v.tone).toBe("bad");
  });

  it("labels a crashed job that left no exit code", () => {
    const v = jobView(job({ status: "failed" }), "");
    expect(v.label).toBe("Install failed");
  });
});

describe("friendlyError", () => {
  it("explains a missing sudo password instead of dumping stderr", () => {
    const msg = friendlyError("sudo: a password is required");
    expect(msg).toContain("needs root on the target");
    expect(msg).toContain("sudo: a password is required");
  });

  it("explains an unreadable /etc/os-release", () => {
    expect(
      friendlyError("could not read /etc/os-release on target 'x'"),
    ).toContain("Only Linux targets can be managed here.");
  });

  it("explains an unreachable host", () => {
    expect(friendlyError("connect: Connection refused")).toContain(
      "Could not reach the target over SSH",
    );
  });

  it("explains rejected credentials", () => {
    expect(friendlyError("Permission denied (publickey)")).toContain(
      "refused the SSH credentials",
    );
  });

  it("unwraps a JSON error envelope rather than showing raw JSON", () => {
    expect(friendlyError('{"error":"hostname is required"}')).toBe(
      "Hostname is required",
    );
  });

  it("never returns an empty message", () => {
    expect(friendlyError("")).toContain("Something went wrong");
    expect(friendlyError(null)).toContain("Something went wrong");
  });
});
