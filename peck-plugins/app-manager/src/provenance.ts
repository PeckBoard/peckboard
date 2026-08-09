// Install provenance: what an install job GENUINELY added to the target.
//
// The core principle is to never trust the installer's self-report — no
// stdout parsing, no asking an agent. Instead every install with a known
// package manager is bracketed by package-database snapshots taken ON THE
// TARGET, inside the same detached script as the install itself:
//
//   snapshot(before) → recipe → snapshot(after) → delta = what landed
//
// Embedding the snapshots in the job script (rather than snapshotting at
// poll time) pins the bracket to the install's real start/finish, so a
// package added by some other activity minutes later can never leak into
// this job's delta. When the job's terminal state is first observed
// (tools.ts appState → settleJobProvenance), the two snapshot files are
// read and deleted in one exec and the delta becomes an `installs` record,
// keyed `<target_id>:<app_id>` so a re-install supersedes the previous
// record instead of duplicating it; a successful remove deletes it.
//
// A record is provenance — "arrived during this job" — NOT a dependency
// graph: a shared library is attributed to whichever job pulled it in
// first. Real dependency edges must come from the package manager itself.
//
// A snapshot that fails (unsupported package manager, permission denied,
// truncated or unparsable output) degrades to tracking "unknown", never to
// a silently-empty delta: an empty list must always mean "this job really
// added no packages". Vendor curl|sh installers on a recognised distro are
// still bracketed — their apt-installed prerequisites show up normally —
// but the app binary itself never enters the package database (see
// trackingState / view.ts for how that's surfaced honestly).
//
// Everything above the exec/store calls is pure and unit-tested in
// test/provenance.test.ts, same split as jobs.ts.

import { findApp, packagesFor } from "./catalog";
import { PackageManager } from "./distro";
import { runOnTarget } from "./exec";
import { storeDelete, storeGet, storeList, storePut } from "./host";
import { JobRecord, shQuote } from "./jobs";
import { TargetRecord } from "./targets";

const COLLECTION = "installs";
const SNAPSHOT_FAILED_SENTINEL = "PECKBOARD_SNAPSHOT_FAILED";
const SNAPSHOT_SEPARATOR = "PECKBOARD_SNAPSHOT_SEPARATOR";
const FETCH_TIMEOUT_SECS = 20;

const SNAPSHOT_FAILED_NOTE =
  "The package-database snapshot failed on the target, so what this install added is unknown.";
const NO_BRACKET_NOTE =
  "The install ran without a package-database snapshot (no supported package manager was detected when it started), so what it added is unknown.";
const PIP_NAMESPACE_NOTE =
  "Python package installed via pip — it lives in pip's namespace, so the package-database snapshot bracket deliberately does not apply. Versions come from pip itself (pip list --format=freeze).";

export interface PkgRef {
  name: string;
  version: string;
}

export interface PkgChange {
  name: string;
  from: string;
  to: string;
}

export type InstallMethod = PackageManager | "vendor" | "pip";

/** "tracked" = a real before/after delta was recorded; "unknown" = the
 * snapshot bracket failed and we refuse to claim anything about what
 * landed; "pip" = the app lives in pip's namespace, where the package-DB
 * bracket deliberately does not apply (versions come from pip itself).
 * There is deliberately no state for "empty" — an empty `added` on a
 * tracked record really means nothing new arrived. */
export type ProvenanceTracking = "tracked" | "unknown" | "pip";

export interface InstallRecord {
  job_id: string;
  target_id: string;
  app_id: string;
  installed_at: string;
  method: InstallMethod;
  tracking: ProvenanceTracking;
  /** Set when tracking is "unknown": why, in prose. */
  note?: string;
  /** The app's own package from the after-snapshot, when it maps to one
   * (vendor-script apps never do — their binary isn't in the package DB). */
  primary: PkgRef | null;
  /** Packages present after the install that weren't before — the app's
   * supporting cast; its own package is broken out as `primary`. */
  added: PkgRef[];
  /** Packages whose version changed during the install (e.g. dependencies
   * upgraded to satisfy the new app). */
  changed: PkgChange[];
}

// --- pure: snapshot scripts -------------------------------------------------

/** The package database dump for one manager: name + version, one package
 * per line (tab-separated; pacman uses a single space). */
export function snapshotCommandFor(pm: PackageManager): string {
  switch (pm) {
    case "apt":
      return "dpkg-query -W -f='${Package}\\t${Version}\\n'";
    case "dnf":
    case "zypper":
      return "rpm -qa --qf '%{NAME}\\t%{VERSION}-%{RELEASE}\\n'";
    case "pacman":
      return "pacman -Q";
  }
}

export function snapshotPathFor(
  jobId: string,
  which: "before" | "after",
): string {
  return `/tmp/peckboard-lam-${jobId}.${which}.pkgs`;
}

/**
 * One bracket step: write the sorted snapshot to `path`, or the failure
 * sentinel when the snapshot command itself fails (missing tool, permission
 * denied) — parseSnapshot turns the sentinel into "unknown", never into an
 * empty delta. Ends with `rm -f`, so it never perturbs `$?` for the
 * surrounding script.
 */
export function buildSnapshotStep(pm: PackageManager, path: string): string {
  const file = shQuote(path);
  const tmp = shQuote(path + ".tmp");
  return (
    `if ${snapshotCommandFor(pm)} > ${tmp} 2>/dev/null; ` +
    `then LC_ALL=C sort ${tmp} > ${file}; ` +
    `else echo ${SNAPSHOT_FAILED_SENTINEL} > ${file}; fi; rm -f ${tmp}`
  );
}

/**
 * Wrap an install recipe with before/after snapshot steps for the job's
 * detached script. The recipe's own exit code is what the job's
 * PECKBOARD_EXIT sentinel must report, so it is captured before the after-
 * snapshot and restored with a trailing `(exit $rc)` subshell. No package
 * manager → no bracket: the recipe runs unchanged and the record later
 * degrades to tracking "unknown".
 */
export function withSnapshotBracket(
  recipe: string,
  pm: PackageManager | null,
  jobId: string,
): string {
  if (!pm) return recipe;
  const before = buildSnapshotStep(pm, snapshotPathFor(jobId, "before"));
  const after = buildSnapshotStep(pm, snapshotPathFor(jobId, "after"));
  return `${before}; ${recipe}; PECKBOARD_RC=$?; ${after}; (exit $PECKBOARD_RC)`;
}

/** Read both snapshot files in one exec, then delete them — the bracket is
 * consumed exactly once, when the job's terminal state is first observed.
 * A missing file yields an empty section, which parses to "unknown". */
export function buildSnapshotFetchScript(jobId: string): string {
  const before = shQuote(snapshotPathFor(jobId, "before"));
  const after = shQuote(snapshotPathFor(jobId, "after"));
  return (
    `cat ${before} 2>/dev/null; echo; echo ${SNAPSHOT_SEPARATOR}; ` +
    `cat ${after} 2>/dev/null; rm -f ${before} ${after}`
  );
}

/** Split fetch-script output into the two raw snapshot texts, or null when
 * the separator never appeared (the fetch failed wholesale). */
export function splitSnapshotPair(
  raw: string,
): { before: string; after: string } | null {
  const lines = raw.split("\n");
  const idx = lines.findIndex((l) => l.trim() === SNAPSHOT_SEPARATOR);
  if (idx < 0) return null;
  return {
    before: lines.slice(0, idx).join("\n"),
    after: lines.slice(idx + 1).join("\n"),
  };
}

// --- pure: snapshot parsing + delta -----------------------------------------

/**
 * Parse one snapshot: `name<TAB>version` (dpkg-query / rpm --qf) or
 * `name version` (pacman -Q), one package per line. Returns null — meaning
 * "unknown", never "empty" — when the failure sentinel is present, the text
 * is empty (a real package database never has zero entries), or no line
 * parses as a package.
 */
export function parseSnapshot(text: string): Map<string, string> | null {
  const pkgs = new Map<string, string>();
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    if (trimmed === SNAPSHOT_FAILED_SENTINEL) return null;
    const tab = trimmed.indexOf("\t");
    const sep = tab >= 0 ? tab : trimmed.indexOf(" ");
    if (sep <= 0) continue;
    const name = trimmed.slice(0, sep).trim();
    const version = trimmed.slice(sep + 1).trim();
    // A version never contains whitespace in any of the three formats — a
    // line that "parses" with one is prose (a warning, an error), not data.
    if (!name || !version || /\s/.test(version)) continue;
    pkgs.set(name, version);
  }
  return pkgs.size === 0 ? null : pkgs;
}

export interface SnapshotDelta {
  added: PkgRef[];
  removed: PkgRef[];
  changed: PkgChange[];
}

/** Diff two snapshots by package name; results sorted by name so records
 * and their rendering are deterministic. */
export function computeDelta(
  before: Map<string, string>,
  after: Map<string, string>,
): SnapshotDelta {
  const added: PkgRef[] = [];
  const removed: PkgRef[] = [];
  const changed: PkgChange[] = [];
  for (const [name, version] of after) {
    const prev = before.get(name);
    if (prev === undefined) added.push({ name, version });
    else if (prev !== version) changed.push({ name, from: prev, to: version });
  }
  for (const [name, version] of before) {
    if (!after.has(name)) removed.push({ name, version });
  }
  const byName = (a: { name: string }, b: { name: string }) =>
    a.name.localeCompare(b.name);
  added.sort(byName);
  removed.sort(byName);
  changed.sort(byName);
  return { added, removed, changed };
}

/**
 * Build a job's install record from its parsed snapshots. Either snapshot
 * null → tracking "unknown" with the reason — the delta is NEVER silently
 * recorded as empty. `primaryNames` is what the chosen recipe explicitly
 * installs (empty for vendor scripts); the first one present in the after-
 * snapshot becomes `primary`, its version straight from the package DB.
 */
export function buildInstallRecord(args: {
  job: Pick<JobRecord, "id" | "target_id" | "app_id">;
  method: InstallMethod;
  primaryNames: string[];
  before: Map<string, string> | null;
  after: Map<string, string> | null;
  nowIso: string;
  unknownNote?: string;
}): InstallRecord {
  const base = {
    job_id: args.job.id,
    target_id: args.job.target_id,
    app_id: args.job.app_id,
    installed_at: args.nowIso,
    method: args.method,
  };
  if (!args.before || !args.after) {
    return {
      ...base,
      tracking: "unknown",
      note: args.unknownNote ?? SNAPSHOT_FAILED_NOTE,
      primary: null,
      added: [],
      changed: [],
    };
  }
  const delta = computeDelta(args.before, args.after);
  let primary: PkgRef | null = null;
  for (const name of args.primaryNames) {
    const version = args.after.get(name);
    if (version !== undefined) {
      primary = { name, version };
      break;
    }
  }
  const primaryName = primary ? primary.name : null;
  return {
    ...base,
    tracking: "tracked",
    primary,
    added: delta.added.filter((p) => p.name !== primaryName),
    changed: delta.changed,
  };
}

/** Machine-readable tracking state for the MCP surface: "tracked" (a real
 * delta was recorded), "untracked" (vendor installer — the app itself never
 * enters the package DB, though bracketed prerequisites still appear in
 * `added`), "unknown" (the snapshot bracket failed), "pip" (pip-namespace
 * app — versions tracked by pip, never by the distro package DB). */
export function trackingState(
  rec: InstallRecord,
): "tracked" | "untracked" | "unknown" | "pip" {
  if (rec.method === "pip" || rec.tracking === "pip") return "pip";
  if (rec.tracking === "unknown") return "unknown";
  return rec.method === "vendor" ? "untracked" : "tracked";
}

// --- store-backed records ---------------------------------------------------

function recordKey(targetId: string, appId: string): string {
  return `${targetId}:${appId}`;
}

export function putInstallRecord(rec: InstallRecord): void {
  storePut(COLLECTION, recordKey(rec.target_id, rec.app_id), rec);
}

export function getInstallRecord(
  targetId: string,
  appId: string,
): InstallRecord | null {
  return (
    (storeGet(COLLECTION, recordKey(targetId, appId)) as InstallRecord) ?? null
  );
}

export function deleteInstallRecord(targetId: string, appId: string): void {
  storeDelete(COLLECTION, recordKey(targetId, appId));
}

export function listInstallRecords(targetId: string): InstallRecord[] {
  return storeList(COLLECTION)
    .map((i) => i.value as InstallRecord)
    .filter((r) => r && r.target_id === targetId)
    .sort((a, b) => a.app_id.localeCompare(b.app_id));
}

/**
 * Called exactly when a poll first observes a job leaving "running" (see
 * appState in tools.ts). A successful install consumes its snapshot bracket
 * into an install record; a successful remove deletes the record — the
 * packages the app arrived with stop being "what this install added" once
 * the app is gone. A failed install records nothing: its app row already
 * reads "Not installed", and hanging a package list off it would present
 * half-landed debris as an install.
 */
export function settleJobProvenance(
  target: TargetRecord,
  job: JobRecord,
): void {
  if (job.action === "remove") {
    if (job.status === "succeeded") {
      deleteInstallRecord(job.target_id, job.app_id);
    }
    return;
  }
  if (job.status !== "succeeded") return;
  const existing = getInstallRecord(job.target_id, job.app_id);
  if (existing && existing.job_id === job.id) return; // bracket already consumed

  const method: InstallMethod = job.method ?? "vendor";
  // pip-namespace installs never touch the distro package DB, so there is
  // no bracket to consume — record the namespace instead of pretending the
  // snapshot failed.
  if (method === "pip") {
    putInstallRecord({
      job_id: job.id,
      target_id: job.target_id,
      app_id: job.app_id,
      installed_at: new Date().toISOString(),
      method,
      tracking: "pip",
      note: PIP_NAMESPACE_NOTE,
      primary: null,
      added: [],
      changed: [],
    });
    return;
  }
  const pm = job.pm ?? null;
  const app = findApp(job.app_id);
  const primaryNames = app ? packagesFor(app, pm) : [];

  let before: Map<string, string> | null = null;
  let after: Map<string, string> | null = null;
  let unknownNote: string | undefined;
  if (!pm) {
    unknownNote = NO_BRACKET_NOTE;
  } else {
    try {
      const res = runOnTarget(
        target,
        buildSnapshotFetchScript(job.id),
        FETCH_TIMEOUT_SECS,
      );
      const pair =
        res.ok && !res.truncated ? splitSnapshotPair(res.stdout) : null;
      if (pair) {
        before = parseSnapshot(pair.before);
        after = parseSnapshot(pair.after);
      }
    } catch {
      /* degrade to "unknown" below — provenance must never break a poll */
    }
  }
  putInstallRecord(
    buildInstallRecord({
      job,
      method,
      primaryNames,
      before,
      after,
      nowIso: new Date().toISOString(),
      unknownNote,
    }),
  );
}
