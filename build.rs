//! Build-time checks for the migrations directory.
//!
//! Three jobs:
//!   1. `cargo:rerun-if-changed=migrations` — otherwise cargo has no
//!      reason to re-invoke the `embed_migrations!()` proc macro when a
//!      SQL file changes, and you can silently ship a binary with stale
//!      migrations.
//!   2. `cargo:rerun-if-changed=web/dist` — same hazard for the embedded
//!      frontend. `src/frontend.rs` embeds `web/dist/` via rust-embed at
//!      compile time, but a frontend-only change touches no `.rs` file,
//!      so without this cargo skips recompilation and the binary keeps
//!      serving stale assets (e2e then runs the rebuilt frontend's tests
//!      against the OLD UI and fails confusingly).
//!   3. Reject duplicate version prefixes. Diesel keys migration runs
//!      by version (the numeric prefix before the first underscore),
//!      so two directories with the same number — e.g.
//!      `00000000000002_user_tabs` and `00000000000002_worker_comm` —
//!      collide: diesel records the version as applied after running
//!      ONE of them, and the other silently never runs. We just lived
//!      this bug. Fail the build instead.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// Stamp the crate with the release version via `PECKBOARD_VERSION`, so
/// `env!("PECKBOARD_VERSION")` reports e.g. `0.0.192`. Since 0.0.192 the
/// `Cargo.toml` version IS the source of truth (bumped on every release, and
/// release-promote.yml refuses a tag that disagrees with it) — necessary
/// because release binaries are built on the MAIN push (build-main.yml),
/// before the tag exists, where `actions/checkout`'s shallow clone carries no
/// tags at all. The 0.0.191 binaries shipped stamped `0.1.0` for exactly that
/// reason, which broke their self-update (`0.1.0` compares newer than every
/// real release). Resolution order:
///   1. `PECKBOARD_VERSION` env override (a release pipeline can pin a tag).
///   2. `git describe --tags --exact-match` — a backstop rebuild on the tag
///      ref itself (the dispatch-only build-*.yml workflows) stamps the tag.
///   3. `CARGO_PKG_VERSION` — the maintained source of truth.
fn stamp_version() {
    println!("cargo:rerun-if-env-changed=PECKBOARD_VERSION");
    // Re-resolve when HEAD moves or tags change.
    for p in [".git/HEAD", ".git/refs/tags", ".git/packed-refs"] {
        println!("cargo:rerun-if-changed={p}");
    }
    let version = resolve_version();
    println!("cargo:rustc-env=PECKBOARD_VERSION={version}");
}

fn resolve_version() -> String {
    if let Ok(v) = std::env::var("PECKBOARD_VERSION") {
        let v = v.trim().trim_start_matches('v');
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Some(v) = git_described_version() {
        return v;
    }
    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into())
}

/// `git describe --tags --exact-match` (no `v` prefix on this repo's tags):
/// the checked-out commit's own tag, or `None` when HEAD isn't tagged —
/// including every on-main build, whose shallow clone has no tags. Never the
/// suffixed `describe` form (`0.0.191-3-gSHA`): a suffixed stamp would
/// compare against releases unpredictably, and `Cargo.toml` is the right
/// answer for untagged builds.
fn git_described_version() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--exact-match"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8(out.stdout).ok()?;
    let v = v.trim().trim_start_matches('v');
    (!v.is_empty()).then(|| v.to_string())
}

fn main() {
    stamp_version();

    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=web/dist");

    let dir = Path::new("migrations");
    if !dir.is_dir() {
        return;
    }

    // Group migration directory names by their version prefix (the
    // part before the first underscore).
    let mut by_version: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            println!("cargo:warning=could not read migrations/: {e}");
            return;
        }
    };

    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let version = name
            .split_once('_')
            .map(|(v, _)| v)
            .unwrap_or(&name)
            .to_string();
        by_version.entry(version).or_default().push(name);
    }

    let dupes: Vec<(String, Vec<String>)> = by_version
        .into_iter()
        .filter(|(_, names)| names.len() > 1)
        .collect();

    if !dupes.is_empty() {
        for (version, names) in &dupes {
            println!("cargo:warning=Duplicate migration version {version}: {names:?}");
        }
        panic!(
            "duplicate migration version(s) detected: {:?}. \
             Rename so each migration has a unique numeric prefix. \
             See AGENTS.md \"Migrations\".",
            dupes,
        );
    }
}
