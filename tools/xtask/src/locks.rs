//! The two locks have to agree about the inputs both of them consume.
//!
//! `flake.lock` is canonical (decision 1): it is what a product carries, what `nix build`
//! reads, and what `nix/template-pin.nix` gets the origin revision from. `devenv.lock` is
//! what the dev shell and CI build with, and it is a *different lock file* — devenv has its
//! own inputs (devenv itself, git-hooks, android-nixpkgs) and its own resolver, so the two
//! cannot be one file.
//!
//! They therefore drift, and the drift is invisible until it bites: `rust-overlay` at two
//! revisions is two toolchains, so the dev shell's clippy and the flake's crane build use
//! different compilers while both are called "the pinned toolchain"; `connectrpc` at two
//! revisions is two sets of buf plugins, so the committed generated code is checked against
//! one and products generate with the other. A dev shell that resolves a different nixpkgs
//! than the flake is the same class of bug with a larger blast radius.
//!
//! `just lock::sync` is the one command that rewrites `devenv.yaml`'s URLs to the revisions
//! `flake.lock` holds; this check is what proves it was run. It runs in `cargo xtask check`,
//! so a lock bump that touches one file and not the other fails with both revisions named.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::Value;

/// The inputs both locks carry, and which therefore have to be at the same revision.
///
/// Named rather than computed as "every input the two have in common": devenv and git-hooks
/// are devenv's alone by design, and the point of this list is that these four are *not*.
pub const SHARED_INPUTS: &[&str] = &["nixpkgs", "crane", "rust-overlay", "connectrpc"];

/// Compare the shared inputs' locked revisions, reporting each disagreement.
///
/// # Errors
///
/// Returns an error only if a lock file exists and cannot be read or parsed. A product that
/// has switched off devenv has no `devenv.lock`, and there is then nothing to compare — that
/// is not a problem, because a second lock is the only thing that could disagree.
pub fn check(root: &Path, checked: &mut usize, problems: &mut Vec<String>) -> Result<()> {
    let flake_path = root.join("flake.lock");
    let devenv_path = root.join("devenv.lock");

    if !devenv_path.is_file() {
        // Devenv removed on purpose (`docs/adr/0026`'s "delete devenv.* and the devenv-only
        // nix/*.nix, and nothing else"). One lock cannot drift from itself.
        *checked += 1;
        return Ok(());
    }

    let flake = read_lock(&flake_path)?;
    let devenv = read_lock(&devenv_path)?;
    *checked += 2;

    for name in SHARED_INPUTS {
        *checked += 1;
        let flake_rev = locked_rev(&flake, name);
        let devenv_rev = locked_rev(&devenv, name);
        match (flake_rev, devenv_rev) {
            (None, None) => {
                problems.push(format!(
                    "{name} is in neither lock — SHARED_INPUTS names it, so either both files \
                     should carry it or the list should not name it"
                ));
            }
            (Some(rev), None) => problems.push(format!(
                "{name} is locked at {rev} in flake.lock but is not a devenv input at all — \
                 the dev shell and the flake cannot agree about an input only one of them has"
            )),
            (None, Some(rev)) => problems.push(format!(
                "{name} is locked at {rev} in devenv.lock but is not a flake input at all — \
                 flake.lock is canonical, so the flake needs the input or devenv does not"
            )),
            (Some(flake_rev), Some(devenv_rev)) if flake_rev != devenv_rev => {
                problems.push(format!(
                    "{name} is locked at two revisions: flake.lock has {flake_rev}, \
                     devenv.lock has {devenv_rev}. flake.lock is canonical — run \
                     `just lock::sync` to point devenv.yaml at the flake's revisions"
                ));
            }
            (Some(_), Some(_)) => {}
        }
    }

    Ok(())
}

fn read_lock(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The `locked.rev` a lock records for a root input, if it has both the input and a revision.
///
/// Resolved through the root node's `inputs` rather than by guessing a node name: a lock file
/// is free to name the node anything (`nixpkgs_2` in `devenv.lock`, because devenv resolves
/// its own nixpkgs first), and only the root's mapping is meaningful.
fn locked_rev(lock: &Value, name: &str) -> Option<String> {
    let root = lock.get("root")?.as_str()?;
    let nodes = lock.get("nodes")?.as_object()?;
    let inputs = nodes.get(root)?.get("inputs")?.as_object()?;
    let node = inputs.get(name)?.as_str()?;
    nodes
        .get(node)?
        .get("locked")?
        .get("rev")?
        .as_str()
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    /// A lock with `inputs` mapped into node names, the way Nix writes one.
    fn lock(root: &str, inputs: &[(&str, &str)]) -> Value {
        let mut nodes = serde_json::Map::new();
        let mut root_inputs = serde_json::Map::new();
        for (name, rev) in inputs {
            // Every node gets its own node name, the way a lock with a repeated input does.
            let node_name = format!("{name}_node");
            nodes.insert(
                node_name.clone(),
                json!({ "locked": { "rev": rev, "type": "github" } }),
            );
            root_inputs.insert((*name).to_owned(), json!(node_name));
        }
        nodes.insert(root.to_owned(), json!({ "inputs": root_inputs }));
        json!({ "nodes": nodes, "root": root, "version": 7 })
    }

    fn repo(flake: &Value, devenv: &Value) -> TempDir {
        let dir = TempDir::new().expect("a temp dir");
        std::fs::write(dir.path().join("flake.lock"), flake.to_string()).expect("write flake.lock");
        std::fs::write(dir.path().join("devenv.lock"), devenv.to_string())
            .expect("write devenv.lock");
        dir
    }

    fn problems(dir: &Path) -> Vec<String> {
        let mut checked = 0;
        let mut problems = Vec::new();
        check(dir, &mut checked, &mut problems).expect("the fixture parses");
        problems
    }

    fn agreeing(rev: &str) -> Value {
        lock(
            "root",
            &SHARED_INPUTS
                .iter()
                .map(|name| (*name, rev))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn two_locks_at_the_same_revisions_are_silent() {
        let dir = repo(&agreeing("aaa"), &agreeing("aaa"));
        assert_eq!(problems(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn a_drifted_input_is_reported_with_both_revisions() {
        let flake = lock(
            "root",
            &[
                ("nixpkgs", "flake-nixpkgs"),
                ("crane", "same"),
                ("rust-overlay", "same"),
                ("connectrpc", "same"),
            ],
        );
        let devenv = lock(
            "root",
            &[
                ("nixpkgs", "devenv-nixpkgs"),
                ("crane", "same"),
                ("rust-overlay", "same"),
                ("connectrpc", "same"),
            ],
        );
        let dir = repo(&flake, &devenv);

        let problems = problems(dir.path());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("flake-nixpkgs") && problems[0].contains("devenv-nixpkgs"),
            "the failure must name both revisions, got: {}",
            problems[0]
        );
        assert!(
            problems[0].contains("just lock::sync"),
            "the failure must say how to fix it, got: {}",
            problems[0]
        );
    }

    #[test]
    fn an_input_only_one_lock_has_is_reported_rather_than_skipped() {
        let flake = lock("root", &[("nixpkgs", "aaa")]);
        let devenv = lock("root", &[("nixpkgs", "aaa"), ("crane", "bbb")]);
        let dir = repo(&flake, &devenv);

        let problems = problems(dir.path());
        assert!(
            problems.iter().any(|p| p.contains("crane")),
            "an input one lock does not have is drift too: {problems:?}"
        );
    }

    #[test]
    fn a_product_without_devenv_has_nothing_to_compare() {
        let dir = TempDir::new().expect("a temp dir");
        std::fs::write(dir.path().join("flake.lock"), agreeing("aaa").to_string())
            .expect("write flake.lock");

        assert_eq!(problems(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn the_node_name_is_read_from_the_root_rather_than_guessed() {
        // `devenv.lock` really does call its nixpkgs node `nixpkgs_2`, because devenv resolves
        // its own nixpkgs first. A check that guessed the node name would fail on a lock that is
        // perfectly consistent.
        let dir = TempDir::new().expect("a temp dir");
        let flake = json!({
            "nodes": {
                "root": { "inputs": { "nixpkgs": "nixpkgs" } },
                "nixpkgs": { "locked": { "rev": "same" } }
            },
            "root": "root"
        });
        let devenv = json!({
            "nodes": {
                "root": { "inputs": { "nixpkgs": "nixpkgs_2" } },
                "nixpkgs": { "locked": { "rev": "someone-elses" } },
                "nixpkgs_2": { "locked": { "rev": "same" } }
            },
            "root": "root"
        });
        std::fs::write(dir.path().join("flake.lock"), flake.to_string()).expect("write flake.lock");
        std::fs::write(dir.path().join("devenv.lock"), devenv.to_string())
            .expect("write devenv.lock");

        // The other three shared inputs are absent from both locks, which is reported; what
        // matters here is that nixpkgs itself is not one of the findings.
        let problems = problems(dir.path());
        assert!(
            !problems
                .iter()
                .any(|p| p.contains("nixpkgs is locked at two")),
            "the root's node mapping must be followed, got: {problems:?}"
        );
    }
}
