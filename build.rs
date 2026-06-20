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

/// Stamp the crate with the real release version (the git tag) via
/// `PECKBOARD_VERSION`, so `env!("PECKBOARD_VERSION")` reports e.g. `0.0.19`
/// instead of the `Cargo.toml` value. The two had drifted — releases are
/// git-tagged `0.0.x` while `Cargo.toml` reads `0.1.0` — which made the
/// plugin-compatibility check (`registry::peckboard_version`) compare against
/// the wrong number. Resolution order:
///   1. `PECKBOARD_VERSION` env override (a release pipeline can pin the tag).
///   2. `git describe --tags` — release builds check out the tag, so this is
///      exactly the tag (`0.0.19`); dev builds get `0.0.19-N-gSHA`.
///   3. `CARGO_PKG_VERSION` as a last resort (loud, since it's the drift).
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
    let cargo = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    println!(
        "cargo:warning=PECKBOARD_VERSION: no git tag or env override found; falling back to \
         Cargo version {cargo}. The binary will report this instead of the release tag."
    );
    cargo
}

/// `git describe --tags` (no `v` prefix on this repo's tags), or `None` when
/// git/the tag isn't available (e.g. a source tarball with no `.git`).
fn git_described_version() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags"])
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
