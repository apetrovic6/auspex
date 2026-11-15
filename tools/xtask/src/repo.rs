//! Locating the repository and walking it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Directories that never contain anything we want to inspect. Walking into `target/`
/// in particular turns a sub-second check into a multi-second one.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".devenv",
    ".direnv",
    ".gradle",
    ".kotlin",
    ".idea",
    ".android",
    "target",
    "generated",
    "dist",
    "node_modules",
    "build",
];

/// The repository root.
///
/// `CARGO_MANIFEST_DIR` is baked in at compile time and this crate lives at
/// `tools/xtask`, so the root is two levels up. That holds however the binary is invoked
/// — `cargo xtask` from a subdirectory, or the built binary directly — which a
/// cwd-based search would not.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask must live at <repo>/tools/xtask")
        .to_path_buf()
}

/// Whether the tree is a product (made by `cli new`) rather than the template itself.
///
/// The signal is the origin record, and the origin record moved: it is the product's
/// `template` input in the flake (decision 1, `flake.lock` is canonical), not a devenv input.
/// Two spellings, because `cli new --no-pin` writes the input and deliberately skips the
/// network lock:
///
/// - `flake.nix` declares `template.url` — true for every product, locked or not;
/// - `flake.lock` has a root `template` input — true for a locked product, and the node
///   `nix/template-pin.nix` reads the revision out of.
///
/// The template repository has neither: it was not instantiated from anything. A tree that
/// has only the lock node (somebody hand-locked an input named `template`) is still a product
/// as far as every check here is concerned — what they need to know is "this is not the
/// template", and the input is not something the template declares.
pub fn is_product(root: &Path) -> bool {
    crate::recipe::is_product(root)
}

/// Whether `dir` is the root of a **nested checkout** — a second copy of a repository —
/// rather than a directory of this one.
///
/// Git worktrees are the case this was written for: the multi-agent workflow puts them under
/// `.claude/worktrees/` by default (it is the harness's location, not a choice per task) and
/// `git worktree list` still reports them after the branch is merged. A worktree is a
/// complete tree whose own root is elsewhere, so every path invariant that walks it reads
/// the file twice and disagrees with itself: the root `Justfile` and
/// `.claude/worktrees/probe/Justfile` are the same file, and the second one sits at a path no
/// rename list names, which is how RT-33 produced 111 findings about files that were already
/// correct.
///
/// **`exists()`, never `is_dir()`.** A clone's or a submodule's `.git` is a directory; a
/// worktree's is a *file* holding a `gitdir:` pointer to the real gitdir. Written with
/// `is_dir()` this returns true for a clone and false for every worktree — which is the
/// reported bug again, hidden inside the predicate that was meant to fix it. The test
/// below pins both halves.
///
/// This is not the same mechanism as the `.git` entry in a walker's skip-list. That one
/// hides the metadata directory by *name*; this one hides the whole copy around it by its
/// shape. A walker needs both, and neither implies the other.
///
/// Any traversal rooted at the repository root has to consult this — `find_files` and
/// `rename::walk` do, and `rename::walk` is the one the five root-rooted invariant call
/// sites go through. The other collectors (`rename::collect_paths`, `scaffold::collect_files`,
/// `platform.rs`'s test-only `collect`) are called with a subtree — a project directory, an
/// app directory — and cannot reach a worktree, which is why they do not.
#[must_use]
pub fn is_nested_checkout(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Every file named `name`, recursively, skipping build output and VCS directories.
pub fn find_files(dir: &Path, name: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            // See `is_nested_checkout`: a second copy of the repository is not a directory
            // in it. `.git` is in `SKIP_DIRS` as well, and that is a different mechanism —
            // it hides the metadata directory, not the copy around it.
            if is_nested_checkout(&path) {
                continue;
            }
            let skip = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| SKIP_DIRS.contains(&n));
            if !skip {
                find_files(&path, name, out);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            out.push(path);
        }
    }
}

/// Expand a workspace-style glob with a single `*` per segment, returning directories.
///
/// Mirrors what Cargo does for `members`, so the invariant checks see the same set of
/// crates the workspace does.
pub fn expand(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut current = vec![root.to_path_buf()];
    for segment in pattern.split('/') {
        let mut next = Vec::new();
        for base in current {
            if segment == "*" {
                if let Ok(entries) = std::fs::read_dir(&base) {
                    for entry in entries.flatten() {
                        if entry.file_type().is_ok_and(|t| t.is_dir()) {
                            next.push(entry.path());
                        }
                    }
                }
            } else {
                let candidate = base.join(segment);
                if candidate.exists() {
                    next.push(candidate);
                }
            }
        }
        current = next;
    }
    current.sort();
    current
}

/// Does `apps/` hold at least one Crux app — an `apps/<name>/core` with a `Cargo.toml`?
///
/// The one question the app-shaped checks ask before insisting on a Crux layout: the `core` and
/// `shells/*` workspace globs, the plural-family shell tree, the `_platforms` recipe. Three
/// things it is deliberately not:
///
/// - "does `apps/` exist": `apps/README.md` keeps the directory alive in a product generated
///   with `new --drop-example`, so a missing directory is never the signal;
/// - "does this glob match anything": a check that goes quiet whenever what it looks for is
///   absent hides real staleness — here the glob is still required, and the tree still checked,
///   the moment one Crux app is present;
/// - "does `apps/` hold any app": a product whose only app is a `cli-app` or `web-app` has no
///   core, no shells and no `_platforms`, and none of those globs (the scaffolder adds them with
///   the first crux-app, not before).
#[must_use]
pub fn has_crux_apps(root: &Path) -> bool {
    expand(root, "apps/*/core")
        .iter()
        .any(|core| core.join("Cargo.toml").is_file())
}

/// An app's own locale namespace: `crates/i18n/locales/pages/<name>/`.
///
/// One spelling shared by the three places that have to agree about it — the scaffolder
/// that creates it, `rename::app` that moves it with the app, and the invariant that fails
/// when an app linking `i18n` has none. Written three times it drifts the way every other
/// path in this repository would, and the drift is silent: a rename moving a directory the
/// invariant looks for somewhere else is a check that passes on a tree it never inspects.
///
/// The directory name *is* the namespace (ADR 0014): `pages/` is a grouping directory that
/// names nothing, so what sits here owns `<name>.*` and rust-i18n's flat key space is the
/// reason the prefix has to be the app's name rather than the directory's.
#[must_use]
pub fn locale_namespace(root: &Path, name: &str) -> PathBuf {
    root.join("crates/i18n/locales/pages").join(name)
}

/// Path relative to the repo root, for readable diagnostics.
pub fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// `PascalCase`, as this repository spells type names derived from a crate name.
///
/// Shared with the scaffolder so a renamed app and a generated one agree about what
/// `my-app` becomes.
#[must_use]
pub fn to_pascal(name: &str) -> String {
    name.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect()
}

/// Format Rust files that have just been written or rewritten.
///
/// Not a nicety: import order depends on the crate's name, and the name is only known
/// here. `use widgets_core::…` sorts after `use crux_core::…` while `use acme_core::…`
/// sorts before it, so no fixed order in the template is correct for every name — and
/// `just check` fails on `cargo fmt --check` before it reaches anything interesting.
///
/// Both callers need it, for the same reason from opposite directions: the scaffolder
/// substitutes a name into a template, the renamer changes one already there, and either
/// way an import block's sort order is a function of that name. A tool whose output does
/// not pass the repository's own gate is a bad first minute.
///
/// rustfmt directly rather than `cargo fmt`, which would reformat the whole workspace
/// including whatever the caller has half-written elsewhere. The edition comes from
/// `[workspace.package]`, since these files inherit it.
///
/// A missing or failing rustfmt is reported and not fatal: the files are already on disk,
/// and `just fmt` finishes the job.
pub fn format_rust(root: &Path, written: &[PathBuf]) -> Result<()> {
    let sources: Vec<&PathBuf> = written
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    if sources.is_empty() {
        return Ok(());
    }

    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .context("reading the workspace manifest")?;
    let manifest: toml::Value =
        toml::from_str(&manifest).context("parsing the workspace manifest")?;
    let edition = manifest
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("edition"))
        .and_then(toml::Value::as_str)
        .context("Cargo.toml has no [workspace.package].edition")?;

    let outcome = std::process::Command::new("rustfmt")
        .arg("--edition")
        .arg(edition)
        .args(&sources)
        .status();

    match outcome {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            println!("  ! rustfmt exited with {status} — run `just fmt` before committing");
            Ok(())
        }
        Err(error) => {
            println!("  ! could not run rustfmt ({error}) — run `just fmt` before committing");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A product is a tree that carries the template input, in either spelling.
    ///
    /// The two halves are the two ways a product exists: `cli new` writes `template.url` into
    /// `flake.nix` and then locks it (`nix flake lock --update-input template`), while
    /// `--no-pin` stops before the lock. Reading only the lock would call every `--no-pin`
    /// tree a product's *opposite* — which is what happened when the signal was devenv.yaml's
    /// input and the input moved: `check-archetypes` generates with `--no-pin`, so the
    /// generated product silently lost every product-only allowance.
    /// A README alone keeps `apps/` alive in a dropped product; only a Crux app counts.
    #[test]
    fn has_crux_apps_means_a_core_not_the_apps_directory() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let base = dir.path();
        assert!(!has_crux_apps(base), "no apps/ at all");
        std::fs::create_dir(base.join("apps")).expect("mkdir apps");
        std::fs::write(base.join("apps/README.md"), "apps").expect("write README");
        assert!(
            !has_crux_apps(base),
            "apps/ with only its README holds no app"
        );
        std::fs::create_dir_all(base.join("apps/zz-single/app")).expect("mkdir web app");
        assert!(
            !has_crux_apps(base),
            "a single-crate app has no core to require"
        );
        std::fs::create_dir_all(base.join("apps/zz-crux/core")).expect("mkdir crux app");
        assert!(
            !has_crux_apps(base),
            "an empty core directory is a stray, not an app"
        );
        std::fs::write(base.join("apps/zz-crux/core/Cargo.toml"), "[package]\n")
            .expect("write core manifest");
        assert!(has_crux_apps(base), "a core with a manifest is a Crux app");
    }

    #[test]
    fn a_product_is_a_tree_that_declares_the_template_input() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let base = dir.path();
        assert!(!is_product(base), "an empty tree is not a product");

        std::fs::write(
            base.join("flake.nix"),
            "{ inputs = { template.url = \"git+https://example.com/t?rev=0\"; }; }",
        )
        .expect("write flake.nix");
        assert!(
            is_product(base),
            "the input in flake.nix is the origin record, locked or not"
        );

        std::fs::remove_file(base.join("flake.nix")).expect("remove flake.nix");
        std::fs::write(
            base.join("flake.lock"),
            r#"{ "nodes": { "root": { "inputs": { "template": "template" } }, "template": {} }, "root": "root", "version": 7 }"#,
        )
        .expect("write flake.lock");
        assert!(
            is_product(base),
            "a locked product whose flake.nix was edited still has the node"
        );
    }

    /// Both spellings of a nested checkout are one, and neither is descended into.
    ///
    /// The two halves matter equally. A clone's `.git` is a directory; a worktree's is a
    /// *file* holding a `gitdir:` pointer. A predicate written with `is_dir()` passes the
    /// first and misses the second, so the guard RT-33 already had was the one that missed
    /// worktrees — and a test built only from a `.git` directory would have passed it. Hence
    /// the file fixture first: it is the case that regressed.
    #[test]
    fn a_nested_checkout_is_not_descended_into() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let base = dir.path();

        // A worktree: `.git` as a file.
        std::fs::create_dir_all(base.join(".real")).expect("create_dir_all");
        std::fs::create_dir_all(base.join("worktree")).expect("create_dir_all");
        std::fs::write(
            base.join("worktree/.git"),
            "gitdir: /somewhere/.real/.git/worktrees/worktree\n",
        )
        .expect("writable");
        std::fs::write(base.join("worktree/Justfile"), "nested worktree\n").expect("writable");

        // A clone: `.git` as a directory.
        std::fs::create_dir_all(base.join("clone/.git")).expect("create_dir_all");
        std::fs::write(base.join("clone/Justfile"), "nested clone\n").expect("writable");

        // And the directory that really is part of the tree.
        std::fs::create_dir_all(base.join("apps")).expect("create_dir_all");
        std::fs::write(base.join("apps/Justfile"), "this repository\n").expect("writable");

        assert!(
            is_nested_checkout(&base.join("worktree")),
            "a worktree's `.git` is a file, and it is still a nested checkout"
        );
        assert!(
            is_nested_checkout(&base.join("clone")),
            "a clone's `.git` is a directory"
        );
        assert!(
            !is_nested_checkout(&base.join("apps")),
            "an ordinary directory of this repository is not one"
        );

        let mut found = Vec::new();
        find_files(base, "Justfile", &mut found);
        found.sort();
        assert_eq!(
            found,
            vec![base.join("apps/Justfile")],
            "only the copy that is part of the tree is walked"
        );
    }
}
