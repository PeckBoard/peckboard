//! Pure policy planning: which paths an agent process gets which access to.
//!
//! Landlock is allow-list only — a right granted on a directory covers its
//! whole subtree and cannot be carved back out. "Everything readable except
//! the data dir" therefore cannot be one rule on `/`. Instead [`plan`] walks
//! from `/` down to each denied path: every ancestor of a denied path gets
//! [`Access::ListOnly`] (it can be listed, nothing in it can be read or
//! created), and every sibling along the way gets its normal access. Denied
//! trees get no rule at all; explicit exceptions inside them (the session's
//! own account dir, its own MCP config file) get their own narrow rules.
//!
//! Kept free of syscalls so it is testable on every platform.

use std::path::{Path, PathBuf};

/// The access class a planned rule grants. Mapped to concrete Landlock
/// rights in `linux.rs` (and masked to file-applicable rights for files).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read + execute files, list directories.
    ReadOnly,
    /// Everything: read, write, create, delete, rename within.
    ReadWrite,
    /// List the directory only — an ancestor of a denied path.
    ListOnly,
    /// The device tree: read/write/truncate existing nodes, create nothing.
    Devices,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub path: PathBuf,
    pub access: Access,
}

/// What a spawn's policy is built from. Paths need not be canonical or even
/// exist; [`plan`] canonicalizes and drops the missing ones.
#[derive(Debug, Default, Clone)]
pub struct PolicyInputs {
    /// Trees the agent may not read or write at all (the data dir, Peckboard's
    /// and Tailscale's own config dirs).
    pub denied: Vec<PathBuf>,
    /// Trees (or single files) the agent may write.
    pub rw: Vec<PathBuf>,
    /// Single files the agent may read even inside a denied tree.
    pub ro_files: Vec<PathBuf>,
    /// Files that must never be writable (the server binary). Only matter
    /// when they sit under a writable root; elsewhere they are read-only
    /// already.
    pub protected: Vec<PathBuf>,
    /// The device tree (`/dev`) — granted [`Access::Devices`] when present.
    pub devices: Option<PathBuf>,
}

fn canon(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

/// Plan the rule set for `inputs`. `children` lists a directory's entries
/// (the real implementation is [`read_children`]; tests can stub it).
pub fn plan(inputs: &PolicyInputs, children: &dyn Fn(&Path) -> Vec<PathBuf>) -> Vec<Rule> {
    let rw: Vec<PathBuf> = inputs.rw.iter().filter_map(|p| canon(p)).collect();
    let mut denied: Vec<PathBuf> = inputs.denied.iter().filter_map(|p| canon(p)).collect();
    let mut ro_files: Vec<PathBuf> = inputs.ro_files.iter().filter_map(|p| canon(p)).collect();
    for p in inputs.protected.iter().filter_map(|p| canon(p)) {
        if rw.iter().any(|r| p.starts_with(r)) {
            denied.push(p.clone());
            ro_files.push(p);
        }
    }
    denied.sort();
    denied.dedup();

    let under_rw = |p: &Path| rw.iter().any(|r| p.starts_with(r));
    let base = |p: &Path| {
        if under_rw(p) {
            Access::ReadWrite
        } else {
            Access::ReadOnly
        }
    };
    let is_denied = |p: &Path| denied.iter().any(|d| p.starts_with(d));
    let is_ancestor = |p: &Path| denied.iter().any(|d| d != p && d.starts_with(p));

    let mut rules = Vec::new();
    // Iterative walk from `/` along the ancestor chains of denied paths.
    let mut stack = vec![PathBuf::from("/")];
    while let Some(dir) = stack.pop() {
        if is_denied(&dir) {
            continue;
        }
        if !is_ancestor(&dir) {
            rules.push(Rule {
                access: base(&dir),
                path: dir,
            });
            continue;
        }
        rules.push(Rule {
            path: dir.clone(),
            access: Access::ListOnly,
        });
        for child in children(&dir) {
            // Symlinks get no rule of their own: access through one is
            // checked against the target's real location, which the walk
            // covers on its own. Granting the link would grant its target —
            // a `~/link -> ~/.peckboard` would otherwise re-open the data dir.
            match std::fs::symlink_metadata(&child) {
                Ok(md) if md.file_type().is_symlink() => continue,
                Ok(_) => stack.push(child),
                Err(_) => continue,
            }
        }
    }

    // Writable roots. One that contains a denied path was already carved by
    // the walk (its children got ReadWrite as `base`); one that IS denied
    // stays denied. A root inside a denied tree is an explicit exception.
    for r in &rw {
        let contains_denied = denied.iter().any(|d| d.starts_with(r));
        if contains_denied {
            continue;
        }
        rules.push(Rule {
            path: r.clone(),
            access: Access::ReadWrite,
        });
    }
    for f in ro_files {
        rules.push(Rule {
            path: f,
            access: Access::ReadOnly,
        });
    }
    if let Some(dev) = inputs.devices.as_deref().and_then(canon)
        && !is_denied(&dev)
    {
        rules.push(Rule {
            path: dev,
            access: Access::Devices,
        });
    }
    rules
}

/// Real directory listing for [`plan`]. Unreadable directories list empty,
/// so their children stay inaccessible.
pub fn read_children(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access_of(rules: &[Rule], p: &Path) -> Vec<Access> {
        rules
            .iter()
            .filter(|r| r.path == p)
            .map(|r| r.access)
            .collect()
    }

    #[test]
    fn carves_the_data_dir_out_of_a_readable_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let home = root.join("home");
        let data = home.join(".peckboard");
        let acc = data.join("claude-accounts/a");
        let project = home.join("proj");
        let cargo = home.join(".cargo");
        for d in [&acc, &project, &cargo, &home.join("docs")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(data.join("jwt_secret"), "s").unwrap();
        std::fs::write(home.join("peckboard-bin"), "x").unwrap();
        std::os::unix::fs::symlink(&data, home.join("sneaky")).unwrap();

        let inputs = PolicyInputs {
            denied: vec![data.clone()],
            rw: vec![project.clone(), cargo.clone(), acc.clone()],
            ro_files: vec![],
            protected: vec![home.join("peckboard-bin")],
            devices: None,
        };
        let rules = plan(&inputs, &read_children);

        // Every ancestor of the data dir is list-only.
        assert_eq!(access_of(&rules, Path::new("/")), vec![Access::ListOnly]);
        assert_eq!(access_of(&rules, &home), vec![Access::ListOnly]);
        // The data dir and its secrets get no rule.
        assert!(access_of(&rules, &data).is_empty());
        assert!(access_of(&rules, &data.join("jwt_secret")).is_empty());
        // Siblings keep their normal access; writable roots are writable.
        assert_eq!(
            access_of(&rules, &home.join("docs")),
            vec![Access::ReadOnly]
        );
        assert!(access_of(&rules, &project).contains(&Access::ReadWrite));
        assert!(access_of(&rules, &cargo).contains(&Access::ReadWrite));
        // The account dir is an explicit exception inside the data dir.
        assert_eq!(access_of(&rules, &acc), vec![Access::ReadWrite]);
        // The symlink into the data dir is never granted.
        assert!(access_of(&rules, &home.join("sneaky")).is_empty());
        // The binary outside any writable root stays plain read-only.
        assert_eq!(
            access_of(&rules, &home.join("peckboard-bin")),
            vec![Access::ReadOnly]
        );
    }

    #[test]
    fn a_writable_root_that_contains_the_data_dir_is_carved_not_granted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let data = root.join("data");
        let other = root.join("other");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let inputs = PolicyInputs {
            denied: vec![data.clone()],
            rw: vec![root.clone()],
            ..Default::default()
        };
        let rules = plan(&inputs, &read_children);
        assert_eq!(access_of(&rules, &root), vec![Access::ListOnly]);
        assert_eq!(access_of(&rules, &other), vec![Access::ReadWrite]);
        assert!(access_of(&rules, &data).is_empty());
    }

    #[test]
    fn a_protected_file_under_a_writable_root_becomes_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("peckboard");
        std::fs::write(&exe, "x").unwrap();
        std::fs::write(bin.join("other-tool"), "x").unwrap();
        let inputs = PolicyInputs {
            rw: vec![bin.clone()],
            protected: vec![exe.clone()],
            ..Default::default()
        };
        let rules = plan(&inputs, &read_children);
        assert_eq!(access_of(&rules, &bin), vec![Access::ListOnly]);
        assert_eq!(access_of(&rules, &exe), vec![Access::ReadOnly]);
        assert_eq!(
            access_of(&rules, &bin.join("other-tool")),
            vec![Access::ReadWrite]
        );
    }
}
