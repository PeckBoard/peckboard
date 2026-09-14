#!/usr/bin/env node
/**
 * Builds `web/e2e/impact-map.json` from one instrumented e2e run.
 *
 * Input: the directory `scripts/e2e-impact-map.sh` pointed the run at,
 * containing
 *   frontend-<shard>.jsonl {spec, sources[]}         — per test, from web/e2e/harness.ts
 *   timing-<shard>.jsonl   {spec, shard, start, end} — per test, from web/e2e/impact/reporter.ts
 *   routes-<shard>.jsonl {t, route}              — per request, from src/impact_log.rs
 *
 * Output: { "<repo-relative source file>": ["tests/foo.spec.ts", ...] }
 *
 * Two halves, because the two sides of the app leave different evidence:
 *
 *  - Frontend: V8 coverage traced through the bundle sourcemap gives the
 *    exact `web/src/**` modules a spec executed.
 *  - Backend: the server logs the route PATTERN of every request with a
 *    timestamp; joining that against each test's wall-clock window (safe:
 *    `workers: 1`, one server per shard, so one test in flight at a time)
 *    gives the routes a spec hit. Route patterns resolve to the
 *    `src/routes/**` file that registers them, and from there we walk
 *    `crate::` references transitively so a change deep in
 *    `src/service/**` or `src/provider/**` still names the specs that can
 *    reach it.
 *
 * The Rust half is deliberately over-inclusive. Selecting a few extra
 * specs costs seconds; missing one hides a regression.
 */
import {
  readFileSync,
  readdirSync,
  writeFileSync,
  existsSync,
  statSync,
} from "node:fs";
import path from "node:path";

const impactDir = process.argv[2];
if (!impactDir) {
  console.error("usage: e2e-impact-map.mjs <impact-dir>");
  process.exit(2);
}
const repoRoot = path.resolve(
  path.dirname(new URL(import.meta.url).pathname),
  "..",
);

const readJsonl = (file) => {
  if (!existsSync(file)) return [];
  return readFileSync(file, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch {
        return null;
      }
    })
    .filter(Boolean);
};

// spec -> Set(source file)
const map = new Map();
const add = (spec, file) => {
  if (!map.has(spec)) map.set(spec, new Set());
  map.get(spec).add(file);
};

// ── frontend: V8 coverage ─────────────────────────────────────────────
// One file per shard — separate processes appending to a single JSONL can
// interleave a long line into an unparseable record.
const shardFiles = (prefix) =>
  readdirSync(impactDir)
    .filter((name) => name.startsWith(`${prefix}-`) && name.endsWith(".jsonl"))
    .map((name) => path.join(impactDir, name));
const frontend = shardFiles("frontend").flatMap(readJsonl);
for (const rec of frontend) {
  for (const source of rec.sources) add(rec.spec, source);
}

// ── frontend, part two: specs that import app source directly ─────────
// A handful of specs are pure-logic tests (no page, no server) that
// import `web/src/**` helpers straight in. They leave no V8 coverage, so
// the pass above never sees them — but they DO have real static import
// edges, which is the one case where static analysis works here.
const resolveImport = (fromFile, specifier) => {
  if (!specifier.startsWith(".")) return null;
  const base = path.resolve(path.dirname(fromFile), specifier);
  const candidates = [
    base,
    `${base}.ts`,
    `${base}.tsx`,
    path.join(base, "index.ts"),
    path.join(base, "index.tsx"),
  ];
  for (const candidate of candidates) {
    if (existsSync(candidate) && statSync(candidate).isFile()) {
      const rel = path.relative(repoRoot, candidate);
      if (rel.startsWith("web/src/")) return rel;
    }
  }
  return null;
};
const importsOf = (absFile) => {
  const out = [];
  const text = readFileSync(absFile, "utf8");
  for (const m of text.matchAll(/from\s+["']([^"']+)["']/g)) {
    const resolved = resolveImport(absFile, m[1]);
    if (resolved) out.push(resolved);
  }
  return out;
};
const e2eDir = path.join(repoRoot, "web", "e2e");
for (const name of readdirSync(path.join(e2eDir, "tests"))) {
  if (!name.endsWith(".spec.ts")) continue;
  const spec = path.join("tests", name);
  // Walk transitively: a spec importing `util/cost` should also re-run
  // when something `util/cost` itself imports changes.
  const seen = new Set(importsOf(path.join(e2eDir, "tests", name)));
  const queue = [...seen];
  while (queue.length) {
    for (const next of importsOf(path.join(repoRoot, queue.pop()))) {
      if (!seen.has(next)) {
        seen.add(next);
        queue.push(next);
      }
    }
  }
  for (const file of seen) add(spec, file);
}

// ── rust: every .rs file, and what each one references ────────────────
const rustFiles = [];
const walk = (dir) => {
  for (const entry of readdirSync(dir)) {
    const full = path.join(dir, entry);
    if (statSync(full).isDirectory()) walk(full);
    else if (entry.endsWith(".rs"))
      rustFiles.push(path.relative(repoRoot, full));
  }
};
walk(path.join(repoRoot, "src"));

/** `crate::a::b::c` -> the longest existing `src/a/b.rs` | `src/a/b/mod.rs`. */
const moduleFile = (segments) => {
  for (let n = segments.length; n > 0; n--) {
    const base = path.join("src", ...segments.slice(0, n));
    for (const candidate of [`${base}.rs`, path.join(base, "mod.rs")]) {
      if (existsSync(path.join(repoRoot, candidate))) return candidate;
    }
  }
  return null;
};

// file -> Set(file it references). Approximated from `crate::…` paths,
// which is how this codebase names everything outside the current module.
const refs = new Map();
const routeOwners = new Map(); // route pattern -> file registering it
for (const file of rustFiles) {
  const text = readFileSync(path.join(repoRoot, file), "utf8");
  const out = new Set();
  for (const m of text.matchAll(/crate::([a-z0-9_]+(?:::[a-z0-9_]+)*)/g)) {
    const target = moduleFile(m[1].split("::"));
    if (target && target !== file) out.add(target);
  }
  refs.set(file, out);
  for (const m of text.matchAll(/\.route\("([^"]+)"/g)) {
    if (!routeOwners.has(m[1])) routeOwners.set(m[1], file);
  }
}

/** Every file reachable from `start` by following `crate::` references. */
const reachableFrom = (start) => {
  const seen = new Set([start]);
  const queue = [start];
  while (queue.length) {
    for (const next of refs.get(queue.pop()) ?? []) {
      if (!seen.has(next)) {
        seen.add(next);
        queue.push(next);
      }
    }
  }
  return seen;
};
const reachCache = new Map();

// ── backend: route log joined to test windows by wall clock ───────────
const timings = shardFiles("timing").flatMap(readJsonl);
const byShard = new Map();
for (const t of timings) {
  if (!byShard.has(t.shard)) byShard.set(t.shard, []);
  byShard.get(t.shard).push(t);
}

let unmatchedRoutes = 0;
for (const [shard, windows] of byShard) {
  windows.sort((a, b) => a.start - b.start);
  const hits = readJsonl(path.join(impactDir, `routes-${shard}.jsonl`));
  for (const hit of hits) {
    // Windows are disjoint and sorted; binary-search the one containing t.
    let lo = 0;
    let hi = windows.length - 1;
    let found = null;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (hit.t < windows[mid].start) hi = mid - 1;
      else if (hit.t > windows[mid].end) lo = mid + 1;
      else {
        found = windows[mid];
        break;
      }
    }
    if (!found) {
      // Server boot, the repeating-task sweep, teardown — nothing owns it.
      unmatchedRoutes++;
      continue;
    }
    const owner = routeOwners.get(hit.route);
    if (!owner) continue;
    if (!reachCache.has(owner)) reachCache.set(owner, reachableFrom(owner));
    for (const file of reachCache.get(owner)) add(found.spec, file);
  }
}

// ── invert to source file -> specs ────────────────────────────────────
const bySource = {};
for (const [spec, sources] of map) {
  for (const source of sources) {
    (bySource[source] ??= []).push(spec);
  }
}
for (const key of Object.keys(bySource))
  bySource[key] = [...new Set(bySource[key])].sort();

const outPath = path.join(repoRoot, "web", "e2e", "impact-map.json");
writeFileSync(
  outPath,
  `${JSON.stringify(bySource, Object.keys(bySource).sort(), 2)}\n`,
);

const specs = new Set(Object.values(bySource).flat());
console.log(
  `impact map: ${Object.keys(bySource).length} source files -> ${specs.size} specs`,
);
console.log(`  frontend coverage records: ${frontend.length}`);
console.log(`  test windows: ${timings.length}`);
console.log(
  `  route hits outside any test window (ignored): ${unmatchedRoutes}`,
);
console.log(`  written to ${path.relative(repoRoot, outPath)}`);
if (!frontend.length) {
  console.error(
    "WARNING: no frontend coverage — was the bundle built with PECKBOARD_E2E_COVERAGE=1?",
  );
}
