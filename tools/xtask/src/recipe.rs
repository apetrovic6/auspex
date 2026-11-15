//! Generator-written product history ([ADR 0030](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0030-products-update-by-regenerating-and-merging.md)).
//!
//! Kept in xtask's library, which the CLI depends on: products retain `tools/xtask`
//! but strip `tools/cli`. A library target shares the format without another crate.
//! `[new]` stores resolved answers; steps store argv after the tool name, without
//! reordering flags or reconstructing defaults. Current generators take names, not file
//! paths, so no path normalization is needed. Xtask's global `--breakdown` is omitted:
//! it only controls check output. `--issuer` is kept as given so base and target receive
//! the same value and reproduce the same local configuration, without merge noise.
//!
//! Recorded: CLI `add` (all six crate archetypes and both native platforms),
//! `add-shell`, `add-client`, `rename app`, `rename org`; xtask `add-auth-oidc`,
//! `add-database`, `new-migration`, `new-feature`, `new-domain`, `new-rpc`.
//! There is no separate CLI `add-platform`.
//! Excluded: checks (`check`, `check-*`, gates), `deny`, `advisory-pr`, and `version`
//! inspect/maintain the repository, not scaffolds; `new-adr` writes prose; `init`
//! consumes transient answers already captured by `new`. `i18n-resources` writes
//! disposable build output, like proto/type generation, not a scaffold.
//! Template invocations never record anything.
//!
//! Mutations hold an exclusive, nonblocking OS file lock (`flock` on Unix) on
//! `.template/recipe.lock` through read, parse, write and atomic rename. A contender
//! errors rather than silently losing a step. Keep the gitignored lock file: unlinking
//! it would let processes lock different inodes. Earlier recipe bytes remain verbatim.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const PATH: &str = ".template/recipe.toml";

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct New {
    pub authors: String,
    pub repository: String,
    pub license: String,
    pub description: String,
    pub app: String,
    pub org: String,
    pub drop_example: bool,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub tool: Tool,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Cli,
    Xtask,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    version: u32,
    pub new: New,
    #[serde(default, rename = "step", skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
}

/// Read a supported recipe, including its required creation answers.
///
/// # Errors
/// Returns an error for unreadable, malformed, incomplete or unsupported recipes.
pub fn read(root: &Path) -> Result<Recipe> {
    parse(&read_text(root)?)
}

fn read_text(root: &Path) -> Result<String> {
    std::fs::read_to_string(root.join(PATH)).map_err(|error| {
        let context = if error.kind() == ErrorKind::NotFound {
            format!(
                "{PATH} is missing. For an existing product, create it once by hand from its \
                 creation answers and generator history: include version = 1, a [new] table \
                 with authors, repository, license, description, app, org, drop_example, \
                 and the earlier [[step]] entries in order"
            )
        } else {
            format!("reading {PATH}")
        };
        anyhow::Error::new(error).context(context)
    })
}

fn lock(root: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".template/recipe.lock"))
        .context("opening .template/recipe.lock")?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!(".template/recipe.lock is held by another generator; recipe was not changed")
        }
        Err(TryLockError::Error(error)) => Err(error).context("locking .template/recipe.lock"),
    }
}

fn parse(text: &str) -> Result<Recipe> {
    let recipe: Recipe = toml::from_str(text).context("parsing .template/recipe.toml")?;
    ensure!(
        recipe.version == 1,
        "unsupported recipe version {}",
        recipe.version
    );
    Ok(recipe)
}

/// Write the initial recipe without inheriting any other product's history.
///
/// # Errors
/// Refuses an existing recipe, or reports serialization and atomic-write failures.
pub fn create(root: &Path, new: New) -> Result<()> {
    std::fs::create_dir_all(root.join(".template")).context("creating .template")?;
    let _lock = lock(root)?;
    ensure!(!root.join(PATH).exists(), "refusing to overwrite {PATH}");
    let recipe = Recipe {
        version: 1,
        new,
        steps: Vec::new(),
    };
    let text = format!(
        "# Written by generators, not by hand.\n{}",
        toml::to_string(&recipe)?
    );
    replace(&root.join(PATH), text.as_bytes())
}

/// Append one successful invocation, preserving all existing bytes.
///
/// # Errors
/// Refuses locked, unreadable, malformed or unsupported history, or reports write failures.
/// The error includes the exact entry to add by hand after a successful generator.
pub fn append(root: &Path, tool: Tool, args: Vec<String>) -> Result<()> {
    let entry = entry(tool, args)?;
    append_entry(root, &entry).with_context(|| {
        format!(
            "generator succeeded, but recording in {PATH} failed; do not rerun the generator.\n\
             Resolve the error, then append this exact TOML entry once by hand:\n\n{entry}"
        )
    })
}

fn entry(tool: Tool, args: Vec<String>) -> Result<String> {
    #[derive(Serialize)]
    struct Entry {
        step: [Step; 1],
    }
    toml::to_string(&Entry {
        step: [Step { tool, args }],
    })
    .context("serializing recipe entry")
}

fn append_entry(root: &Path, entry: &str) -> Result<()> {
    let _lock = lock(root)?;
    let mut text = read_text(root)?;
    parse(&text)?;
    // Serialize only the new entry: comments, quoting and whitespace of every
    // earlier entry must survive byte for byte.
    text.push('\n');
    text.push_str(entry);
    replace(&root.join(PATH), text.as_bytes())
}

fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("recipe has no parent directory")?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).context("creating recipe temporary file")?;
    temp.write_all(bytes)
        .context("writing recipe temporary file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))
            .context("setting recipe permissions to 0644")?;
    }
    temp.as_file()
        .sync_all()
        .context("syncing recipe temporary file")?;
    temp.persist(path)
        .context("renaming recipe temporary file")?;
    Ok(())
}

/// The existing origin-pin signal, shared with CLI recording.
#[must_use]
pub fn is_product(root: &Path) -> bool {
    if std::fs::read_to_string(root.join("flake.nix"))
        .is_ok_and(|text| text.contains("template.url"))
    {
        return true;
    }
    std::fs::read_to_string(root.join("flake.lock")).is_ok_and(|text| {
        serde_json::from_str::<serde_json::Value>(&text).is_ok_and(|lock| {
            let nodes = lock.get("nodes");
            lock.get("root")
                .and_then(serde_json::Value::as_str)
                .and_then(|name| nodes.and_then(|nodes| nodes.get(name)))
                .and_then(|node| node.get("inputs"))
                .and_then(|inputs| inputs.get("template"))
                .is_some()
        })
    })
}

pub fn check(root: &Path, checked: &mut usize, problems: &mut Vec<String>) {
    *checked += 1;
    if is_product(root) {
        if let Err(error) = read(root) {
            problems.push(format!("{PATH}: {error:#}"));
        }
    } else if root.join(PATH).exists() {
        problems.push(format!("{PATH}: recipe must be absent in the template"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers() -> New {
        New {
            authors: "A <a@example.org>".into(),
            repository: "https://example.org".into(),
            license: "MIT".into(),
            description: "Quotes: \" and newline\n".into(),
            app: "counter".into(),
            org: "io.example".into(),
            drop_example: false,
        }
    }

    #[test]
    fn appends_preserve_bytes_and_order_and_replace_the_file() {
        let dir = tempfile::tempdir().expect("directory");
        create(dir.path(), answers()).expect("create");
        assert_eq!(read(dir.path()).expect("read").new, answers());
        let path = dir.path().join(PATH);
        let mut original = std::fs::read_to_string(&path).expect("read");
        original.push_str("\n# preserve this comment without a trailing newline");
        std::fs::write(&path, &original).expect("write");
        // An open reader must keep the complete old inode, never a truncated append.
        let mut old = std::fs::File::open(&path).expect("open old inode");
        let args = vec!["add".into(), "service".into(), "billing".into()];
        append(dir.path(), Tool::Cli, args.clone()).expect("append");
        let mut old_bytes = String::new();
        std::io::Read::read_to_string(&mut old, &mut old_bytes).expect("read old inode");
        assert_eq!(old_bytes, original);
        let first = std::fs::read_to_string(&path).expect("read first");
        assert!(first.starts_with(&original));
        append(
            dir.path(),
            Tool::Xtask,
            vec!["add-database".into(), "billing".into()],
        )
        .expect("append second");
        assert!(
            std::fs::read_to_string(&path)
                .expect("read second")
                .starts_with(&first)
        );
        assert_eq!(
            read(dir.path()).expect("parse").steps,
            vec![
                Step {
                    tool: Tool::Cli,
                    args
                },
                Step {
                    tool: Tool::Xtask,
                    args: vec!["add-database".into(), "billing".into()]
                },
            ]
        );
    }

    #[test]
    fn invalid_or_unknown_recipes_are_never_overwritten() {
        let dir = tempfile::tempdir().expect("directory");
        std::fs::create_dir(dir.path().join(".template")).expect("create parent");
        let valid = toml::to_string(&Recipe {
            version: 1,
            new: answers(),
            steps: Vec::new(),
        })
        .expect("serialize");
        for text in [
            "broken = [".to_owned(),
            valid.replace("version = 1", "version = 2"),
            "version = 1\n".to_owned(),
        ] {
            std::fs::write(dir.path().join(PATH), &text).expect("write");
            assert!(append(dir.path(), Tool::Cli, vec!["add".into()]).is_err());
            assert!(create(dir.path(), answers()).is_err());
            assert_eq!(
                std::fs::read_to_string(dir.path().join(PATH)).expect("unchanged"),
                text
            );
        }
    }

    #[test]
    fn failed_rename_keeps_destination_and_removes_temporary_file() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("destination");
        std::fs::create_dir(&path).expect("destination directory");
        assert!(replace(&path, b"replacement").is_err());
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).expect("entries").count(), 1);
    }

    #[test]
    fn held_lock_refuses_an_append_and_reports_the_exact_recovery_entry() {
        #[derive(Deserialize)]
        struct Recovery {
            step: Vec<Step>,
        }
        let dir = tempfile::tempdir().expect("directory");
        create(dir.path(), answers()).expect("create");
        let original = std::fs::read(dir.path().join(PATH)).expect("read");
        let guard = lock(dir.path()).expect("hold lock");
        let args = vec!["rename".into(), "app".into(), "old".into(), "new".into()];
        let error = append(dir.path(), Tool::Cli, args.clone()).expect_err("contender fails fast");
        let message = format!("{error:#}");
        assert!(
            message.contains(".template/recipe.lock is held by another generator"),
            "{message}"
        );
        assert!(message.contains("generator succeeded"), "{message}");
        let context = error.to_string();
        let recovery = context
            .split_once("once by hand:\n\n")
            .expect("manual entry")
            .1;
        let recovery: Recovery = toml::from_str(recovery).expect("complete valid recovery TOML");
        assert_eq!(
            recovery.step,
            vec![Step {
                tool: Tool::Cli,
                args: args.clone()
            }]
        );
        assert_eq!(
            std::fs::read(dir.path().join(PATH)).expect("unchanged"),
            original
        );
        drop(guard);
        append(dir.path(), Tool::Cli, args.clone()).expect("released lock");
        assert_eq!(
            read(dir.path()).expect("read").steps,
            vec![Step {
                tool: Tool::Cli,
                args
            }]
        );
    }

    #[test]
    fn concurrent_appends_record_every_success_and_each_loser_errors() {
        let dir = tempfile::tempdir().expect("directory");
        create(dir.path(), answers()).expect("create");
        let barrier = std::sync::Barrier::new(8);
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|id| {
                    let root = dir.path();
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let args = vec!["add".into(), "lib-crate".into(), format!("probe-{id}")];
                        barrier.wait();
                        let result = append(root, Tool::Cli, args.clone());
                        (args, result)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("join"))
                .collect::<Vec<_>>()
        });
        let mut succeeded = Vec::new();
        for (args, result) in results {
            match result {
                Ok(()) => succeeded.push(args),
                Err(error) => assert!(
                    format!("{error:#}")
                        .contains(".template/recipe.lock is held by another generator"),
                    "{error:#}"
                ),
            }
        }
        let mut recorded: Vec<_> = read(dir.path())
            .expect("read")
            .steps
            .into_iter()
            .map(|step| step.args)
            .collect();
        succeeded.sort();
        recorded.sort();
        assert_eq!(recorded, succeeded);
    }

    #[test]
    #[cfg(unix)]
    fn create_and_append_publish_world_readable_recipes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("directory");
        create(dir.path(), answers()).expect("create");
        let path = dir.path().join(PATH);
        assert_eq!(
            std::fs::metadata(&path).expect("mode").permissions().mode() & 0o777,
            0o644
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("restrict");
        append(
            dir.path(),
            Tool::Cli,
            vec!["add-client".into(), "probe".into()],
        )
        .expect("append");
        assert_eq!(
            std::fs::metadata(&path).expect("mode").permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn invariant_requires_valid_product_recipe_and_absent_template_recipe() {
        let dir = tempfile::tempdir().expect("directory");
        let mut checked = 0;
        let mut problems = Vec::new();
        check(dir.path(), &mut checked, &mut problems);
        assert_eq!(problems, Vec::<String>::new());
        std::fs::write(dir.path().join("flake.nix"), "template.url = \"origin\";")
            .expect("product");
        check(dir.path(), &mut checked, &mut problems);
        assert_eq!(problems.len(), 1);
        problems.clear();
        create(dir.path(), answers()).expect("create");
        check(dir.path(), &mut checked, &mut problems);
        assert_eq!(problems, Vec::<String>::new());
        std::fs::remove_file(dir.path().join("flake.nix")).expect("template");
        check(dir.path(), &mut checked, &mut problems);
        assert_eq!(
            problems,
            vec![format!("{PATH}: recipe must be absent in the template")]
        );
    }
}
