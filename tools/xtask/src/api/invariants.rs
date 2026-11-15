//! Structural rules about the inside of a service.
//!
//! Both of these guard the same failure mode, and it is the one the generators in this
//! module exist to prevent: a file that is **correct and unreachable**. A migration the
//! `Migrator` does not list compiles, lints and tests clean, and the table is simply not
//! there. A feature slice `register_services` does not mount compiles just as cleanly, and
//! every one of its methods answers 404.
//!
//! Neither has a symptom at build time, which is what makes them worth a check rather than
//! a convention. `cargo xtask new-migration` and `new-feature` write both halves, so the
//! only way to arrive here is by hand — which is exactly when a reminder is wanted.

use std::path::Path;

use anyhow::{Context, Result};

use crate::repo;

/// Service trees to inspect: every real service, and the skeleton `just add service`
/// renders.
///
/// The skeleton is included for the reason the other template checks exist: it is the one
/// tree in the repository that nothing compiles by default, so a slice left unmounted
/// there would ship into every product made from it and be found by whoever first wondered
/// why their scaffolded service answered 404.
fn service_trees(root: &Path) -> Vec<std::path::PathBuf> {
    let mut trees = repo::expand(root, "services/*");
    let skeleton = root.join("templates/service/services/__name__");
    if skeleton.is_dir() {
        trees.push(skeleton);
    }
    trees.retain(|tree| tree.join("src/startup.rs").is_file());
    trees
}

pub fn check(root: &Path, checked: &mut usize, problems: &mut Vec<String>) -> Result<()> {
    for tree in service_trees(root) {
        migrations_are_registered(root, &tree, checked, problems)?;
        features_are_mounted(root, &tree, checked, problems)?;
    }
    Ok(())
}

/// Every `m*.rs` beside the `Migrator` is in the list it returns.
///
/// Checked on both halves — the `mod` declaration and the `Box::new(..)` entry — because
/// they fail differently and only one of them is loud. A missing `mod` is a dead file the
/// compiler never reads; a missing `Box::new` is a module that compiles, is never run, and
/// leaves the schema one table short of what the code expects.
fn migrations_are_registered(
    root: &Path,
    tree: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let registry = tree.join("src/migrations/mod.rs");
    if !registry.is_file() {
        // A stateless service, which is the default. Nothing to check.
        return Ok(());
    }

    let source = std::fs::read_to_string(&registry)
        .with_context(|| format!("reading {}", repo::rel(root, &registry)))?;

    let directory = registry.parent().context("the registry has a directory")?;
    let mut modules: Vec<String> = std::fs::read_dir(directory)
        .with_context(|| format!("reading {}", repo::rel(root, directory)))?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "rs" {
                return None;
            }
            let stem = path.file_stem()?.to_str()?;
            // `m<timestamp>_<name>`, which is what both this tool and `sea-orm-cli migrate
            // generate` produce. Anything else in here is a helper rather than a migration.
            stem.starts_with('m')
                .then(|| stem.to_owned())
                .filter(|stem| stem[1..].starts_with(|c: char| c.is_ascii_digit()))
        })
        .collect();
    modules.sort();

    for module in modules {
        *checked += 1;
        if !source.contains(&format!("mod {module};")) {
            problems.push(format!(
                "{} is not declared in {} — a migration file nothing declares is never \
                 compiled, and the table it creates is simply not there",
                repo::rel(root, &directory.join(format!("{module}.rs"))),
                repo::rel(root, &registry),
            ));
            continue;
        }
        if !source.contains(&format!("Box::new({module}::Migration)")) {
            problems.push(format!(
                "{module} is declared in {} but missing from `Migrator::migrations()` — it \
                 compiles and never runs, which is a schema one table short with nothing to \
                 report it",
                repo::rel(root, &registry),
            ));
        }
    }
    Ok(())
}

/// Every feature slice is mounted in `register_services`.
///
/// A slice is a directory with a `service/v1.rs`: that file exists to be dispatched to, so
/// one that nothing mounts is a whole feature answering 404 with no error anywhere.
fn features_are_mounted(
    root: &Path,
    tree: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let startup = tree.join("src/startup.rs");
    let source = std::fs::read_to_string(&startup)
        .with_context(|| format!("reading {}", repo::rel(root, &startup)))?;

    // Only what `register_services` names. Read from the whole file, a slice would count as
    // mounted because its module is imported at the top — which is the state this check is
    // most likely to be looking at, since importing it is the half people remember.
    let registered = source
        .split_once("fn register_services")
        .map_or_else(String::new, |(_, body)| body.to_owned());

    let source_dir = tree.join("src");
    let mut features: Vec<String> = std::fs::read_dir(&source_dir)
        .with_context(|| format!("reading {}", repo::rel(root, &source_dir)))?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            path.join("service/v1.rs")
                .is_file()
                .then(|| path.file_name()?.to_str().map(ToOwned::to_owned))
                .flatten()
        })
        .collect();
    features.sort();

    for feature in features {
        *checked += 1;
        if !registered.contains(&format!("{feature}::service::v1::")) {
            problems.push(format!(
                "{} is never mounted in {}'s `register_services` — every method it \
                 implements answers 404, and nothing else in the repository would say so",
                repo::rel(root, &source_dir.join(&feature)),
                repo::rel(root, &startup),
            ));
        }
    }
    Ok(())
}
