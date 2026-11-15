//! The reachability half of the rename machinery, kept here because the layout invariants
//! need it in a product. The rename logic itself — `rename::app`, `rename::org`, the
//! spellings and the substitution — lives in the CLI crate; products do not carry it.
//!
//! What stays is the walk that a path invariant goes through, and the three lists of files
//! outside `apps/` that may name an app path. `app_paths_outside_apps_are_reachable` reads
//! them to decide whether a reference a rename would have to reach is actually reachable.

use std::path::{Path, PathBuf};

use crate::repo;

/// Directories whose contents are generated, vendored, or not ours — matched by name.
pub fn is_skipped(path: &Path) -> bool {
    const SKIP: &[&str] = &[
        ".git",
        "target",
        "generated",
        "gen",
        "build",
        "dist",
        ".gradle",
        ".android",
        ".devenv",
        "node_modules",
    ];
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIP.contains(&name))
}

/// Every file under `dir`, skipping build output and nested checkouts.
pub fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if is_skipped(&path) {
            continue;
        }
        if path.is_dir() {
            if repo::is_nested_checkout(&path) {
                continue;
            }
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Files where the app's *path* may be rewritten. The CLI's own `rename.rs` carries the
/// authoritative list; this is the product's subset — the same files, minus the ones that
/// moved to the CLI (`skeletons.rs`) and the ones a product does not carry.
pub const PROSE: &[&str] = &["README.md", "AGENTS.md", "Cargo.toml"];

pub const PATHS_ONLY: &[&str] = &[
    ".forgejo/workflows/ci.yaml",
    "crates/identity/README.md",
    "crates/identity/src/lib.rs",
    "services/api/tests/api/auth.rs",
    "tools/xtask/src/i18n_resources.rs",
    "tools/xtask/src/gates.rs",
    "tools/cli/src/skeletons.rs",
];

pub const PATHS_ONLY_TREES: &[&str] = &["templates", "docs/adr", ".claude/skills"];
