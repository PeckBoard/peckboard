//! Execution tools: `git` and `run_tests`.
//!
//! Both shell out through the host's `peckboard_exec`, which only runs an
//! allowlisted executable, passes args as an argv array (no shell), and pins
//! the working directory to the caller's project folder. On top of that the
//! `git` tool restricts itself to **read-only** subcommands, and `run_tests`
//! only ever invokes a known test runner.

use super::host_bridge::{HostCtx, HostFn};

/// Read-only git subcommands the tool will run. Anything that mutates the repo
/// or its objects (commit/push/reset/checkout/merge/clean/add/rm/…) is absent
/// by design, so the tool can inspect a repo but never change it.
const GIT_READONLY: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "branch",
    "blame",
    "ls-files",
    "ls-tree",
    "rev-parse",
    "rev-list",
    "describe",
    "shortlog",
    "tag",
    "remote",
    "for-each-ref",
    "cat-file",
    "name-rev",
    "symbolic-ref",
    "whatchanged",
    "reflog",
];

pub fn git_tool(args: serde_json::Value, ctx: &HostCtx) -> Result<serde_json::Value, String> {
    let subcommand = args
        .get("subcommand")
        .and_then(|v| v.as_str())
        .ok_or("`subcommand` (string) is required")?
        .trim()
        .to_string();
    if !GIT_READONLY.contains(&subcommand.as_str()) {
        return Err(format!(
            "git subcommand '{subcommand}' is not permitted (read-only only); allowed: {}",
            GIT_READONLY.join(", ")
        ));
    }

    let mut argv = vec![subcommand.clone()];
    if let Some(extra) = args.get("args").and_then(|v| v.as_array()) {
        for a in extra {
            match a.as_str() {
                Some(s) => argv.push(s.to_string()),
                None => return Err("each entry in `args` must be a string".to_string()),
            }
        }
    }

    let mut req = serde_json::json!({ "command": "git", "args": argv });
    if let Some(t) = args.get("timeout_secs").and_then(|v| v.as_u64()) {
        req["timeout_secs"] = serde_json::json!(t);
    }
    let mut result = ctx.call_host(HostFn::Exec, &req)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "command".to_string(),
            serde_json::json!(format!("git {}", argv.join(" "))),
        );
    }
    Ok(result)
}

/// A detected/declared test runner: the executable plus its base arguments.
struct Runner {
    name: &'static str,
    command: &'static str,
    base_args: Vec<&'static str>,
}

pub fn run_tests_tool(args: serde_json::Value, ctx: &HostCtx) -> Result<serde_json::Value, String> {
    let requested = args
        .get("runner")
        .and_then(|v| v.as_str())
        .unwrap_or("auto");

    let runner = match requested {
        "auto" => detect_runner(ctx)?,
        other => named_runner(other)
            .ok_or_else(|| format!("unknown runner '{other}'; use auto | cargo | npm | pytest | go | gradle | maven | rspec | phpunit"))?,
    };

    let mut argv: Vec<String> = runner.base_args.iter().map(|s| s.to_string()).collect();
    if let Some(extra) = args.get("args").and_then(|v| v.as_array()) {
        for a in extra {
            match a.as_str() {
                Some(s) => argv.push(s.to_string()),
                None => return Err("each entry in `args` must be a string".to_string()),
            }
        }
    }

    let timeout = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(300);
    let req = serde_json::json!({
        "command": runner.command,
        "args": argv,
        "timeout_secs": timeout,
    });
    let mut result = ctx.call_host(HostFn::Exec, &req)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("runner".to_string(), serde_json::json!(runner.name));
        obj.insert(
            "command".to_string(),
            serde_json::json!(format!("{} {}", runner.command, argv.join(" "))),
        );
        // A convenience pass/fail read for the agent.
        let passed = obj
            .get("exit_code")
            .and_then(|c| c.as_i64())
            .map(|c| c == 0)
            .unwrap_or(false);
        obj.insert("passed".to_string(), serde_json::json!(passed));
    }
    Ok(result)
}

fn named_runner(name: &str) -> Option<Runner> {
    Some(match name {
        "cargo" => Runner {
            name: "cargo",
            command: "cargo",
            base_args: vec!["test"],
        },
        "npm" => Runner {
            name: "npm",
            command: "npm",
            base_args: vec!["test"],
        },
        "pnpm" => Runner {
            name: "pnpm",
            command: "pnpm",
            base_args: vec!["test"],
        },
        "yarn" => Runner {
            name: "yarn",
            command: "yarn",
            base_args: vec!["test"],
        },
        "pytest" => Runner {
            name: "pytest",
            command: "pytest",
            base_args: vec![],
        },
        "go" => Runner {
            name: "go",
            command: "go",
            base_args: vec!["test", "./..."],
        },
        "gradle" => Runner {
            name: "gradle",
            command: "gradle",
            base_args: vec!["test"],
        },
        "maven" => Runner {
            name: "maven",
            command: "mvn",
            base_args: vec!["test"],
        },
        "rspec" => Runner {
            name: "rspec",
            command: "rspec",
            base_args: vec![],
        },
        "phpunit" => Runner {
            name: "phpunit",
            command: "phpunit",
            base_args: vec![],
        },
        "dotnet" => Runner {
            name: "dotnet",
            command: "dotnet",
            base_args: vec!["test"],
        },
        _ => return None,
    })
}

/// Auto-detect the test runner from the marker files at the project root.
fn detect_runner(ctx: &HostCtx) -> Result<Runner, String> {
    let resp = ctx.call_host(HostFn::ListProjectFiles, &serde_json::json!({}))?;
    let empty = vec![];
    let files = resp["files"].as_array().unwrap_or(&empty);

    // Build a set of top-level (no '/') file names for marker checks.
    let mut roots = std::collections::HashSet::new();
    let mut any_py = false;
    for f in files {
        if let Some(p) = f["path"].as_str() {
            if !p.contains('/') {
                roots.insert(p.to_string());
            }
            if p.ends_with(".py") {
                any_py = true;
            }
        }
    }
    let has = |n: &str| roots.contains(n);

    let pick = if has("Cargo.toml") {
        "cargo"
    } else if has("go.mod") {
        "go"
    } else if has("package.json") {
        "npm"
    } else if has("pyproject.toml")
        || has("setup.py")
        || has("pytest.ini")
        || has("tox.ini")
        || any_py
    {
        "pytest"
    } else if has("Gemfile") || has(".rspec") {
        "rspec"
    } else if has("pom.xml") {
        "maven"
    } else if has("build.gradle") || has("build.gradle.kts") {
        "gradle"
    } else if has("composer.json") {
        "phpunit"
    } else {
        return Err(
            "could not auto-detect a test runner; pass `runner` explicitly (cargo | npm | pytest | go | gradle | maven | rspec | phpunit)"
                .to_string(),
        );
    };
    named_runner(pick).ok_or_else(|| "internal: unknown detected runner".to_string())
}
