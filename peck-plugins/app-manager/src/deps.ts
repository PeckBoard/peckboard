// Dependency graph: which packages each installed app REQUIRES, queried
// from the package manager itself — never inferred from install-time deltas.
// Provenance (provenance.ts) answers "what arrived during this job"; this
// module answers "what does this app need right now". The two are separate
// collections and must never overwrite each other.
//
// It is a DAG, not a tree: install git and node and both depend on libssl3 —
// that node has TWO parents. Consequences honoured here:
//   - a shared dependency appears under EVERY app that requires it, flagged
//     `shared` (markShared / depTree), never attributed to whichever app's
//     install happened to pull it in first;
//   - removal safety follows autoremove semantics (removalImpact): a package
//     is collateral only when NOTHING outside the removal set still depends
//     on it, so removing git never presents libssl3 as deletable while node
//     needs it.
//
// Cost control: recursive expansion is one exec per BFS level (batched, not
// per package), seeded from the installed catalog apps' own packages plus
// the provenance delta set, depth-limited (default 2, max 4), capped at
// MAX_GRAPH_NODES with an explicit `truncated` flag — never a silent cap.
// The graph refreshes when a job settles and on explicit user request, and
// is stored with an `at` timestamp: it is a SNAPSHOT that drifts with
// upgrades, not permanent truth. Rendering always reads the cache.
//
// Honest limits: vendor curl|sh installs (claude, cursor-agent, ollama)
// never enter the package database, so they get an explicit "not tracked by
// the package manager" note instead of an empty tree; pip packages are a
// different namespace entirely and are kept OUT of the system graph — they
// ride along as `StoredDepGraph.pip`, probed with pip itself (`pip list
// --format=freeze` for versions, `pip show` for Requires/Required-by edges)
// and rendered as their own clearly-labelled section.
//
// Same layout as provenance.ts/jobs.ts: everything above the exec/store
// calls is pure, DOM-free and host-free, unit-tested in test/deps.test.ts.

import { APPS, isVendorApp, packagesFor } from "./catalog";
import {
  OS_RELEASE_PROBE,
  PackageManager,
  detectPackageManager,
  parseOsRelease,
} from "./distro";
import { runOnTarget } from "./exec";
import { storeGet, storePut } from "./host";
import { JobRecord, shQuote } from "./jobs";
import {
  InstallRecord,
  listInstallRecords,
  parseSnapshot,
  snapshotCommandFor,
} from "./provenance";
import { TargetRecord } from "./targets";

const DEP_COLLECTION = "depgraphs";
const PKG_SENTINEL = "PECKBOARD_DEP_PKG:";
const CAP_SENTINEL = "PECKBOARD_DEP_CAP:";

export const DEFAULT_DEPTH = 2;
export const MAX_DEPTH = 4;
/** Hard ceiling on graph size; hitting it sets `truncated` (shown in the
 * UI) rather than silently dropping packages. */
export const MAX_GRAPH_NODES = 600;
/** Package names per batched query exec, to keep command lines bounded. */
const QUERY_CHUNK = 100;

const PROBE_TIMEOUT_SECS = 20;
const DUMP_TIMEOUT_SECS = 60;
const QUERY_TIMEOUT_SECS = 60;

const VENDOR_NOTE =
  "Installed by a vendor script — not tracked by the package manager, so no dependency data exists for it.";
const STALE_NOTE =
  "Not in the dependency graph — either it was not installed through the package manager, or the graph predates its install. Refresh dependencies to update.";
const PIP_APP_NOTE =
  "Python package in pip's namespace — never part of the system package graph. Its pip-reported dependencies are in the pip section.";

export type DepNodeKind = "app" | "library" | "binary";

export interface DepNode {
  name: string;
  version: string;
  kind: DepNodeKind;
  /** Executables in bin/sbin dirs; resolved for catalog app packages only. */
  binaries?: string[];
  /** Required by two or more packages — a multi-parent DAG node. */
  shared?: boolean;
}

export interface DepEdge {
  from: string;
  to: string;
  kind: "depends";
}

export interface StoredDepGraph {
  target_id: string;
  pm: PackageManager;
  /** When this snapshot was resolved — dependency sets drift with upgrades. */
  at: string;
  depth: number;
  truncated: boolean;
  nodes: DepNode[];
  edges: DepEdge[];
  /** pip-namespace ride-along: the catalog's pip packages as pip reports
   * them. Beside the system graph, never merged into nodes/edges — pip is
   * a separate namespace (absent on graphs stored before it existed). */
  pip?: PipPkgInfo[];
}

// --- pure: classification ----------------------------------------------------

/** Kind is a display heuristic: catalog app packages are "app" roots;
 * lib-named packages and .so capabilities are "library"; everything else —
 * supporting tools like perl or git-man — renders as "binary". */
export function classifyKind(name: string, appPkgs: Set<string>): DepNodeKind {
  if (appPkgs.has(name)) return "app";
  if (name.startsWith("/")) return "binary";
  if (name.indexOf("lib") >= 0 || name.indexOf(".so") >= 0) return "library";
  return "binary";
}

export function clampDepth(v: unknown): number {
  const n =
    typeof v === "number"
      ? v
      : typeof v === "string" && v.trim()
        ? Number(v)
        : NaN;
  if (!Number.isFinite(n)) return DEFAULT_DEPTH;
  return Math.min(MAX_DEPTH, Math.max(1, Math.floor(n)));
}

// --- pure: query scripts -----------------------------------------------------

/** One batched forward-dependency query for a BFS level. apt and pacman
 * separate packages natively (header at column 0 / a Name field); rpm -qR
 * output has no package marker, so it gets a sentinel-per-package loop. */
export function buildDependsScript(pm: PackageManager, pkgs: string[]): string {
  const quoted = pkgs.map(shQuote).join(" ");
  switch (pm) {
    case "apt":
      return `apt-cache depends ${quoted} 2>/dev/null || true`;
    case "dnf":
    case "zypper":
      return `for p in ${quoted}; do echo "${PKG_SENTINEL}$p"; rpm -qR "$p" 2>/dev/null; done`;
    case "pacman":
      return `pacman -Qi ${quoted} 2>/dev/null || true`;
  }
}

/** rpm -qR yields capabilities (sonames, paths, perl(...) modules), not
 * package names; this resolves the ones that aren't already installed
 * package names, batched in one exec. */
export function buildWhatProvidesScript(caps: string[]): string {
  const quoted = caps.map(shQuote).join(" ");
  return (
    `for c in ${quoted}; do echo "${CAP_SENTINEL}$c"; ` +
    `rpm -q --whatprovides --qf '%{NAME}\\n' "$c" 2>/dev/null; done`
  );
}

/** Files each package provides, filtered to bin/sbin on the target (awk)
 * so a package shipping thousands of files doesn't flood the exec capture;
 * the parser applies the same filter again, so the awk is only an
 * output-size optimisation. */
export function buildFileListScript(
  pm: PackageManager,
  pkgs: string[],
): string {
  const lister =
    pm === "apt" ? "dpkg -L" : pm === "pacman" ? "pacman -Qlq" : "rpm -ql";
  const quoted = pkgs.map(shQuote).join(" ");
  return (
    `for p in ${quoted}; do echo "${PKG_SENTINEL}$p"; ` +
    `${lister} "$p" 2>/dev/null | awk '/^\\/(usr\\/)?(local\\/)?s?bin\\/[^\\/]+$/'; done`
  );
}

/** System-wide reverse dependencies of one package (installed only). */
export function buildRdependsScript(pm: PackageManager, pkg: string): string {
  const q = shQuote(pkg);
  switch (pm) {
    case "apt":
      return `apt-cache rdepends --installed ${q} 2>/dev/null || true`;
    case "dnf":
    case "zypper":
      return `rpm -q --whatrequires --qf '%{NAME}\\n' ${q} 2>/dev/null || true`;
    case "pacman":
      return `pacman -Qi ${q} 2>/dev/null || true`;
  }
}

// --- pure: parsers -----------------------------------------------------------
// Package-manager output is human-formatted and varies by version; every
// parser here is defensive — unknown lines are skipped, never fatal.

/** `apt-cache depends a b ...`: package headers at column 0, dependency
 * lines indented. Only Depends/PreDepends count (`|` marks an alternative);
 * `<...>` wraps virtual packages; `:any`/`:amd64` arch qualifiers are
 * stripped so names match the dpkg database. */
export function parseAptDepends(raw: string): Map<string, string[]> {
  const out = new Map<string, string[]>();
  let current: string[] | null = null;
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue;
    if (!/^\s/.test(line)) {
      current = [];
      out.set(stripAptDecorations(line), current);
      continue;
    }
    const m = /^\s*\|?\s*(?:Pre)?Depends:\s*(.+)$/.exec(line);
    if (!m || !current) continue;
    const name = stripAptDecorations(m[1]);
    if (name && current.indexOf(name) < 0) current.push(name);
  }
  return out;
}

/** `apt-cache rdepends <pkg>`: the package's own name, a "Reverse Depends:"
 * header, then one indented dependent per line. */
export function parseAptRdepends(raw: string): string[] {
  const out: string[] = [];
  let started = false;
  for (const line of raw.split("\n")) {
    const t = line.trim();
    if (!t) continue;
    if (/^Reverse Depends:/i.test(t)) {
      started = true;
      continue;
    }
    if (!started || !/^\s/.test(line)) continue;
    const name = stripAptDecorations(t.startsWith("|") ? t.slice(1) : t);
    if (name && out.indexOf(name) < 0) out.push(name);
  }
  return out;
}

function stripAptDecorations(s: string): string {
  let name = s.trim();
  if (name.startsWith("<") && name.endsWith(">")) name = name.slice(1, -1);
  return name.replace(/:[a-z0-9-]+$/i, "").trim();
}

function parseSentinelBatch(
  raw: string,
  sentinel: string,
): Map<string, string[]> {
  const out = new Map<string, string[]>();
  let current: string[] | null = null;
  for (const line of raw.split("\n")) {
    const t = line.trim();
    if (!t) continue;
    if (t.startsWith(sentinel)) {
      const key = t.slice(sentinel.length);
      current = out.get(key) ?? [];
      out.set(key, current);
      continue;
    }
    if (current) current.push(t);
  }
  return out;
}

/** Sentinel-batched `rpm -qR`: one capability per line, with version
 * constraints (`>= 3.0`) on the tail. rpmlib()/config() internals are
 * noise, not dependencies. */
export function parseRpmRequiresBatch(raw: string): Map<string, string[]> {
  const out = new Map<string, string[]>();
  for (const [pkg, lines] of parseSentinelBatch(raw, PKG_SENTINEL)) {
    const caps: string[] = [];
    for (const line of lines) {
      const cap = line.split(/\s+/)[0];
      if (!cap || /^rpmlib\(/.test(cap) || /^config\(/.test(cap)) continue;
      if (caps.indexOf(cap) < 0) caps.push(cap);
    }
    out.set(pkg, caps);
  }
  return out;
}

/** Sentinel-batched `rpm -q --whatprovides --qf '%{NAME}\n'`: the first
 * provider wins (multilib can list several); "no package provides"/error
 * lines resolve to null. */
export function parseRpmWhatProvides(raw: string): Map<string, string | null> {
  const out = new Map<string, string | null>();
  for (const [cap, lines] of parseSentinelBatch(raw, CAP_SENTINEL)) {
    const hit = lines.find(
      (l) => !/^no package provides/i.test(l) && !/^error:/i.test(l),
    );
    out.set(cap, hit ? hit.split(/\s+/)[0] : null);
  }
  return out;
}

/** `rpm -q --whatrequires <pkg>` with a NAME queryformat. */
export function parseRpmWhatRequires(raw: string): string[] {
  const out: string[] = [];
  for (const line of raw.split("\n")) {
    const t = line.trim();
    if (!t || /^no package requires/i.test(t) || /^error:/i.test(t)) continue;
    const name = t.split(/\s+/)[0];
    if (name && out.indexOf(name) < 0) out.push(name);
  }
  return out;
}

export interface PacmanInfo {
  name: string;
  version: string;
  depends: string[];
  requiredBy: string[];
}

/** `pacman -Qi a b ...`: "Field : value" lines per package section, values
 * wrapping onto indented continuation lines; list fields separate entries
 * with runs of spaces and use "None" for empty. Version pins (`bash>=5`)
 * are stripped to bare names. */
export function parsePacmanInfo(raw: string): PacmanInfo[] {
  const sections: Array<Record<string, string>> = [];
  let fields: Record<string, string> | null = null;
  let currentKey: string | null = null;
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue;
    const m = /^([A-Za-z][A-Za-z0-9 ]*?)\s*:\s?(.*)$/.exec(line);
    if (m && !/^\s/.test(line)) {
      const key = m[1].trim().toLowerCase();
      if (key === "name") {
        if (fields) sections.push(fields);
        fields = {};
      }
      if (!fields) continue;
      currentKey = key;
      fields[key] = m[2].trim();
    } else if (fields && currentKey && /^\s/.test(line)) {
      fields[currentKey] += " " + line.trim();
    }
  }
  if (fields) sections.push(fields);
  return sections
    .filter((s) => s["name"])
    .map((s) => ({
      name: s["name"],
      version: s["version"] ?? "",
      depends: splitPacmanList(s["depends on"] ?? ""),
      requiredBy: splitPacmanList(s["required by"] ?? ""),
    }));
}

function splitPacmanList(v: string): string[] {
  if (!v.trim() || /^none$/i.test(v.trim())) return [];
  const out: string[] = [];
  for (const tok of v.split(/\s+/)) {
    const name = tok.replace(/[<>=].*$/, "").trim();
    if (name && out.indexOf(name) < 0) out.push(name);
  }
  return out;
}

/** Sentinel-batched file listings → executables in bin/sbin per package.
 * Tolerates both bare paths (dpkg -L, rpm -ql, pacman -Qlq) and pacman's
 * `pkg /path` -Ql shape. */
export function parseFileListBatch(raw: string): Map<string, string[]> {
  const out = new Map<string, string[]>();
  for (const [pkg, lines] of parseSentinelBatch(raw, PKG_SENTINEL)) {
    const bins: string[] = [];
    for (const line of lines) {
      const idx = line.indexOf(" /");
      const path = line.startsWith("/")
        ? line
        : idx >= 0
          ? line.slice(idx + 1)
          : "";
      const p = path.trim();
      if (isBinaryPath(p) && bins.indexOf(p) < 0) bins.push(p);
    }
    out.set(pkg, bins.sort());
  }
  return out;
}

export function isBinaryPath(path: string): boolean {
  return /^\/(?:usr\/)?(?:local\/)?s?bin\/[^/]+$/.test(path);
}

export function parseDependsOutput(
  pm: PackageManager,
  raw: string,
): Map<string, string[]> {
  switch (pm) {
    case "apt":
      return parseAptDepends(raw);
    case "dnf":
    case "zypper":
      return parseRpmRequiresBatch(raw);
    case "pacman": {
      const out = new Map<string, string[]>();
      for (const s of parsePacmanInfo(raw)) out.set(s.name, s.depends);
      return out;
    }
  }
}

export function parseRdependsOutput(pm: PackageManager, raw: string): string[] {
  switch (pm) {
    case "apt":
      return parseAptRdepends(raw);
    case "dnf":
    case "zypper":
      return parseRpmWhatRequires(raw);
    case "pacman": {
      const sections = parsePacmanInfo(raw);
      return sections.length ? sections[0].requiredBy : [];
    }
  }
}

// --- pure: pip namespace -----------------------------------------------------

/** One pip-namespace package, as pip itself reports it. `requires` /
 * `required_by` come from `pip show`'s Requires/Required-by lines — edges
 * that live entirely inside pip's namespace and never join the system
 * graph's nodes/edges. */
export interface PipPkgInfo {
  name: string;
  version: string;
  requires: string[];
  required_by: string[];
}

/** Versions for every pip-visible package: `name==version` per line. */
export function buildPipFreezeScript(): string {
  return "python3 -m pip list --format=freeze --disable-pip-version-check 2>/dev/null";
}

/** Requires/Required-by for the catalog's pip packages: one `pip show`
 * block per package, `---` lines between them. A package pip doesn't know
 * is simply missing from the output (pip warns on stderr and exits 1;
 * the parser sees neither). */
export function buildPipShowScript(pkgs: string[]): string {
  return (
    "python3 -m pip show --disable-pip-version-check " +
    pkgs.map(shQuote).join(" ") +
    " 2>/dev/null"
  );
}

export function parsePipFreeze(raw: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const line of raw.split("\n")) {
    const m = /^([A-Za-z0-9._-]+)==(.+)$/.exec(line.trim());
    if (m) out.set(m[1].toLowerCase(), m[2].trim());
  }
  return out;
}

/** Parse `pip show a b c` output: `Key: value` field lines, `---` between
 * packages. Unknown keys are skipped, never fatal. */
export function parsePipShow(raw: string): PipPkgInfo[] {
  const out: PipPkgInfo[] = [];
  let cur: PipPkgInfo | null = null;
  for (const line of raw.split("\n")) {
    if (line.trim() === "---") {
      cur = null;
      continue;
    }
    const m = /^([A-Za-z-]+):\s?(.*)$/.exec(line);
    if (!m) continue;
    const key = m[1].toLowerCase();
    const value = m[2].trim();
    if (key === "name") {
      cur = { name: value, version: "", requires: [], required_by: [] };
      out.push(cur);
    } else if (!cur) {
      continue;
    } else if (key === "version") {
      cur.version = value;
    } else if (key === "requires") {
      cur.requires = splitPipList(value);
    } else if (key === "required-by") {
      cur.required_by = splitPipList(value);
    }
  }
  return out;
}

function splitPipList(v: string): string[] {
  return v
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

// --- pure: graph assembly ----------------------------------------------------

export interface Expansion {
  names: string[];
  edges: DepEdge[];
  truncated: boolean;
}

/** Depth-limited BFS from the seed set. `resolveLevel` answers one level's
 * forward deps (one batched exec in production, a stub in tests). Deps not
 * in `versions` (not installed, unresolvable virtuals) are dropped: this is
 * a graph of the INSTALLED world, which is what removal safety needs. */
export function expandLevels(
  seeds: string[],
  depth: number,
  maxNodes: number,
  versions: Map<string, string>,
  resolveLevel: (frontier: string[]) => Map<string, string[]>,
): Expansion {
  const seen = new Set<string>(seeds);
  const edges: DepEdge[] = [];
  const edgeSeen = new Set<string>();
  let truncated = false;
  let frontier = seeds.slice();
  for (let level = 0; level < depth && frontier.length; level++) {
    const deps = resolveLevel(frontier);
    const next: string[] = [];
    for (const [from, tos] of deps) {
      for (const to of tos) {
        if (to === from || !versions.has(to)) continue;
        const key = from + " " + to;
        if (!edgeSeen.has(key)) {
          edgeSeen.add(key);
          edges.push({ from, to, kind: "depends" });
        }
        if (!seen.has(to)) {
          if (seen.size >= maxNodes) {
            truncated = true;
            continue;
          }
          seen.add(to);
          next.push(to);
        }
      }
    }
    frontier = next;
  }
  return { names: [...seen].sort(), edges, truncated };
}

/** Flag every node with two or more distinct dependents. Recomputed from
 * the edges on read as well, so a stored graph can never lie about it. */
export function markShared(nodes: DepNode[], edges: DepEdge[]): void {
  const parents = parentMap(edges);
  for (const n of nodes) {
    if ((parents.get(n.name)?.size ?? 0) >= 2) n.shared = true;
    else delete n.shared;
  }
}

/** The catalog app packages present in the installed set: pkg → app id.
 * These are the BFS roots (together with the provenance delta set). */
export function installedAppPackages(
  pm: PackageManager,
  versions: Map<string, string>,
): Map<string, string> {
  const out = new Map<string, string>();
  for (const app of APPS) {
    for (const p of packagesFor(app, pm)) {
      if (versions.has(p) && !out.has(p)) out.set(p, app.id);
    }
  }
  return out;
}

function parentMap(edges: DepEdge[]): Map<string, Set<string>> {
  const out = new Map<string, Set<string>>();
  for (const e of edges) {
    let s = out.get(e.to);
    if (!s) {
      s = new Set();
      out.set(e.to, s);
    }
    s.add(e.from);
  }
  return out;
}

function childMap(edges: DepEdge[]): Map<string, string[]> {
  const out = new Map<string, string[]>();
  for (const e of edges) {
    let k = out.get(e.from);
    if (!k) {
      k = [];
      out.set(e.from, k);
    }
    if (k.indexOf(e.to) < 0) k.push(e.to);
  }
  return out;
}

function reachableFrom(
  starts: string[],
  kids: Map<string, string[]>,
): Set<string> {
  const seen = new Set<string>(starts);
  const stack = starts.slice();
  while (stack.length) {
    const cur = stack.pop()!;
    for (const c of kids.get(cur) ?? []) {
      if (!seen.has(c)) {
        seen.add(c);
        stack.push(c);
      }
    }
  }
  return seen;
}

// --- pure: views -------------------------------------------------------------

export interface DepTreeNode {
  name: string;
  version: string;
  kind: DepNodeKind;
  shared: boolean;
  binaries?: string[];
  children: DepTreeNode[];
}

/** Expand the DAG into a per-app tree. A shared node appears under every
 * parent that requires it (that is the point of a DAG rendering); only a
 * cycle back to an ancestor on the current path is cut. */
export function depTree(
  graph: Pick<StoredDepGraph, "nodes" | "edges">,
  roots: string[],
): DepTreeNode[] {
  const byName = new Map(graph.nodes.map((n) => [n.name, n] as const));
  const kids = childMap(graph.edges);
  const parents = parentMap(graph.edges);
  const build = (name: string, path: Set<string>): DepTreeNode | null => {
    const n = byName.get(name);
    if (!n) return null;
    const nextPath = new Set(path);
    nextPath.add(name);
    const children = (kids.get(name) ?? [])
      .filter((c) => !path.has(c))
      .sort()
      .map((c) => build(c, nextPath))
      .filter((x): x is DepTreeNode => x !== null);
    const node: DepTreeNode = {
      name,
      version: n.version,
      kind: n.kind,
      shared: (parents.get(name)?.size ?? 0) >= 2,
      children,
    };
    if (n.binaries && n.binaries.length) node.binaries = n.binaries;
    return node;
  };
  return roots
    .map((r) => build(r, new Set()))
    .filter((x): x is DepTreeNode => x !== null);
}

export interface ReverseEntry {
  name: string;
  version: string;
  kind: DepNodeKind;
  shared: boolean;
  /** Catalog apps (display names) whose dependency closure includes this
   * package — the "which apps require this library" view. */
  required_by: string[];
}

export function reverseEntries(
  graph: Pick<StoredDepGraph, "nodes" | "edges">,
  appsByPkg: Map<string, string>,
): ReverseEntry[] {
  const kids = childMap(graph.edges);
  const parents = parentMap(graph.edges);
  const reachApps = new Map<string, Set<string>>();
  for (const [pkg, appName] of appsByPkg) {
    for (const v of reachableFrom([pkg], kids)) {
      if (v === pkg) continue;
      let s = reachApps.get(v);
      if (!s) {
        s = new Set();
        reachApps.set(v, s);
      }
      s.add(appName);
    }
  }
  return graph.nodes
    .filter((n) => !appsByPkg.has(n.name))
    .map((n) => ({
      name: n.name,
      version: n.version,
      kind: n.kind,
      shared: (parents.get(n.name)?.size ?? 0) >= 2,
      required_by: [...(reachApps.get(n.name) ?? [])].sort(),
    }))
    .sort(
      (a, b) =>
        Number(b.shared) - Number(a.shared) || a.name.localeCompare(b.name),
    );
}

export interface RemovalImpact {
  /** Autoremove collateral: reachable deps nothing else still needs. */
  also_removed: Array<{ name: string; version: string }>;
  /** Reachable deps that SURVIVE the removal, with why (which other apps —
   * or, failing that, which direct dependents — still need them). */
  kept: Array<{ name: string; version: string; needed_by: string[] }>;
  note: string;
}

/** Simulate `remove <app>` + autoremove on the installed graph: starting
 * from the app's own packages, a dependency joins the removal set only when
 * every dependent it has is already in it. Catalog app packages other than
 * the removed app's are protected roots. A shared dependency therefore
 * NEVER shows up as collateral while another app still requires it — the
 * list must agree with what the package manager would actually do. */
export function removalImpact(
  graph: Pick<StoredDepGraph, "nodes" | "edges">,
  ownPkgs: string[],
  appsByPkg: Map<string, string>,
  appName: string,
): RemovalImpact {
  const byName = new Map(graph.nodes.map((n) => [n.name, n] as const));
  const parents = parentMap(graph.edges);
  const kids = childMap(graph.edges);
  const removed = new Set(ownPkgs);
  const protectedRoots = new Set(
    [...appsByPkg.keys()].filter((p) => !removed.has(p)),
  );

  let changed = true;
  while (changed) {
    changed = false;
    for (const n of graph.nodes) {
      if (removed.has(n.name) || protectedRoots.has(n.name)) continue;
      const ps = parents.get(n.name);
      if (!ps || ps.size === 0) continue; // an orphan root is not collateral
      let orphaned = true;
      for (const p of ps) {
        if (!removed.has(p)) {
          orphaned = false;
          break;
        }
      }
      if (orphaned) {
        removed.add(n.name);
        changed = true;
      }
    }
  }

  const reach = reachableFrom(ownPkgs, kids);
  for (const p of ownPkgs) reach.delete(p);

  const otherReach = new Map<string, Set<string>>();
  for (const [pkg, name] of appsByPkg) {
    if (name === appName) continue;
    for (const v of reachableFrom([pkg], kids)) {
      let s = otherReach.get(v);
      if (!s) {
        s = new Set();
        otherReach.set(v, s);
      }
      s.add(name);
    }
  }

  const also_removed = [...removed]
    .filter((n) => ownPkgs.indexOf(n) < 0)
    .sort()
    .map((n) => ({ name: n, version: byName.get(n)?.version ?? "" }));

  const kept = [...reach]
    .filter((n) => !removed.has(n))
    .sort()
    .map((name) => {
      const needs = [...(otherReach.get(name) ?? [])].sort();
      if (!needs.length) {
        for (const p of parents.get(name) ?? []) {
          if (!removed.has(p) && needs.indexOf(p) < 0) needs.push(p);
        }
        needs.sort();
      }
      return {
        name,
        version: byName.get(name)?.version ?? "",
        needed_by: needs,
      };
    });

  const parts: string[] = [];
  if (also_removed.length) {
    const n = also_removed.length;
    parts.push(
      `Removing ${appName} leaves ${n === 1 ? "one package" : `${n} packages`} unneeded ` +
        `(autoremove would delete ${n === 1 ? "it" : "them"}): ` +
        `${nameList(
          also_removed.map((p) => p.name),
          8,
        )}.`,
    );
  } else {
    parts.push(`No other packages become unneeded when ${appName} is removed.`);
  }
  const sharedKept = kept.filter((k) => k.needed_by.length);
  if (sharedKept.length) {
    parts.push(
      "Shared dependencies stay installed: " +
        nameList(
          sharedKept.map(
            (k) => `${k.name} (needed by ${k.needed_by.join(", ")})`,
          ),
          5,
        ) +
        ".",
    );
  }
  return { also_removed, kept, note: parts.join(" ") };
}

function nameList(names: string[], cap: number): string {
  if (names.length <= cap) return names.join(", ");
  return names.slice(0, cap).join(", ") + ` and ${names.length - cap} more`;
}

/** Everything the dashboard / app_deps needs, shaped as plain data. Reads
 * only what it is given — building this NEVER touches the target. */
export function buildDepsOverview(
  targetId: string,
  graph: StoredDepGraph | null,
  records: InstallRecord[],
): any {
  const appPkgs = new Map<string, string[]>();
  const appsByPkg = new Map<string, string>();
  if (graph) {
    markShared(graph.nodes, graph.edges);
    const nodeNames = new Set(graph.nodes.map((n) => n.name));
    for (const app of APPS) {
      const pkgs = packagesFor(app, graph.pm).filter((p) => nodeNames.has(p));
      if (pkgs.length) {
        appPkgs.set(app.id, pkgs);
        for (const p of pkgs) {
          if (!appsByPkg.has(p)) appsByPkg.set(p, app.name);
        }
      }
    }
  }
  const apps = APPS.map((app) => {
    const rec = records.find((r) => r.app_id === app.id) ?? null;
    const pkgs = appPkgs.get(app.id) ?? [];
    if (graph && pkgs.length) {
      const impact = removalImpact(graph, pkgs, appsByPkg, app.name);
      return {
        id: app.id,
        name: app.name,
        tracked: true,
        note: null as string | null,
        packages: pkgs,
        tree: depTree(graph, pkgs),
        also_removed: impact.also_removed,
        kept: impact.kept,
        removal_note: impact.note,
      };
    }
    return {
      id: app.id,
      name: app.name,
      tracked: false,
      note:
        app.namespace === "pip"
          ? PIP_APP_NOTE
          : isVendorApp(app) || (rec && rec.method === "vendor")
            ? VENDOR_NOTE
            : graph
              ? STALE_NOTE
              : null,
      packages: [] as string[],
      tree: null,
      also_removed: [],
      kept: [],
      removal_note: null as string | null,
    };
  });
  return {
    target: targetId,
    graph: graph
      ? {
          at: graph.at,
          pm: graph.pm,
          depth: graph.depth,
          truncated: !!graph.truncated,
          node_count: graph.nodes.length,
          edge_count: graph.edges.length,
        }
      : null,
    apps,
    // pip namespace, separate from `nodes`/`edges` — label it as such.
    pip_packages: graph?.pip ?? [],
    libraries: graph ? reverseEntries(graph, appsByPkg) : [],
    nodes: graph ? graph.nodes : [],
    edges: graph ? graph.edges : [],
  };
}

// --- store-backed graph ------------------------------------------------------

export function getDepGraph(targetId: string): StoredDepGraph | null {
  const g = storeGet(DEP_COLLECTION, targetId);
  if (!g || !Array.isArray(g.nodes) || !Array.isArray(g.edges)) return null;
  return g as StoredDepGraph;
}

export function putDepGraph(graph: StoredDepGraph): void {
  storePut(DEP_COLLECTION, graph.target_id, graph);
}

/** The read path behind GET /deps and app_deps: cached graph only, zero
 * execs — rendering must never trigger resolution. */
export function depsOverview(target: TargetRecord): any {
  return buildDepsOverview(
    target.id,
    getDepGraph(target.id),
    listInstallRecords(target.id),
  );
}

// --- exec-backed refresh -----------------------------------------------------

function chunkList<T>(items: T[], size: number): T[][] {
  const out: T[][] = [];
  for (let i = 0; i < items.length; i += size)
    out.push(items.slice(i, i + size));
  return out;
}

function queryLevel(
  target: TargetRecord,
  pm: PackageManager,
  frontier: string[],
  versions: Map<string, string>,
): Map<string, string[]> {
  const merged = new Map<string, string[]>();
  for (const chunk of chunkList(frontier, QUERY_CHUNK)) {
    const res = runOnTarget(
      target,
      buildDependsScript(pm, chunk),
      QUERY_TIMEOUT_SECS,
    );
    if (res.truncated) {
      throw new Error(
        "the dependency query output was truncated on the target, so the graph would be incomplete",
      );
    }
    if (!res.ok && !res.stdout.trim()) {
      throw new Error(
        `the dependency query failed on the target: ${res.stderr.trim() || "no output"}`,
      );
    }
    for (const [pkg, deps] of parseDependsOutput(pm, res.stdout)) {
      merged.set(pkg, deps);
    }
  }
  if (pm === "dnf" || pm === "zypper") {
    return resolveRpmCaps(target, merged, versions);
  }
  return merged;
}

/** Swap rpm capabilities for the installed packages that provide them; a
 * capability that is already an installed package name needs no lookup, and
 * one nothing provides is dropped. */
function resolveRpmCaps(
  target: TargetRecord,
  deps: Map<string, string[]>,
  versions: Map<string, string>,
): Map<string, string[]> {
  const unresolved: string[] = [];
  for (const caps of deps.values()) {
    for (const c of caps) {
      if (!versions.has(c) && unresolved.indexOf(c) < 0) unresolved.push(c);
    }
  }
  const provides = new Map<string, string | null>();
  for (const chunk of chunkList(unresolved, QUERY_CHUNK)) {
    const res = runOnTarget(
      target,
      buildWhatProvidesScript(chunk),
      QUERY_TIMEOUT_SECS,
    );
    if (!res.stdout.trim()) continue; // degrade: those capabilities drop out
    for (const [cap, pkg] of parseRpmWhatProvides(res.stdout)) {
      provides.set(cap, pkg);
    }
  }
  const out = new Map<string, string[]>();
  for (const [pkg, caps] of deps) {
    const names: string[] = [];
    for (const c of caps) {
      const name = versions.has(c) ? c : (provides.get(c) ?? null);
      if (name && names.indexOf(name) < 0) names.push(name);
    }
    out.set(pkg, names);
  }
  return out;
}

/** Probe the catalog's pip-namespace packages with pip itself: freeze for
 * authoritative versions, `pip show` for Requires/Required-by edges.
 * Returns only packages pip actually knows — an uninstalled one is absent,
 * never an empty entry. */
function probePipPackages(target: TargetRecord, pkgs: string[]): PipPkgInfo[] {
  const freeze = runOnTarget(
    target,
    buildPipFreezeScript(),
    QUERY_TIMEOUT_SECS,
  );
  const versions = parsePipFreeze(freeze.stdout);
  const show = runOnTarget(
    target,
    buildPipShowScript(pkgs),
    QUERY_TIMEOUT_SECS,
  );
  const wanted = new Set(pkgs.map((p) => p.toLowerCase()));
  return parsePipShow(show.stdout)
    .filter((i) => wanted.has(i.name.toLowerCase()))
    .map((i) => ({
      ...i,
      version: versions.get(i.name.toLowerCase()) ?? i.version,
    }));
}
/**
 * Re-resolve the whole graph for a target: one package-database dump, one
 * batched query per BFS level (plus one capability-resolution pass on rpm),
 * one batched file listing for the app packages' binaries. Runs on job
 * settle and on explicit user request — NEVER on page render. Throws prose
 * on failure and leaves the previous snapshot in place.
 */
export function refreshDepGraph(target: TargetRecord, depthArg?: unknown): any {
  const depth = clampDepth(depthArg);
  const os = runOnTarget(target, OS_RELEASE_PROBE, PROBE_TIMEOUT_SECS);
  if (!os.ok) {
    throw new Error(
      `could not read /etc/os-release on '${target.label}': ${os.stderr.trim() || "no output"}`,
    );
  }
  const pm = detectPackageManager(parseOsRelease(os.stdout));
  if (!pm) {
    throw new Error(
      `'${target.label}' has no supported package manager, so no dependency graph can be resolved`,
    );
  }
  const dump = runOnTarget(target, snapshotCommandFor(pm), DUMP_TIMEOUT_SECS);
  if (!dump.ok || dump.truncated) {
    throw new Error(
      `could not list the installed packages on '${target.label}': ` +
        `${dump.truncated ? "the listing was truncated" : dump.stderr.trim() || "no output"}`,
    );
  }
  const versions = parseSnapshot(dump.stdout);
  if (!versions) {
    throw new Error(
      `the package listing from '${target.label}' could not be parsed`,
    );
  }

  const appsByPkg = installedAppPackages(pm, versions);
  const seeds = new Set<string>(appsByPkg.keys());
  for (const rec of listInstallRecords(target.id)) {
    if (rec.primary && versions.has(rec.primary.name))
      seeds.add(rec.primary.name);
    for (const p of rec.added ?? []) {
      if (versions.has(p.name)) seeds.add(p.name);
    }
  }

  const expansion = expandLevels(
    [...seeds].sort(),
    depth,
    MAX_GRAPH_NODES,
    versions,
    (frontier) => queryLevel(target, pm, frontier, versions),
  );

  // Binaries are cosmetic detail for the app rows; a failed listing must
  // not sink the refresh.
  let binaries = new Map<string, string[]>();
  const appPkgSet = new Set(appsByPkg.keys());
  if (appPkgSet.size) {
    const res = runOnTarget(
      target,
      buildFileListScript(pm, [...appPkgSet].sort()),
      QUERY_TIMEOUT_SECS,
    );
    if (res.stdout.trim()) binaries = parseFileListBatch(res.stdout);
  }

  const nodes: DepNode[] = expansion.names.map((name) => {
    const node: DepNode = {
      name,
      version: versions.get(name) ?? "",
      kind: classifyKind(name, appPkgSet),
    };
    const bins = binaries.get(name);
    if (bins && bins.length) node.binaries = bins;
    return node;
  });
  markShared(nodes, expansion.edges);

  // pip ride-along: probed with pip itself, stored beside the system graph,
  // never merged into its nodes/edges — pip is a separate namespace. A
  // missing or broken pip must not sink the system refresh.
  const pipNames = APPS.filter((a) => a.pip_package).map(
    (a) => a.pip_package as string,
  );
  let pip: PipPkgInfo[] = [];
  if (pipNames.length) {
    try {
      pip = probePipPackages(target, pipNames.sort());
    } catch {
      /* pip absent or unusable — the pip section just stays empty */
    }
  }
  const graph: StoredDepGraph = {
    target_id: target.id,
    pm,
    at: new Date().toISOString(),
    depth,
    truncated: expansion.truncated,
    nodes,
    edges: expansion.edges,
    pip,
  };
  putDepGraph(graph);
  return buildDepsOverview(target.id, graph, listInstallRecords(target.id));
}

/** Post-job hook (see appState in tools.ts): a succeeded install or remove
 * changed the installed set, so the cached graph is stale. Swallows every
 * failure — the graph is a cache and must never break status polling. */
export function refreshAfterJob(target: TargetRecord, job: JobRecord): void {
  if (job.status !== "succeeded") return;
  try {
    refreshDepGraph(target);
  } catch {
    /* stale snapshot is survivable; the next explicit refresh heals it */
  }
}

/** System-wide reverse deps of one package, on explicit user request (one
 * exec). The name is validated against the stored graph before it goes
 * anywhere near a shell — only names the package manager itself reported
 * are queryable. */
export function systemReverseDeps(target: TargetRecord, pkgRef: unknown): any {
  const pkg = typeof pkgRef === "string" ? pkgRef.trim() : "";
  const graph = getDepGraph(target.id);
  if (!graph) {
    throw new Error(
      "no dependency graph exists for this target yet — refresh dependencies first",
    );
  }
  const node = graph.nodes.find((n) => n.name === pkg);
  if (!node) {
    throw new Error(
      `'${pkg || "(empty)"}' is not in this target's dependency graph, so it cannot be queried`,
    );
  }
  const res = runOnTarget(
    target,
    buildRdependsScript(graph.pm, node.name),
    QUERY_TIMEOUT_SECS,
  );
  if (!res.ok && !res.stdout.trim()) {
    throw new Error(
      `the reverse-dependency query failed on the target: ${res.stderr.trim() || "no output"}`,
    );
  }
  const required_by = parseRdependsOutput(graph.pm, res.stdout).filter(
    (n) => n !== node.name,
  );
  return {
    pkg: node.name,
    version: node.version,
    required_by,
    note: required_by.length
      ? null
      : "No installed package declares a dependency on it.",
  };
}
