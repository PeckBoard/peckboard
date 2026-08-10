// The dependency graph, pure parts first: the defensive parsers for all
// three package-manager families (forward deps, reverse deps, file lists),
// the depth-limited BFS, shared/multi-parent marking, DAG→tree expansion,
// the reverse (library → apps) view, and the autoremove-accurate removal
// impact (a shared dependency must NEVER be listed as collateral). Then the
// exec/store-backed refresh pipeline against the mocked host — same split
// as provenance.test.ts.

import { describe, expect, it } from "vitest";
import { Store, installHost } from "./hostShim";
import { LOCAL_TARGET } from "../src/targets";
import { InstallRecord, putInstallRecord } from "../src/provenance";
import {
  DepEdge,
  DepNode,
  StoredDepGraph,
  buildDependsScript,
  buildDepsOverview,
  buildFileListScript,
  buildRdependsScript,
  buildWhatProvidesScript,
  clampDepth,
  classifyKind,
  depTree,
  depsOverview,
  expandLevels,
  buildPipFreezeScript,
  buildPipShowScript,
  parsePipFreeze,
  parsePipShow,
  getDepGraph,
  markShared,
  parseAptDepends,
  parseAptRdepends,
  parseFileListBatch,
  parsePacmanInfo,
  parseRpmRequiresBatch,
  parseRpmWhatProvides,
  parseRpmWhatRequires,
  refreshDepGraph,
  removalImpact,
  reverseEntries,
  systemReverseDeps,
} from "../src/deps";

// --- parsers: apt ------------------------------------------------------------

const APT_DEPENDS_BATCH = [
  "git",
  "  PreDepends: libc6",
  "  PreDepends: zlib1g",
  "  Depends: libcurl3-gnutls",
  "  Depends: git-man",
  " |Depends: perl:any",
  "  Depends: <perl-cgi-abstraction>",
  "    perl",
  "  Recommends: ca-certificates",
  "  Suggests: git-daemon-run",
  "  Replaces: git-core",
  "  Breaks: bash-completion",
  "curl",
  "  Depends: libc6",
  "  Depends: libcurl4t64",
].join("\n");

describe("parseAptDepends", () => {
  it("splits a batched output by column-0 headers and keeps only hard deps", () => {
    const map = parseAptDepends(APT_DEPENDS_BATCH);
    expect([...map.keys()]).toEqual(["git", "curl"]);
    expect(map.get("curl")).toEqual(["libc6", "libcurl4t64"]);
    const git = map.get("git")!;
    expect(git).toContain("libc6"); // PreDepends counts
    expect(git).toContain("zlib1g");
    expect(git).toContain("git-man");
    expect(git).not.toContain("ca-certificates"); // Recommends does not
    expect(git).not.toContain("git-daemon-run");
    expect(git).not.toContain("git-core");
    expect(git).not.toContain("bash-completion");
  });

  it("strips arch qualifiers, alternation pipes and virtual brackets", () => {
    const git = parseAptDepends(APT_DEPENDS_BATCH).get("git")!;
    expect(git).toContain("perl"); // from `|Depends: perl:any`
    expect(git).toContain("perl-cgi-abstraction"); // <virtual> unwrapped
    expect(git).not.toContain("perl:any");
    // The provider continuation line under the virtual entry is not a dep line.
    expect(git.filter((d) => d === "perl")).toHaveLength(1);
  });
});

describe("parseAptRdepends", () => {
  it("collects the indented dependents after the header", () => {
    const raw = [
      "libssl3",
      "Reverse Depends:",
      "  git",
      " |curl",
      "  <libssl-dev>",
      "  nodejs",
      "  nodejs",
    ].join("\n");
    expect(parseAptRdepends(raw)).toEqual([
      "git",
      "curl",
      "libssl-dev",
      "nodejs",
    ]);
  });

  it("returns nothing when the header never appears", () => {
    expect(parseAptRdepends("E: No packages found")).toEqual([]);
  });
});

// --- parsers: rpm ------------------------------------------------------------

const RPM_REQUIRES_BATCH = [
  "PECKBOARD_DEP_PKG:git",
  "/usr/bin/sh",
  "git-core = 2.45.0-1.fc40",
  "libc.so.6()(64bit)",
  "libcurl.so.4()(64bit)",
  "rpmlib(CompressedFileNames) <= 3.0.4-1",
  "config(git) = 2.45.0-1.fc40",
  "perl(Getopt::Long)",
  "libc.so.6()(64bit)",
  "PECKBOARD_DEP_PKG:nodejs",
  "libc.so.6()(64bit)",
  "nodejs-libs(x86-64) = 1:20.12.2-1.fc40",
].join("\n");

describe("parseRpmRequiresBatch", () => {
  it("splits by sentinel, strips version constraints and drops rpm internals", () => {
    const map = parseRpmRequiresBatch(RPM_REQUIRES_BATCH);
    expect(map.get("git")).toEqual([
      "/usr/bin/sh",
      "git-core",
      "libc.so.6()(64bit)",
      "libcurl.so.4()(64bit)",
      "perl(Getopt::Long)",
    ]);
    expect(map.get("nodejs")).toEqual([
      "libc.so.6()(64bit)",
      "nodejs-libs(x86-64)",
    ]);
  });
});

describe("parseRpmWhatProvides", () => {
  it("pairs capabilities with their first provider, null when nothing provides", () => {
    const raw = [
      "PECKBOARD_DEP_CAP:libc.so.6()(64bit)",
      "glibc",
      "glibc32",
      "PECKBOARD_DEP_CAP:/usr/bin/sh",
      "bash",
      "PECKBOARD_DEP_CAP:libweird.so.9",
      "no package provides libweird.so.9",
    ].join("\n");
    const map = parseRpmWhatProvides(raw);
    expect(map.get("libc.so.6()(64bit)")).toBe("glibc");
    expect(map.get("/usr/bin/sh")).toBe("bash");
    expect(map.get("libweird.so.9")).toBeNull();
  });
});

describe("parseRpmWhatRequires", () => {
  it("lists requiring packages and recognises the no-match message", () => {
    expect(parseRpmWhatRequires("git\nnodejs-libs\n")).toEqual([
      "git",
      "nodejs-libs",
    ]);
    expect(parseRpmWhatRequires("no package requires openssl-libs\n")).toEqual(
      [],
    );
  });
});

// --- parsers: pacman ---------------------------------------------------------

const PACMAN_QI = [
  "Name            : git",
  "Version         : 2.45.0-1",
  "Description     : the fast distributed version control system",
  "URL             : https://git-scm.com/",
  "Depends On      : curl  expat  perl  glibc>=2.38  openssl",
  "                  pcre2  zlib",
  "Optional Deps   : tk: gitk and git gui",
  "Required By     : None",
  "",
  "Name            : openssl",
  "Version         : 3.3.0-1",
  "Depends On      : glibc",
  "Required By     : curl  git  nodejs",
].join("\n");

describe("parsePacmanInfo", () => {
  it("parses sections, wrapped list fields, version pins and None", () => {
    const infos = parsePacmanInfo(PACMAN_QI);
    expect(infos).toHaveLength(2);
    expect(infos[0].name).toBe("git");
    expect(infos[0].version).toBe("2.45.0-1");
    expect(infos[0].depends).toEqual([
      "curl",
      "expat",
      "perl",
      "glibc", // >=2.38 stripped
      "openssl",
      "pcre2", // continuation line merged
      "zlib",
    ]);
    expect(infos[0].requiredBy).toEqual([]);
    expect(infos[1].requiredBy).toEqual(["curl", "git", "nodejs"]);
  });
});

// --- parsers: file lists -----------------------------------------------------

describe("parseFileListBatch", () => {
  it("keeps only bin/sbin entries from bare-path listings", () => {
    const raw = [
      "PECKBOARD_DEP_PKG:git",
      "/.",
      "/usr",
      "/usr/bin",
      "/usr/bin/git",
      "/usr/bin/git-shell",
      "/usr/share/doc/git/README",
      "/usr/sbin/gitd",
      "/bin/git-alias",
      "PECKBOARD_DEP_PKG:ripgrep",
      "/usr/bin/rg",
      "/usr/share/man/man1/rg.1.gz",
    ].join("\n");
    const map = parseFileListBatch(raw);
    expect(map.get("git")).toEqual([
      "/bin/git-alias",
      "/usr/bin/git",
      "/usr/bin/git-shell",
      "/usr/sbin/gitd",
    ]);
    expect(map.get("ripgrep")).toEqual(["/usr/bin/rg"]);
  });

  it("tolerates pacman's `pkg /path` -Ql shape", () => {
    const raw = [
      "PECKBOARD_DEP_PKG:git",
      "git /usr/",
      "git /usr/bin/",
      "git /usr/bin/git",
    ].join("\n");
    expect(parseFileListBatch(raw).get("git")).toEqual(["/usr/bin/git"]);
  });
});

// --- query scripts -----------------------------------------------------------

describe("query scripts", () => {
  it("batches natively where the manager separates output, sentinels where not", () => {
    expect(buildDependsScript("apt", ["git", "curl"])).toBe(
      "apt-cache depends 'git' 'curl' 2>/dev/null || true",
    );
    expect(buildDependsScript("dnf", ["git"])).toContain(
      "PECKBOARD_DEP_PKG:$p",
    );
    expect(buildDependsScript("dnf", ["git"])).toContain('rpm -qR "$p"');
    expect(buildDependsScript("pacman", ["git"])).toContain("pacman -Qi 'git'");
    expect(buildWhatProvidesScript(["libc.so.6()(64bit)"])).toContain(
      "--whatprovides",
    );
  });

  it("filters file listings to bin dirs on the target and quotes names", () => {
    const s = buildFileListScript("apt", ["git", "o'brien"]);
    expect(s).toContain("dpkg -L");
    expect(s).toContain("awk");
    expect(s).toContain("PECKBOARD_DEP_PKG:$p");
    expect(s).toContain("'o'\\''brien'"); // shQuote applied
  });

  it("asks for installed reverse deps only", () => {
    expect(buildRdependsScript("apt", "libssl3")).toContain(
      "rdepends --installed",
    );
    expect(buildRdependsScript("dnf", "openssl-libs")).toContain(
      "--whatrequires",
    );
    expect(buildRdependsScript("pacman", "openssl")).toContain("pacman -Qi");
  });
});

// --- graph assembly ----------------------------------------------------------

describe("expandLevels", () => {
  const versions = new Map([
    ["a", "1"],
    ["b", "1"],
    ["c", "1"],
    ["d", "1"],
  ]);
  const chain: Record<string, string[]> = { a: ["b"], b: ["c"], c: ["d"] };
  const resolve = (frontier: string[]) => {
    const m = new Map<string, string[]>();
    for (const f of frontier) m.set(f, chain[f] ?? []);
    return m;
  };

  it("stops at the depth limit", () => {
    const g = expandLevels(["a"], 2, 100, versions, resolve);
    expect(g.names).toEqual(["a", "b", "c"]); // d is beyond depth 2
    expect(g.edges).toEqual([
      { from: "a", to: "b", kind: "depends" },
      { from: "b", to: "c", kind: "depends" },
    ]);
    expect(g.truncated).toBe(false);
  });

  it("drops dependencies that are not installed and flags the node cap", () => {
    const notInstalled = expandLevels(
      ["a"],
      3,
      100,
      new Map([
        ["a", "1"],
        ["b", "1"],
      ]),
      resolve,
    );
    expect(notInstalled.names).toEqual(["a", "b"]); // c is not in the dump
    const capped = expandLevels(["a"], 3, 2, versions, resolve);
    expect(capped.truncated).toBe(true);
    expect(capped.names).toEqual(["a", "b"]);
  });
});

describe("classifyKind / clampDepth", () => {
  it("classifies apps, libraries, paths and supporting binaries", () => {
    const apps = new Set(["git"]);
    expect(classifyKind("git", apps)).toBe("app");
    expect(classifyKind("libssl3", apps)).toBe("library");
    expect(classifyKind("zlib1g", apps)).toBe("library");
    expect(classifyKind("libc.so.6()(64bit)", apps)).toBe("library");
    expect(classifyKind("/usr/bin/sh", apps)).toBe("binary");
    expect(classifyKind("perl", apps)).toBe("binary");
  });

  it("clamps the configurable depth into [1, 4] with a default of 2", () => {
    expect(clampDepth(undefined)).toBe(2);
    expect(clampDepth("nonsense")).toBe(2);
    expect(clampDepth(1)).toBe(1);
    expect(clampDepth("3")).toBe(3);
    expect(clampDepth(99)).toBe(4);
    expect(clampDepth(0)).toBe(1);
  });
});

// A DAG, not a tree: git and node both depend on libssl3.
function fixtureGraph(): StoredDepGraph {
  const nodes: DepNode[] = [
    {
      name: "git",
      version: "1:2.43.0-1",
      kind: "app",
      binaries: ["/usr/bin/git"],
    },
    { name: "nodejs", version: "18.19.0", kind: "app" },
    { name: "git-man", version: "1:2.43.0-1", kind: "binary" },
    { name: "libssl3", version: "3.0.13-1", kind: "library" },
    { name: "libc6", version: "2.38-3", kind: "library" },
  ];
  const edges: DepEdge[] = [
    { from: "git", to: "git-man", kind: "depends" },
    { from: "git", to: "libssl3", kind: "depends" },
    { from: "nodejs", to: "libssl3", kind: "depends" },
    { from: "nodejs", to: "libc6", kind: "depends" },
    { from: "libssl3", to: "libc6", kind: "depends" },
  ];
  return {
    target_id: "local",
    pm: "apt",
    at: "2026-08-09T00:00:00.000Z",
    depth: 2,
    truncated: false,
    nodes,
    edges,
  };
}

describe("markShared / depTree", () => {
  it("flags multi-parent nodes and shows them under EVERY requiring app", () => {
    const g = fixtureGraph();
    markShared(g.nodes, g.edges);
    expect(g.nodes.find((n) => n.name === "libssl3")?.shared).toBe(true);
    expect(g.nodes.find((n) => n.name === "git-man")?.shared).toBeUndefined();

    const gitTree = depTree(g, ["git"]);
    const nodeTree = depTree(g, ["nodejs"]);
    const gitSsl = gitTree[0].children.find((c) => c.name === "libssl3");
    const nodeSsl = nodeTree[0].children.find((c) => c.name === "libssl3");
    expect(gitSsl).toBeDefined();
    expect(nodeSsl).toBeDefined();
    expect(gitSsl!.shared).toBe(true);
    expect(nodeSsl!.shared).toBe(true);
    expect(gitSsl!.version).toBe("3.0.13-1");
    expect(gitTree[0].binaries).toEqual(["/usr/bin/git"]);
  });

  it("cuts cycles instead of recursing forever", () => {
    const g = fixtureGraph();
    g.edges.push({ from: "libc6", to: "libssl3", kind: "depends" });
    const tree = depTree(g, ["nodejs"]);
    const ssl = tree[0].children.find((c) => c.name === "libssl3")!;
    const c6 = ssl.children.find((c) => c.name === "libc6")!;
    expect(c6.children.find((c) => c.name === "libssl3")).toBeUndefined();
  });
});

describe("reverseEntries", () => {
  it("answers 'which apps require this library' across the whole closure", () => {
    const g = fixtureGraph();
    const entries = reverseEntries(
      g,
      new Map([
        ["git", "Git"],
        ["nodejs", "Node.js"],
      ]),
    );
    const ssl = entries.find((e) => e.name === "libssl3")!;
    expect(ssl.required_by).toEqual(["Git", "Node.js"]);
    expect(ssl.shared).toBe(true);
    // libc6 is reached by git only transitively (via libssl3) — still counted.
    const c6 = entries.find((e) => e.name === "libc6")!;
    expect(c6.required_by).toEqual(["Git", "Node.js"]);
    const man = entries.find((e) => e.name === "git-man")!;
    expect(man.required_by).toEqual(["Git"]);
    // No app packages in the reverse list.
    expect(entries.find((e) => e.name === "git")).toBeUndefined();
  });
});

describe("removalImpact", () => {
  const appsByPkg = new Map([
    ["git", "Git"],
    ["nodejs", "Node.js"],
  ]);

  it("never lists a shared dependency as collateral while another app needs it", () => {
    const impact = removalImpact(fixtureGraph(), ["git"], appsByPkg, "Git");
    expect(impact.also_removed.map((p) => p.name)).toEqual(["git-man"]);
    const kept = impact.kept.find((k) => k.name === "libssl3")!;
    expect(kept.needed_by).toEqual(["Node.js"]);
    expect(impact.note).toContain("git-man");
    expect(impact.note).toContain("libssl3 (needed by Node.js)");
    expect(impact.note).not.toMatch(/delete[^.]*libssl3/);
  });

  it("removes whole exclusive chains once nothing else holds them", () => {
    const g = fixtureGraph();
    g.nodes.push({
      name: "liberror-perl",
      version: "0.17029-2",
      kind: "library",
    });
    g.edges.push({ from: "git-man", to: "liberror-perl", kind: "depends" });
    const impact = removalImpact(g, ["git"], appsByPkg, "Git");
    expect(impact.also_removed.map((p) => p.name)).toEqual([
      "git-man",
      "liberror-perl",
    ]);
  });

  it("frees the shared dependency only when the last dependent goes", () => {
    const g = fixtureGraph();
    const impact = removalImpact(
      g,
      ["nodejs"],
      new Map([["nodejs", "Node.js"]]), // git is not a catalog root here
      "Node.js",
    );
    // git (an orphan-root package in this variant) still depends on libssl3.
    expect(impact.also_removed.map((p) => p.name)).not.toContain("libssl3");
  });
});

describe("buildDepsOverview", () => {
  it("builds tracked app entries with trees and removal prose", () => {
    const o = buildDepsOverview("local", fixtureGraph(), []);
    expect(o.graph.node_count).toBe(5);
    const git = o.apps.find((a: any) => a.id === "git");
    expect(git.tracked).toBe(true);
    expect(git.packages).toEqual(["git"]);
    expect(git.tree[0].children.length).toBe(2);
    expect(git.removal_note).toContain("git-man");
    const node = o.apps.find((a: any) => a.id === "node");
    expect(node.tracked).toBe(true);
    expect(node.packages).toEqual(["nodejs"]); // npm absent from the graph
    expect(o.libraries.find((l: any) => l.name === "libssl3").shared).toBe(
      true,
    );
  });

  it("renders vendor installs as 'not tracked', never as an empty tree", () => {
    const rec = {
      job_id: "j1",
      target_id: "local",
      app_id: "claude",
      installed_at: "2026-08-09T00:00:00.000Z",
      method: "vendor",
      tracking: "tracked",
      primary: null,
      added: [],
      changed: [],
    } as InstallRecord;
    const o = buildDepsOverview("local", fixtureGraph(), [rec]);
    const claude = o.apps.find((a: any) => a.id === "claude");
    expect(claude.tracked).toBe(false);
    expect(claude.tree).toBeNull();
    expect(claude.note).toContain("not tracked by the package manager");
  });

  it("labels a vendor app 'not tracked' even with no install record", () => {
    // claude installed outside app-manager (the common case): no InstallRecord
    // exists, but it must still read as vendor-untracked — never STALE_NOTE's
    // "Refresh dependencies to update", which can never move a vendor app into
    // the package graph.
    const o = buildDepsOverview("local", fixtureGraph(), []);
    const claude = o.apps.find((a: any) => a.id === "claude");
    expect(claude.tracked).toBe(false);
    expect(claude.tree).toBeNull();
    expect(claude.note).toContain("not tracked by the package manager");
    expect(claude.note).not.toContain("Refresh dependencies");
  });

  it("carries no graph block at all before the first resolution", () => {
    const o = buildDepsOverview("local", null, []);
    expect(o.graph).toBeNull();
    expect(o.libraries).toEqual([]);
    expect(o.apps.find((a: any) => a.id === "git").note).toBeNull();
  });
});

// --- exec/store-backed refresh ----------------------------------------------

const OK_EXEC = {
  exit_code: 0,
  stdout: "",
  stderr: "",
  stdout_truncated: false,
  stderr_truncated: false,
  timed_out: false,
};

const DUMP = [
  "git\t1:2.43.0-1",
  "git-man\t1:2.43.0-1",
  "libc6\t2.38-3",
  "libcurl3-gnutls\t8.5.0-2",
  "nodejs\t18.19.0+dfsg-6",
  "npm\t9.2.0-1",
  "libssl3\t3.0.13-1",
  "liberror-perl\t0.17029-2",
  "perl\t5.36.0-10",
].join("\n");

const APT_SECTIONS: Record<string, string> = {
  git: "git\n  Depends: git-man\n  Depends: libcurl3-gnutls\n  Depends: libc6\n  Recommends: patch\n",
  nodejs: "nodejs\n  Depends: libc6\n  Depends: libssl3\n",
  npm: "npm\n  Depends: nodejs\n",
  "liberror-perl": "liberror-perl\n  Depends: perl\n",
  "git-man": "git-man\n",
  libc6: "libc6\n",
  "libcurl3-gnutls": "libcurl3-gnutls\n  Depends: libc6\n  Depends: libssl3\n",
  libssl3: "libssl3\n  Depends: libc6\n",
  perl: "perl\n  Depends: perl-base\n  Depends: libc6\n",
};

const FILES = [
  "PECKBOARD_DEP_PKG:git",
  "/usr/bin/git",
  "/usr/bin/git-shell",
  "PECKBOARD_DEP_PKG:nodejs",
  "/usr/bin/node",
  "PECKBOARD_DEP_PKG:npm",
  "/usr/bin/npm",
].join("\n");

const RDEPS = [
  "libssl3",
  "Reverse Depends:",
  "  nodejs",
  "  libcurl3-gnutls",
].join("\n");

/** A canned apt world: only the packages actually named in the level's
 * script are answered, exactly as apt-cache itself behaves. `extra` lets a
 * test answer additional scripts (e.g. pip probes) before the apt chain;
 * return null to fall through. */
function installAptWorld(extra?: (script: string) => any | null): Store {
  return installHost(
    {},
    {
      execAny: ({ args }) => {
        const script = args[1] ?? "";
        if (extra) {
          const answered = extra(script);
          if (answered) return answered;
        }
        if (script.indexOf("os-release") >= 0) {
          return { ...OK_EXEC, stdout: "ID=debian\n" };
        }
        if (script.indexOf("dpkg-query -W") >= 0) {
          return { ...OK_EXEC, stdout: DUMP };
        }
        if (script.indexOf("apt-cache depends") >= 0) {
          let out = "";
          for (const name of Object.keys(APT_SECTIONS)) {
            if (script.indexOf("'" + name + "'") >= 0)
              out += APT_SECTIONS[name];
          }
          return { ...OK_EXEC, stdout: out };
        }
        if (script.indexOf("dpkg -L") >= 0) {
          return { ...OK_EXEC, stdout: FILES };
        }
        if (script.indexOf("apt-cache rdepends") >= 0) {
          return { ...OK_EXEC, stdout: RDEPS };
        }
        return { ...OK_EXEC, exit_code: 1, stderr: "unexpected: " + script };
      },
    },
  );
}

describe("refreshDepGraph (mocked apt host)", () => {
  it("resolves seeds → depth-2 graph, stores it, and shapes the overview", () => {
    const store = installAptWorld();
    // A provenance record contributes its delta to the seed set — but its
    // edges still come from the package manager, never from the delta.
    putInstallRecord({
      job_id: "j1",
      target_id: "local",
      app_id: "git",
      installed_at: "2026-08-09T00:00:00.000Z",
      method: "apt",
      tracking: "tracked",
      primary: { name: "git", version: "1:2.43.0-1" },
      added: [{ name: "liberror-perl", version: "0.17029-2" }],
      changed: [],
    });

    const o = refreshDepGraph(LOCAL_TARGET);
    expect(o.graph.pm).toBe("apt");
    expect(o.graph.truncated).toBe(false);
    // git, git-man, libc6, libcurl3-gnutls, liberror-perl, libssl3, nodejs,
    // npm, perl — perl-base is not installed and stays out.
    expect(o.graph.node_count).toBe(9);
    expect(o.graph.edge_count).toBe(11);

    const stored = getDepGraph("local")!;
    expect(stored.target_id).toBe("local");
    expect(store["depgraphs"]["local"].nodes.length).toBe(9);
    expect(stored.nodes.find((n) => n.name === "git")!.binaries).toEqual([
      "/usr/bin/git",
      "/usr/bin/git-shell",
    ]);
    expect(stored.nodes.find((n) => n.name === "libssl3")!.shared).toBe(true);
    expect(stored.nodes.find((n) => n.name === "libssl3")!.kind).toBe(
      "library",
    );

    const git = o.apps.find((a: any) => a.id === "git");
    expect(git.tree[0].name).toBe("git");
    expect(git.tree[0].children.map((c: any) => c.name)).toEqual([
      "git-man",
      "libc6",
      "libcurl3-gnutls",
    ]);
    // Removal safety: libcurl3-gnutls is git's alone, libssl3 is not.
    expect(git.also_removed.map((p: any) => p.name)).toEqual([
      "git-man",
      "libcurl3-gnutls",
    ]);
    expect(git.kept.find((k: any) => k.name === "libssl3").needed_by).toEqual([
      "Node.js",
    ]);

    // The read path returns the same thing without touching the target.
    const read = depsOverview(LOCAL_TARGET);
    expect(read.graph.node_count).toBe(9);
  });

  it("queries system-wide reverse deps only for names the graph knows", () => {
    installAptWorld();
    refreshDepGraph(LOCAL_TARGET);
    const r = systemReverseDeps(LOCAL_TARGET, "libssl3");
    expect(r.required_by).toEqual(["nodejs", "libcurl3-gnutls"]);
    expect(() => systemReverseDeps(LOCAL_TARGET, "evil; rm -rf /")).toThrow(
      /not in this target's dependency graph/,
    );
  });

  // --- pip namespace -----------------------------------------------------------

  describe("pip namespace probes", () => {
    it("builds the pip probe scripts", () => {
      expect(buildPipFreezeScript()).toContain("pip list --format=freeze");
      // pip names are shell-quoted like every other package name reaching a
      // command (defense-in-depth: the sibling apt/rpm builders quote too).
      expect(buildPipShowScript(["graphifyy", "foo"])).toContain(
        "pip show --disable-pip-version-check 'graphifyy' 'foo'",
      );
    });

    it("parses freeze output into versions, skipping non-pinned lines", () => {
      const m = parsePipFreeze(
        "graphifyy==1.4.2\nNetworkX==3.3\nnot a line\n-e git+https://x#egg=y\n",
      );
      expect(m.get("graphifyy")).toBe("1.4.2");
      expect(m.get("networkx")).toBe("3.3");
      expect(m.size).toBe(2);
    });

    it("parses multi-package pip show blocks with Requires/Required-by edges", () => {
      const raw = [
        "Name: graphifyy",
        "Version: 1.4.2",
        "Summary: graphs: for code",
        "Requires: networkx, graspologic",
        "Required-by: ",
        "---",
        "Name: networkx",
        "Version: 3.3",
        "Requires: ",
        "Required-by: graphifyy",
      ].join("\n");
      expect(parsePipShow(raw)).toEqual([
        {
          name: "graphifyy",
          version: "1.4.2",
          requires: ["networkx", "graspologic"],
          required_by: [],
        },
        {
          name: "networkx",
          version: "3.3",
          requires: [],
          required_by: ["graphifyy"],
        },
      ]);
    });

    it("ignores field lines before the first Name", () => {
      expect(parsePipShow("WARNING: stuff\nVersion: 9.9\n")).toEqual([]);
    });
  });

  describe("pip ride-along in refreshDepGraph", () => {
    it("probes pip packages via pip, labels them, and keeps them out of the system graph", () => {
      installAptWorld((script) => {
        if (script.indexOf("pip list --format=freeze") >= 0) {
          return { ...OK_EXEC, stdout: "graphifyy==1.4.2\nnetworkx==3.3\n" };
        }
        if (script.indexOf("pip show") >= 0) {
          return {
            ...OK_EXEC,
            stdout:
              "Name: graphifyy\nVersion: 1.4.0\nRequires: networkx, graspologic\nRequired-by: \n",
          };
        }
        return null;
      });
      const d = refreshDepGraph(LOCAL_TARGET);
      // Freeze is authoritative for the version (1.4.2, not pip show's 1.4.0).
      expect(d.pip_packages).toEqual([
        {
          name: "graphifyy",
          version: "1.4.2",
          requires: ["networkx", "graspologic"],
          required_by: [],
        },
      ]);
      // Separate namespace: pip names never join the system nodes, and the
      // app entry says where its dependencies actually live.
      expect(d.nodes.map((n: any) => n.name)).not.toContain("graphifyy");
      const entry = d.apps.find((a: any) => a.id === "graphifyy");
      expect(entry.tracked).toBe(false);
      expect(entry.note).toMatch(/pip's namespace/);
    });

    it("a host without pip leaves the pip section empty without sinking the refresh", () => {
      installAptWorld(); // pip scripts fall through to exit 1
      const d = refreshDepGraph(LOCAL_TARGET);
      expect(d.pip_packages).toEqual([]);
      expect(d.graph.node_count).toBeGreaterThan(0);
    });
  });
});
