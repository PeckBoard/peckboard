// The snapshot-bracket provenance pipeline, pure parts first: snapshot
// script building, snapshot parsing for the three package-database formats,
// delta computation, record building (including the failed-snapshot →
// "unknown" degradation and the vendor empty-delta case), then the
// store-backed record lifecycle and settleJobProvenance against the mocked
// exec host — same split as jobs.test.ts.

import { describe, expect, it, vi } from "vitest";
import { installHost } from "./hostShim";
import { JobRecord } from "../src/jobs";
import { LOCAL_TARGET } from "../src/targets";
import {
  buildInstallRecord,
  buildSnapshotFetchScript,
  buildSnapshotStep,
  computeDelta,
  deleteInstallRecord,
  getInstallRecord,
  listInstallRecords,
  parseSnapshot,
  putInstallRecord,
  settleJobProvenance,
  snapshotCommandFor,
  snapshotPathFor,
  splitSnapshotPair,
  trackingState,
  withSnapshotBracket,
  InstallRecord,
} from "../src/provenance";

const OK_EXEC = {
  exit_code: 0,
  stdout: "",
  stderr: "",
  stdout_truncated: false,
  stderr_truncated: false,
  timed_out: false,
};

function installJob(over: Partial<JobRecord> = {}): JobRecord {
  return {
    id: "j1",
    target_id: "local",
    app_id: "git",
    action: "install",
    pid: 42,
    logfile: "/tmp/peckboard-lam-j1.log",
    status: "succeeded",
    exit_code: 0,
    pm: "apt",
    method: "apt",
    ...over,
  };
}

function record(over: Partial<InstallRecord> = {}): InstallRecord {
  return {
    job_id: "j1",
    target_id: "local",
    app_id: "git",
    installed_at: "2026-08-09T00:00:00.000Z",
    method: "apt",
    tracking: "tracked",
    primary: { name: "git", version: "1:2.43.0-1" },
    added: [{ name: "git-man", version: "1:2.43.0-1" }],
    changed: [],
    ...over,
  };
}

// --- snapshot scripts -------------------------------------------------------

describe("snapshotCommandFor", () => {
  it("dumps name + version per line for each package database", () => {
    expect(snapshotCommandFor("apt")).toBe(
      "dpkg-query -W -f='${Package}\\t${Version}\\n'",
    );
    expect(snapshotCommandFor("dnf")).toBe(
      "rpm -qa --qf '%{NAME}\\t%{VERSION}-%{RELEASE}\\n'",
    );
    expect(snapshotCommandFor("zypper")).toBe(
      "rpm -qa --qf '%{NAME}\\t%{VERSION}-%{RELEASE}\\n'",
    );
    expect(snapshotCommandFor("pacman")).toBe("pacman -Q");
  });
});

describe("buildSnapshotStep", () => {
  it("writes the sorted snapshot, or the failure sentinel when the dump fails", () => {
    expect(buildSnapshotStep("pacman", "/tmp/s")).toBe(
      "if pacman -Q > '/tmp/s.tmp' 2>/dev/null; " +
        "then LC_ALL=C sort '/tmp/s.tmp' > '/tmp/s'; " +
        "else echo PECKBOARD_SNAPSHOT_FAILED > '/tmp/s'; fi; rm -f '/tmp/s.tmp'",
    );
  });
});

describe("withSnapshotBracket", () => {
  it("brackets the recipe and preserves its exit code for the job sentinel", () => {
    const s = withSnapshotBracket(
      "sudo -A apt-get install -y git",
      "apt",
      "j7",
    );
    expect(s).toContain("/tmp/peckboard-lam-j7.before.pkgs");
    expect(s).toContain("sudo -A apt-get install -y git; PECKBOARD_RC=$?;");
    expect(s).toContain("/tmp/peckboard-lam-j7.after.pkgs");
    expect(s.endsWith("; (exit $PECKBOARD_RC)")).toBe(true);
  });

  it("leaves the recipe unbracketed when there is no package manager", () => {
    expect(withSnapshotBracket("curl x | sh", null, "j7")).toBe("curl x | sh");
  });
});

describe("buildSnapshotFetchScript / splitSnapshotPair", () => {
  it("reads both files once, separator-delimited, then deletes them", () => {
    const b = snapshotPathFor("j7", "before");
    const a = snapshotPathFor("j7", "after");
    expect(buildSnapshotFetchScript("j7")).toBe(
      `cat '${b}' 2>/dev/null; echo; echo PECKBOARD_SNAPSHOT_SEPARATOR; ` +
        `cat '${a}' 2>/dev/null; rm -f '${b}' '${a}'`,
    );
  });

  it("splits fetch output into the two raw snapshots", () => {
    const raw = "git\t1\n\nPECKBOARD_SNAPSHOT_SEPARATOR\ngit\t1\nnew\t2\n";
    expect(splitSnapshotPair(raw)).toEqual({
      before: "git\t1\n",
      after: "git\t1\nnew\t2\n",
    });
  });

  it("returns null when the separator never appeared", () => {
    expect(splitSnapshotPair("sh: cat: not found")).toBeNull();
  });
});

// --- snapshot parsing -------------------------------------------------------

describe("parseSnapshot", () => {
  it("parses the dpkg-query tab-separated format", () => {
    expect(
      parseSnapshot(
        "git\t1:2.43.0-1\ngit-man\t1:2.43.0-1\nlibc6\t2.39-0ubuntu8\n",
      ),
    ).toEqual(
      new Map([
        ["git", "1:2.43.0-1"],
        ["git-man", "1:2.43.0-1"],
        ["libc6", "2.39-0ubuntu8"],
      ]),
    );
  });

  it("parses the rpm -qa --qf tab-separated format", () => {
    expect(parseSnapshot("bash\t5.2.26-3.fc40\ngit\t2.45.2-1.fc40\n")).toEqual(
      new Map([
        ["bash", "5.2.26-3.fc40"],
        ["git", "2.45.2-1.fc40"],
      ]),
    );
  });

  it("parses the pacman -Q space-separated format", () => {
    expect(parseSnapshot("git 2.45.2-1\nlinux 6.9.7.arch1-1\n")).toEqual(
      new Map([
        ["git", "2.45.2-1"],
        ["linux", "6.9.7.arch1-1"],
      ]),
    );
  });

  it("returns null — unknown, not empty — for the failure sentinel", () => {
    expect(parseSnapshot("PECKBOARD_SNAPSHOT_FAILED\n")).toBeNull();
  });

  it("returns null for empty output (a real package DB is never empty)", () => {
    expect(parseSnapshot("")).toBeNull();
    expect(parseSnapshot("\n\n")).toBeNull();
  });

  it("returns null when nothing parses as a package line", () => {
    expect(
      parseSnapshot("permission denied while reading the database"),
    ).toBeNull();
  });

  it("skips prose lines mixed into otherwise valid output", () => {
    expect(
      parseSnapshot("warning: db was locked, retrying\ngit\t1:2.43.0-1\n"),
    ).toEqual(new Map([["git", "1:2.43.0-1"]]));
  });
});

// --- delta ------------------------------------------------------------------

describe("computeDelta", () => {
  it("reports added, removed and version-changed packages, sorted by name", () => {
    const before = new Map([
      ["keep", "1.0"],
      ["upgraded", "1.0"],
      ["dropped", "1.0"],
    ]);
    const after = new Map([
      ["keep", "1.0"],
      ["upgraded", "2.0"],
      ["zeta", "3.0"],
      ["alpha", "0.1"],
    ]);
    expect(computeDelta(before, after)).toEqual({
      added: [
        { name: "alpha", version: "0.1" },
        { name: "zeta", version: "3.0" },
      ],
      removed: [{ name: "dropped", version: "1.0" }],
      changed: [{ name: "upgraded", from: "1.0", to: "2.0" }],
    });
  });

  it("reports no delta for identical snapshots", () => {
    const snap = new Map([["git", "1"]]);
    expect(computeDelta(snap, snap)).toEqual({
      added: [],
      removed: [],
      changed: [],
    });
  });
});

// --- record building --------------------------------------------------------

describe("buildInstallRecord", () => {
  const nowIso = "2026-08-09T00:00:00.000Z";

  it("records what genuinely landed: primary broken out, the rest as added", () => {
    const before = parseSnapshot("libc6\t2.39-0ubuntu8\n")!;
    const after = parseSnapshot(
      "git\t1:2.43.0-1\ngit-man\t1:2.43.0-1\nlibc6\t2.39-0ubuntu8\nliberror-perl\t0.17029-2\n",
    )!;
    const rec = buildInstallRecord({
      job: { id: "j9", target_id: "local", app_id: "git" },
      method: "apt",
      primaryNames: ["git"],
      before,
      after,
      nowIso,
    });
    expect(rec).toEqual({
      job_id: "j9",
      target_id: "local",
      app_id: "git",
      installed_at: nowIso,
      method: "apt",
      tracking: "tracked",
      primary: { name: "git", version: "1:2.43.0-1" },
      added: [
        { name: "git-man", version: "1:2.43.0-1" },
        { name: "liberror-perl", version: "0.17029-2" },
      ],
      changed: [],
    });
    expect(trackingState(rec)).toBe("tracked");
  });

  it("records an honestly-empty delta for a vendor install that touched no packages", () => {
    const snap = parseSnapshot("libc6\t2.39-0ubuntu8\n")!;
    const rec = buildInstallRecord({
      job: { id: "j9", target_id: "local", app_id: "claude" },
      method: "vendor",
      primaryNames: [],
      before: snap,
      after: snap,
      nowIso,
    });
    expect(rec.tracking).toBe("tracked");
    expect(rec.primary).toBeNull();
    expect(rec.added).toEqual([]);
    // …but the app itself is explicitly untracked, never "has no dependencies".
    expect(trackingState(rec)).toBe("untracked");
  });

  it("degrades to unknown — never a silently-empty delta — when a snapshot failed", () => {
    const rec = buildInstallRecord({
      job: { id: "j9", target_id: "local", app_id: "git" },
      method: "apt",
      primaryNames: ["git"],
      before: parseSnapshot("libc6\t1\n"),
      after: null,
      nowIso,
    });
    expect(rec.tracking).toBe("unknown");
    expect(rec.note).toMatch(/snapshot failed/);
    expect(rec.added).toEqual([]);
    expect(trackingState(rec)).toBe("unknown");
  });

  it("carries a caller-supplied reason for the unknown state", () => {
    const rec = buildInstallRecord({
      job: { id: "j9", target_id: "local", app_id: "git" },
      method: "vendor",
      primaryNames: [],
      before: null,
      after: null,
      nowIso,
      unknownNote: "no supported package manager",
    });
    expect(rec.note).toBe("no supported package manager");
  });

  it("keeps dependency upgrades in changed, not added", () => {
    const rec = buildInstallRecord({
      job: { id: "j9", target_id: "local", app_id: "git" },
      method: "apt",
      primaryNames: ["git"],
      before: parseSnapshot("git\t1:2.40.0-1\nlibcurl4\t8.0.0-1\n"),
      after: parseSnapshot("git\t1:2.43.0-1\nlibcurl4\t8.5.0-1\n"),
      nowIso,
    });
    expect(rec.added).toEqual([]);
    expect(rec.changed).toEqual([
      { name: "git", from: "1:2.40.0-1", to: "1:2.43.0-1" },
      { name: "libcurl4", from: "8.0.0-1", to: "8.5.0-1" },
    ]);
  });
});

// --- store-backed records ---------------------------------------------------

describe("install record store", () => {
  it("round-trips, lists per target, and a re-install supersedes", () => {
    installHost({});
    putInstallRecord(record({ job_id: "j1" }));
    putInstallRecord(
      record({ app_id: "ripgrep", job_id: "j2", primary: null }),
    );
    putInstallRecord(record({ target_id: "t9", job_id: "j3" }));

    expect(getInstallRecord("local", "git")?.job_id).toBe("j1");
    expect(listInstallRecords("local").map((r) => r.app_id)).toEqual([
      "git",
      "ripgrep",
    ]);

    // Same target+app again: superseded, not duplicated.
    putInstallRecord(record({ job_id: "j4" }));
    expect(listInstallRecords("local")).toHaveLength(2);
    expect(getInstallRecord("local", "git")?.job_id).toBe("j4");

    deleteInstallRecord("local", "git");
    expect(getInstallRecord("local", "git")).toBeNull();
  });
});

// --- settleJobProvenance ----------------------------------------------------

const FETCH_STDOUT =
  "libc6\t2.39-0ubuntu8\n\nPECKBOARD_SNAPSHOT_SEPARATOR\n" +
  "git\t1:2.43.0-1\ngit-man\t1:2.43.0-1\nlibc6\t2.39-0ubuntu8\n";

describe("settleJobProvenance", () => {
  it("consumes the bracket of a succeeded install into a tracked record, once", () => {
    const execAny = vi.fn((_input: any) => ({
      ...OK_EXEC,
      stdout: FETCH_STDOUT,
    }));
    installHost({}, { execAny });

    const job = installJob({ id: "j9" });
    settleJobProvenance(LOCAL_TARGET, job);

    expect(execAny).toHaveBeenCalledTimes(1);
    expect(execAny.mock.calls[0][0].args[1]).toContain(
      "cat '/tmp/peckboard-lam-j9.before.pkgs'",
    );
    const rec = getInstallRecord("local", "git")!;
    expect(rec.tracking).toBe("tracked");
    expect(rec.primary).toEqual({ name: "git", version: "1:2.43.0-1" });
    expect(rec.added).toEqual([{ name: "git-man", version: "1:2.43.0-1" }]);

    // A second poll of the same settled job must not re-read (the files are
    // gone) and must not overwrite the good record.
    settleJobProvenance(LOCAL_TARGET, job);
    expect(execAny).toHaveBeenCalledTimes(1);
    expect(getInstallRecord("local", "git")).toEqual(rec);
  });

  it("degrades to unknown when the fetch exec fails", () => {
    installHost(
      {},
      { execAny: () => ({ ...OK_EXEC, exit_code: 1, stderr: "boom" }) },
    );
    settleJobProvenance(LOCAL_TARGET, installJob());
    expect(getInstallRecord("local", "git")?.tracking).toBe("unknown");
  });

  it("degrades to unknown when the fetch output was truncated", () => {
    installHost(
      {},
      {
        execAny: () => ({
          ...OK_EXEC,
          stdout: FETCH_STDOUT,
          stdout_truncated: true,
        }),
      },
    );
    settleJobProvenance(LOCAL_TARGET, installJob());
    expect(getInstallRecord("local", "git")?.tracking).toBe("unknown");
  });

  it("records unknown without any exec when the install ran unbracketed", () => {
    const execAny = vi.fn();
    installHost({}, { execAny });
    settleJobProvenance(
      LOCAL_TARGET,
      installJob({ pm: undefined, method: undefined }),
    );
    expect(execAny).not.toHaveBeenCalled();
    const rec = getInstallRecord("local", "git")!;
    expect(rec.tracking).toBe("unknown");
    expect(rec.method).toBe("vendor");
    expect(rec.note).toMatch(/no supported package manager/);
  });

  it("does nothing for a failed install", () => {
    const execAny = vi.fn();
    installHost({}, { execAny });
    settleJobProvenance(LOCAL_TARGET, installJob({ status: "failed" }));
    expect(execAny).not.toHaveBeenCalled();
    expect(getInstallRecord("local", "git")).toBeNull();
  });

  it("drops the record when a remove succeeds, and keeps it when it fails", () => {
    installHost({});
    putInstallRecord(record());
    settleJobProvenance(
      LOCAL_TARGET,
      installJob({ action: "remove", status: "failed" }),
    );
    expect(getInstallRecord("local", "git")).not.toBeNull();
    settleJobProvenance(
      LOCAL_TARGET,
      installJob({ action: "remove", status: "succeeded" }),
    );
    expect(getInstallRecord("local", "git")).toBeNull();
  });

  it("records the pip namespace — no bracket exec — for a pip-method install", () => {
    const execAny = vi.fn();
    installHost({}, { execAny });
    settleJobProvenance(
      LOCAL_TARGET,
      installJob({ app_id: "graphifyy", pm: null, method: "pip" }),
    );
    expect(execAny).not.toHaveBeenCalled();
    const rec = getInstallRecord("local", "graphifyy")!;
    expect(rec.method).toBe("pip");
    expect(rec.tracking).toBe("pip");
    expect(rec.note).toMatch(/pip's namespace/);
    // Distinct machine-readable label — a pip package must never read as a
    // tracked/untracked system package.
    expect(trackingState(rec)).toBe("pip");
  });
});
