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

fn main() {
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
