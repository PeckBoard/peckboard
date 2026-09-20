#!/usr/bin/env node
/**
 * Picks the e2e spec files a change can plausibly affect.
 *
 * Prints one spec path per line on stdout, or the single line `ALL` when
 * the change is too broad (or too unknown) to narrow safely. The rationale
 * goes to stderr so it is always visible without polluting the list.
 *
 *   node scripts/e2e-impacted.mjs            # working tree vs HEAD
 *   node scripts/e2e-impacted.mjs origin/main  # vs a base ref
 *
 * This is an INNER-LOOP tool. `scripts/verify.sh` always runs the full
 * suite; a stale map must never be able to hide a regression from a merge.
 * See scripts/e2e-impact-map.sh for how the map is regenerated.
 */
import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";

const repoRoot = path.resolve(
  path.dirname(new URL(import.meta.url).pathname),
  "..",
);
const mapPath = path.join(repoRoot, "web", "e2e", "impact-map.json");

/**
 * Only files the app or the suite is actually BUILT from can affect a
 * spec: Rust sources (including the first-party plugin crates, which
 * compile natively into the release binary Playwright boots), the web
 * bundle's inputs, the e2e suite itself, and the build manifests.
 * Everything else — scratch logs in the repo root, docs, graphify caches,
 * scripts (never compiled in or served) — is irrelevant by construction.
 * A whitelist, not a blacklist: untracked junk must never be able to
 * force a full run by being unclassifiable.
 */
const RELEVANT = [
  /^src\//,
  /^peck-plugins\/[^/]+\/(src\/|Cargo\.toml$)/,
  /^web\/src\//,
  /^web\/e2e\//,
  /^migrations\//,
  /^Cargo\.(toml|lock)$/,
  /^build\.rs$/,
  /^web\/(index\.html|package(-lock)?\.json|vite\.config\.ts|tsconfig[^/]*\.json)$/,
];

/**
 * Changing any of these invalidates the map's assumptions wholesale — the
 * harness itself, the app shell every screen mounts inside, the shared
 * primitives CLAUDE.md requires every view to reuse, the schema, or the
 * build. Narrowing here is exactly where a missed regression would hide.
 * (Spec files are handled separately below — a changed spec selects
 * itself, so this e2e pattern only catches the harness/config/impact
 * plumbing shared by every spec.)
 */
const ALWAYS_ALL = [
  /^web\/e2e\/(?!tests\/)/,
  /^web\/src\/(main|App)\.tsx$/,
  /^web\/src\/components\/(Modal|List|ListViewHeader|Dropdown|ConfirmDialog|FieldError)\.tsx$/,
  /^web\/(index\.html|package(-lock)?\.json|vite\.config\.ts|tsconfig[^/]*\.json)$/,
  /^src\/(main|lib|server|state|frontend)\.rs$/,
  /^src\/db\//,
  /^migrations\//,
  /^Cargo\.(toml|lock)$/,
  /^build\.rs$/,
];

const git = (args) =>
  execFileSync("git", args, { cwd: repoRoot, encoding: "utf8" })
    .split("\n")
    .filter(Boolean);

const base = process.argv[2];
const changed = [
  ...(base ? git(["diff", "--name-only", `${base}...HEAD`]) : []),
  ...git(["diff", "--name-only", "HEAD"]),
  ...git(["ls-files", "--others", "--exclude-standard"]),
];

const relevant = [...new Set(changed)].filter((f) =>
  RELEVANT.some((re) => re.test(f)),
);

// A changed spec file impacts exactly itself — the map is source→specs and
// has nothing to add. Split them out so one edited spec runs one spec, not
// the whole suite. (Deleted specs would no longer exist on disk; filter.)
const specSelf = [];
const sources = [];
for (const f of relevant) {
  const m = /^web\/e2e\/(tests\/[^/]+\.spec\.ts)$/.exec(f);
  if (m && existsSync(path.join(repoRoot, f))) specSelf.push(m[1]);
  else sources.push(f);
}

// A rationale naming 190 files is a rationale nobody reads.
const summarise = (files, limit = 6) =>
  files.length <= limit
    ? files.join(", ")
    : `${files.slice(0, limit).join(", ")} (+${files.length - limit} more)`;

const runAll = (why) => {
  console.error(`e2e selection: running EVERYTHING — ${why}`);
  console.log("ALL");
  process.exit(0);
};

if (!relevant.length) {
  console.error("e2e selection: no relevant changes; nothing to run");
  process.exit(0);
}

const broad = sources.filter((f) => ALWAYS_ALL.some((re) => re.test(f)));
if (broad.length) runAll(`shared surface changed: ${summarise(broad)}`);

if (sources.length && !existsSync(mapPath)) {
  runAll(
    `no impact map at ${path.relative(repoRoot, mapPath)} (run scripts/e2e-impact-map.sh)`,
  );
}

const map = sources.length ? JSON.parse(readFileSync(mapPath, "utf8")) : {};
const unmapped = sources.filter((f) => !(f in map));
if (unmapped.length) {
  // A source file no spec ever executed is either untested or new. Either
  // way, guessing "nothing to run" is the one answer that can be wrong in
  // the direction that matters.
  runAll(`no spec is known to exercise: ${summarise(unmapped)}`);
}

const specs = [
  ...new Set([...sources.flatMap((f) => map[f]), ...specSelf]),
].sort();
console.error(
  `e2e selection: ${specs.length} spec files for ${relevant.length} changed files ` +
    `(${summarise(relevant)})`,
);
for (const spec of specs) console.log(spec);
