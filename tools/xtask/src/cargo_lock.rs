//! Keep `Cargo.lock` covering the workspace after a generator edits it (RT-76).
//!
//! A generator that adds a member or a dependency (`cli add`, `add-shell`, `add-client`,
//! `cargo xtask add-database`, …) writes manifests and nothing else, and the lock catches up on
//! the next cargo command — unless the next thing that reads it is CI. Every pipeline stage runs
//! `--locked` against sources vendored from the committed lock, so a lock without the new
//! member's dependencies fails the delta dependency layer with "no matching package named
//! `boltffi` found", which names a crate rather than the stale file. That was auspex's first
//! `ci-pure`.
//!
//! The refresh also decides WHICH versions the new dependencies get. Resolved cold, cargo takes
//! the newest compatible release; the snapshot's lock (`nix/deps-base/Cargo.lock`, which `cli new`
//! leaves as the template's) already names a version of nearly every crate an archetype uses, and
//! the bottom dependency layer holds exactly those builds. So the snapshot's registry entries the
//! live lock lacks are appended first: cargo prefers a locked version for a package it needs and
//! prunes the entries nothing needs, which leaves the lock as small as a cold resolve and its new
//! crates as the ones the bottom layer already compiled. Measured on `new --drop-example` + `add
//! crux-app --shells tui`: 26 of 609 registry crates outside the template's lock resolved cold,
//! 0 of 603 seeded.

use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// The snapshot's lock, relative to the workspace root.
pub const SNAPSHOT_LOCK: &str = "nix/deps-base/Cargo.lock";

/// Re-resolve `root/Cargo.lock` for the workspace as it now stands.
///
/// New packages prefer the snapshot's versions. Resolution runs offline first (the vendored
/// sandbox, a laptop with a warm registry), then online; on failure the lock is left exactly as
/// it was found.
///
/// # Errors
///
/// When the live lock cannot be parsed, or `cargo metadata` fails both offline and online.
pub fn refresh(root: &Path) -> Result<()> {
    let lock_path = root.join("Cargo.lock");
    let original = std::fs::read_to_string(&lock_path).ok();
    let snapshot = std::fs::read_to_string(root.join(SNAPSHOT_LOCK)).ok();
    let seeded = match (&original, &snapshot) {
        (Some(live), Some(snapshot)) => {
            let (merged, added) = seed(live, snapshot)?;
            if added > 0 {
                std::fs::write(&lock_path, merged).context("writing the seeded Cargo.lock")?;
            }
            added
        }
        _ => 0,
    };

    let manifest = root.join("Cargo.toml");
    let resolve = |offline: bool| -> std::result::Result<(), String> {
        let mut command = Command::new("cargo");
        command
            .args(["metadata", "--format-version", "1", "--manifest-path"])
            .arg(&manifest)
            .stdout(Stdio::null());
        if offline {
            command.arg("--offline");
        }
        match command.output() {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
            Err(error) => Err(format!("could not run cargo: {error}")),
        }
    };
    // Offline first; its error is kept, because when the online attempt fails too the cause is
    // as likely a manifest the generator wrote as the network, and cargo's message says which.
    if let Err(offline) = resolve(true)
        && let Err(online) = resolve(false)
    {
        // The seeded entries are only useful to a resolve that ran; left behind, they are a
        // lock full of packages nothing uses.
        if let Some(original) = &original {
            std::fs::write(&lock_path, original).context("restoring Cargo.lock")?;
        }
        bail!(
            "could not re-resolve Cargo.lock for the new workspace. CI builds `--locked`, and a \
             lock without the new dependencies fails it: fix what cargo reports below, run any \
             cargo command, and commit Cargo.lock.\n\
             cargo metadata --offline:\n{offline}\n\
             cargo metadata:\n{online}"
        );
    }
    if seeded > 0 {
        println!(
            "  ✓ updated Cargo.lock (new dependencies prefer the versions in {SNAPSHOT_LOCK})"
        );
    } else {
        println!("  ✓ updated Cargo.lock");
    }
    Ok(())
}

/// Append the snapshot's registry packages the live lock does not name (by name and version).
///
/// Returns the merged text and how many entries were added. Git and path packages are not
/// carried: a git entry pins a rev the live tree may have moved on from, and path packages are
/// workspace members. The locks are edited as TOML documents, not as text, so line endings,
/// `[metadata]` or `[[patch.unused]]` tables in either file cannot leak into the other; a snapshot
/// that does not parse seeds nothing (seeding is an optimisation, the resolve still runs).
///
/// # Errors
///
/// When the live lock is not valid TOML.
pub fn seed(live: &str, snapshot: &str) -> Result<(String, usize)> {
    let mut document = live
        .parse::<toml_edit::DocumentMut>()
        .context("parsing Cargo.lock")?;
    let Ok(snapshot) = snapshot.parse::<toml_edit::DocumentMut>() else {
        return Ok((live.to_owned(), 0));
    };
    let Some(candidates) = snapshot
        .get("package")
        .and_then(toml_edit::Item::as_array_of_tables)
    else {
        return Ok((live.to_owned(), 0));
    };
    let key = |table: &toml_edit::Table| {
        let field = |name: &str| table.get(name).and_then(toml_edit::Item::as_str);
        Some((field("name")?.to_owned(), field("version")?.to_owned()))
    };
    let present: HashSet<(String, String)> = document
        .get("package")
        .and_then(toml_edit::Item::as_array_of_tables)
        .map(|packages| packages.iter().filter_map(key).collect())
        .unwrap_or_default();

    let additions: Vec<toml_edit::Table> = candidates
        .iter()
        .filter(|table| {
            table
                .get("source")
                .and_then(toml_edit::Item::as_str)
                .is_some_and(|source| source.starts_with("registry+"))
        })
        .filter(|table| key(table).is_some_and(|k| !present.contains(&k)))
        .cloned()
        .collect();
    if additions.is_empty() {
        return Ok((live.to_owned(), 0));
    }
    let added = additions.len();
    let packages = document
        .entry("package")
        .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .context("Cargo.lock's `package` is not an array of tables")?;
    for table in additions {
        packages.push(table);
    }
    Ok((document.to_string(), added))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
        [[package]]\nname = \"serde\"\nversion = \"1.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"aa\"\n";

    const SNAPSHOT: &str = "version = 4\n\n[[package]]\nname = \"counter\"\nversion = \"0.1.0\"\n\n\
        [[package]]\nname = \"serde\"\nversion = \"1.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"aa\"\n\n\
        [[package]]\nname = \"boltffi\"\nversion = \"0.3.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"bb\"\ndependencies = [\n \"serde\",\n]\n\n\
        [[package]]\nname = \"forked\"\nversion = \"0.1.0\"\nsource = \"git+https://example.com/forked?rev=1#1\"\n";

    #[test]
    fn only_the_snapshots_missing_registry_packages_are_appended() {
        let (merged, added) = seed(LIVE, SNAPSHOT).expect("seed");
        assert_eq!(added, 1, "{merged}");
        let names = names(&merged);
        assert_eq!(
            names,
            [
                ("app".to_owned(), "0.1.0".to_owned()),
                ("serde".to_owned(), "1.0.1".to_owned()),
                ("boltffi".to_owned(), "0.3.0".to_owned()),
            ],
            "the workspace member and the git package stay out, serde is not duplicated"
        );
        assert!(
            merged.contains("dependencies = [\n \"serde\",\n]"),
            "{merged}"
        );
    }

    fn names(lock: &str) -> Vec<(String, String)> {
        let document = lock
            .parse::<toml_edit::DocumentMut>()
            .expect("the lock parses");
        document["package"]
            .as_array_of_tables()
            .expect("packages")
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().expect("name").to_owned(),
                    t["version"].as_str().expect("version").to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn crlf_and_trailing_tables_neither_hide_entries_nor_leak() {
        let snapshot = format!("{SNAPSHOT}\n[metadata]\nold = \"v1\"\n").replace('\n', "\r\n");
        let live = format!("{LIVE}\n[metadata]\nmine = \"kept\"\n");
        let (merged, added) = seed(&live, &snapshot).expect("seed");
        assert_eq!(added, 1, "{merged}");
        assert!(names(&merged).contains(&("boltffi".to_owned(), "0.3.0".to_owned())));
        assert!(merged.contains("mine = \"kept\""), "{merged}");
        assert!(!merged.contains("old = "), "{merged}");
    }

    #[test]
    fn a_snapshot_that_does_not_parse_seeds_nothing() {
        let (merged, added) = seed(LIVE, "[[package]\nname = ").expect("seed");
        assert_eq!((merged.as_str(), added), (LIVE, 0));
    }

    #[test]
    fn a_lock_that_already_covers_the_snapshot_is_unchanged() {
        let (merged, added) = seed(LIVE, LIVE).expect("seed");
        assert_eq!(added, 0);
        assert_eq!(merged, LIVE);
    }
}
