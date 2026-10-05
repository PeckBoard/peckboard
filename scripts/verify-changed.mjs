#!/usr/bin/env node
/**
 * Plans a proportional `scripts/verify.sh --changed` run.
 *
 *   node scripts/verify-changed.mjs [base-ref]   # default base: origin/main
 *
 * Classifies every changed build input (commits since <base> + the working
 * tree + untracked files) and prints shell assignments verify.sh `eval`s:
 *
 *   SCOPE=none|web|rust|full
 *   RUST_CHANGED=0|1  WEB_CHANGED=0|1
 *   INTEGRATION_TESTS='a b'   # tests/<name>.rs to run on top of --lib
 *   WEB_FILES='src/x.tsx …'   # changed web files, relative to web/
 *
 * SCOPE=full means "run the whole Definition of Done": anything that
 * changes the schema, the build, the dependency graph, or the harness
 * every test leans on. The rationale goes to stderr.
 *
 * This narrows only what verify.sh runs locally. It is a heuristic, not a
 * proof — see "Proportional Verify" in AGENTS.md for when the full suite
 * is still required.
 */
import { execFileSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import path from "node:path";

const repoRoot = path.resolve(
  path.dirname(new URL(import.meta.url).pathname),
  "..",
);
const base = process.argv[2] || "origin/main";

// Same whitelist idea as e2e-impacted.mjs: only files something is BUILT
// from count, so untracked scratch in the repo root can't widen the run.
const RELEVANT = [
  /^src\//,
  /^tests\//,
  /^peck-plugins\/[^/]+\/(src\/|Cargo\.toml$|build\.rs$)/,
  /^peck-plugins-wasm\//,
  /^peckboard-agent-protocol\//,
  // The relay/tunnel crate: its own tests + the box that links it.
  /^peckboard-relay\/(src\/|tests\/|examples\/|Cargo\.(toml|lock)$|build\.rs$)/,
  /^web\/src\//,
  /^web\/public\//,
  /^web\/e2e\//,
  /^migrations\//,
  /^Cargo\.(toml|lock)$/,
  /^build\.rs$/,
  /^\.cargo\//,
  /^web\/(index\.html|package(-lock)?\.json|vite\.config\.ts|tsconfig[^/]*\.json|eslint\.config\.[cm]?js)$/,
];

// Schema, build, dependency graph, crate roots, shared test harness.
const FULL = [
  /^migrations\//,
  /^src\/db\//,
  /^src\/schema\.rs$/,
  /^src\/(main|lib)\.rs$/,
  /^build\.rs$/,
  /^\.cargo\//,
  /^Cargo\.(toml|lock)$/,
  /^peck-plugins\/[^/]+\/(Cargo\.toml|build\.rs)$/,
  // Prebuilt plugin blobs are include_bytes!'d / rust-embedded.
  /^peck-plugins-wasm\//,
  /^peckboard-agent-protocol\//,
  /^tests\/common\//,
  /^web\/(index\.html|package(-lock)?\.json|vite\.config\.ts|tsconfig[^/]*\.json|eslint\.config\.[cm]?js)$/,
];

const RUST = [/^src\//, /^tests\//, /^peck-plugins\//, /^peckboard-relay\//];
const RELAY = [/^peckboard-relay\//];
const WEB = [/^web\//];

const git = (args) => {
  try {
    return execFileSync("git", args, {
      cwd: repoRoot,
      encoding: "utf8",
      maxBuffer: 256 * 1024 * 1024,
      stdio: ["ignore", "pipe", "ignore"],
    })
      .split("\n")
      .filter(Boolean);
  } catch {
    return null;
  }
};

const since = git(["diff", "--name-only", `${base}...HEAD`]);
if (since === null) {
  console.error(`verify --changed: base ref ${base} not found — running full`);
  console.log("SCOPE=full");
  process.exit(0);
}
// A release's own version bump (Cargo.toml `version = …` plus the matching
// Cargo.lock entry) changes no code; don't let it escalate every release to
// the full suite. Any other manifest line still does.
const mergeBase = git(["merge-base", base, "HEAD"])?.[0];
const versionBumpOnly = (file) => {
  const diff = mergeBase && git(["diff", "-U0", mergeBase, "--", file]);
  if (!diff) return false;
  const edits = diff.filter(
    (l) => /^[+-]/.test(l) && !/^(\+\+\+|---) /.test(l),
  );
  return (
    edits.length > 0 && edits.every((l) => /^[+-]version = "[^"]*"$/.test(l))
  );
};

const changed = [
  ...new Set([
    ...since,
    ...(git(["diff", "--name-only", "HEAD"]) ?? []),
    ...(git(["ls-files", "--others", "--exclude-standard"]) ?? []),
  ]),
].filter(
  // Dot-dirs (editor/agent config like web/e2e/.cursor/) are never built.
  (f) =>
    RELEVANT.some((re) => re.test(f)) &&
    !/(^|\/)\./.test(f.replace(/^\.cargo\//, "")) &&
    !(/^Cargo\.(toml|lock)$/.test(f) && versionBumpOnly(f)),
);

const q = (xs) => `'${xs.join(" ")}'`;
const summarise = (xs, n = 6) =>
  xs.length <= n
    ? xs.join(", ")
    : `${xs.slice(0, n).join(", ")} (+${xs.length - n} more)`;

if (!changed.length) {
  console.error(`verify --changed: no build inputs changed vs ${base}`);
  console.log("SCOPE=none");
  process.exit(0);
}

const full = changed.filter((f) => FULL.some((re) => re.test(f)));
const rust = changed.filter((f) => RUST.some((re) => re.test(f)));
const web = changed.filter((f) => WEB.some((re) => re.test(f)));
const relay = changed.filter((f) => RELAY.some((re) => re.test(f)));

// Integration tests: a changed tests/<name>.rs selects itself; a changed
// source file selects every tests/*.rs whose name mentions one of its path
// segments (src/service/tts/kokoro.rs → *tts*, *kokoro*). Short or generic
// segments are skipped — they'd match everything and prove nothing.
const GENERIC = new Set([
  "src",
  "mod",
  "lib",
  "main",
  "tests",
  "routes",
  "service",
  "util",
  "utils",
  "types",
  "peck",
  "plugins",
  "plugin",
  "peckboard",
]);
const testsDir = path.join(repoRoot, "tests");
const allTests = existsSync(testsDir)
  ? readdirSync(testsDir)
      .filter((f) => f.endsWith(".rs"))
      .map((f) => f.slice(0, -3))
  : [];
const integration = new Set();
for (const f of rust) {
  const self = /^tests\/([^/]+)\.rs$/.exec(f);
  if (self) {
    if (existsSync(path.join(repoRoot, f))) integration.add(self[1]);
    continue;
  }
  const tokens = f
    .replace(/\.rs$/, "")
    .split(/[/_.-]/)
    .filter((t) => t.length >= 4 && !GENERIC.has(t));
  for (const t of allTests)
    if (tokens.some((tok) => t.includes(tok))) integration.add(t);
}

let scope = "web";
if (full.length) scope = "full";
else if (rust.length) scope = "rust";

console.error(
  `verify --changed: scope=${scope} for ${changed.length} changed inputs vs ${base} (${summarise(changed)})` +
    (full.length ? `\n  full because: ${summarise(full)}` : ""),
);
console.log(`SCOPE=${scope}`);
console.log(`RUST_CHANGED=${rust.length ? 1 : 0}`);
console.log(`WEB_CHANGED=${web.length ? 1 : 0}`);
console.log(`RELAY_CHANGED=${relay.length ? 1 : 0}`);
console.log(`INTEGRATION_TESTS=${q([...integration].sort())}`);
// Relative to web/, only files that still exist (deletions have nothing to
// lint), and only what eslint / prettier understand.
console.log(
  `WEB_FILES=${q(
    web
      .filter(
        (f) =>
          /\.(tsx?|jsx?|mjs|cjs|css|html|json|md)$/.test(f) &&
          !/[\s'"]/.test(f),
      )
      .filter((f) => existsSync(path.join(repoRoot, f)))
      .map((f) => f.slice("web/".length)),
  )}`,
);
