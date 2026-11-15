//! Generators for the inside of a service.
//!
//! `just add service` creates a service. These create the things a service grows
//! afterwards: a migration, a feature slice with its contract and conversions, one more
//! DTO on an existing slice, and the database plumbing a stateless service starts without.
//!
//! # Why these are not templates under `templates/`
//!
//! [`crate::scaffold`] renders a template as an overlay on the repository root, which
//! works because an archetype's destination is a function of its name alone. Nothing here
//! has that property: the destination depends on which service, the file contents depend
//! on a field list that is different every time, and most of these generators do most of
//! their work *editing files that already exist* — splicing a `mod` into a `Migrator`, a
//! `pub mod` into a `lib.rs`, an `add_service` into the one place that knows every feature,
//! an `rpc` into a contract somebody maintains by hand. A template can express none of
//! that, so these build their output in code and the template directory stays what it is:
//! skeletons for whole crates.
//!
//! # The order these run in
//!
//! ```text
//! add-database   once per service, if it has no schema yet
//! add-auth-oidc  once per service, if it should verify OIDC tokens (independent of the above)
//! new-feature    the slice: contract, domain, conversions, service impl, migration
//! just proto::generate                   the contract compiles
//! just services/<svc>/migrate            the table exists
//! just services/<svc>/entities           the entity exists
//! new-rpc        later, for each method the feature grows
//! new-domain     later, for each further DTO declared in the contract by hand
//! ```
//!
//! The last two overlap on purpose. `new-rpc` writes the contract *and* the DTO, so it is
//! what to reach for when the method is new; `new-domain` writes only the DTO, for a payload
//! the contract already declares — a method added by hand, or a second payload one method
//! carries. Running `new-domain` twice is not `new-rpc`: it produces the two DTOs and leaves
//! the method undeclared and unimplemented.
//!
//! `new-feature` writes code that names an entity which does not exist until the two
//! middle steps have run, and says so when it finishes. That is the same bargain
//! `just add service` already makes with `just proto::generate` — a scaffold that does not
//! compile until a generator has run — rather than a new one.

mod auth;
mod database;
mod domain;
mod feature;
pub mod invariants;
mod migration;
mod protoedit;
mod rpc;
mod rustedit;
mod schema;

#[cfg(test)]
mod fixtures;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
pub use auth::{Options as AuthOptions, add_auth_oidc};
pub use database::add_database;
pub use domain::new_domain;
pub use feature::new_feature;
pub use migration::new_migration;
pub use rpc::new_rpc;

use crate::repo;

/// A service this family of generators can write into.
///
/// Resolved once, up front, so every refusal arrives before anything is on disk. The
/// fields are the four paths each generator would otherwise re-derive, and re-deriving a
/// path in four places is how the renamer and the invariants once disagreed about where an
/// app's locale namespace lived.
pub struct Service {
    /// The repository root.
    pub root: PathBuf,
    /// `services/<name>`.
    pub dir: PathBuf,
    /// As it is spelled on the command line and in `Cargo.toml`.
    pub name: String,
}

impl Service {
    /// Resolve a service by name, refusing one that is not there.
    pub fn open(name: &str) -> Result<Self> {
        let root = repo::root();
        let dir = root.join("services").join(name);

        if !dir.join("Cargo.toml").is_file() {
            let available = available_services(&root);
            let known = if available.is_empty() {
                "there are none — `just add service <name>` creates one".to_owned()
            } else {
                format!("known services: {}", available.join(", "))
            };
            bail!(
                "no service at {} — {known}",
                repo::rel(&root, &dir.join("Cargo.toml"))
            );
        }

        Ok(Self {
            root,
            dir,
            name: name.to_owned(),
        })
    }

    /// `services/<name>/src/<rest>`.
    pub fn src(&self, rest: &str) -> PathBuf {
        self.dir.join("src").join(rest)
    }

    /// Path relative to the repository root, for diagnostics.
    pub fn rel(&self, path: &Path) -> String {
        repo::rel(&self.root, path)
    }

    /// Whether this service has the schema plumbing the other generators write into.
    ///
    /// Read off the files rather than off a manifest flag, because the files are what the
    /// generated code names. `templates/service` ships none of them — a stateless service
    /// is the default, deliberately (`services/api/src/migrations/mod.rs` argues it) — so
    /// this is false for every freshly scaffolded service and [`add_database`] is what
    /// changes it.
    pub fn has_database(&self) -> bool {
        self.src("db.rs").is_file() && self.src("migrations/mod.rs").is_file()
    }

    /// Refuse a generator that needs a schema, naming the command that installs one.
    pub fn require_database(&self) -> Result<()> {
        if self.has_database() {
            return Ok(());
        }
        bail!(
            "service {:?} has no database plumbing — {} and {} are both absent.\n\
             \n\
             That is the default rather than a mistake: a service owns its own schema or \
             has none at all, so `just add service` ships neither. Install it with:\n\
             \n    cargo xtask add-database {}\n",
            self.name,
            self.rel(&self.src("db.rs")),
            self.rel(&self.src("migrations/mod.rs")),
            self.name,
        )
    }

    /// `services/<name>/tests/<name>/auth.rs` — the classification table.
    ///
    /// A test file, which no generator writes and which `new-feature` nonetheless has to
    /// edit: its `PUBLIC`/`PROTECTED` lists are the only place a decision about who may
    /// call an RPC is written down, and `every_registered_method_is_classified` fails until
    /// every method `register_services` answers to is in exactly one of them (ADR 0012).
    /// The path is stated once here because the alternative is four copies of a convention
    /// that lives in another crate.
    pub fn auth_test(&self) -> PathBuf {
        self.dir.join("tests").join(&self.name).join("auth.rs")
    }

    /// The entity sea-orm-cli generates for `table`, if it has been generated yet.
    pub fn entity(&self, table: &str) -> Option<PathBuf> {
        let path = self.src(&format!("entities/{table}.rs"));
        path.is_file().then_some(path)
    }
}

/// Service directories that have a manifest, for a diagnostic that names the real set.
fn available_services(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = repo::expand(root, "services/*")
        .into_iter()
        .filter(|dir| dir.join("Cargo.toml").is_file())
        .filter_map(|dir| {
            dir.file_name()
                .and_then(|n| n.to_str())
                .map(ToOwned::to_owned)
        })
        .collect();
    names.sort();
    names
}

/// Files a generator is about to write, refused as a set before any of them lands.
///
/// The scaffolder checks the same way and for the same reason: a refusal that arrives
/// halfway through leaves a tree nobody asked for, and the second run then refuses because
/// of the mess the first one made.
#[derive(Default)]
pub struct Writes {
    planned: Vec<(PathBuf, String)>,
}

impl Writes {
    pub fn add(&mut self, path: PathBuf, contents: String) {
        self.planned.push((path, contents));
    }

    /// Write every planned file, after checking that none of them exists.
    ///
    /// Returns the paths written, for rustfmt.
    pub fn commit(self, service: &Service) -> Result<Vec<PathBuf>> {
        for (path, _) in &self.planned {
            if path.exists() {
                bail!(
                    "{} already exists — refusing to overwrite",
                    service.rel(path)
                );
            }
        }

        let mut written = Vec::new();
        for (path, contents) in self.planned {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&path, contents)
                .with_context(|| format!("writing {}", service.rel(&path)))?;
            written.push(path);
        }
        Ok(written)
    }
}

/// Add `[dependencies]` entries to a service's manifest, as `name.workspace = true`.
///
/// Shared rather than written twice: `add-database` needs sea-orm and `new-feature` needs
/// thiserror, and the interesting half is the same for both — a generator that writes code
/// naming a crate the manifest does not list produces a service that does not compile, and
/// the error names the crate rather than the generator that forgot it.
///
/// Always `workspace = true`: a version here would be a second place to bump, which
/// `versions.toml` and `[workspace.dependencies]` exist to prevent. An entry that is already
/// present is left exactly as it is, so running a generator twice adds nothing.
///
/// # Errors
///
/// Returns an error if the manifest does not parse or has no `[dependencies]` table.
pub fn add_dependencies(source: &str, names: &[&str]) -> Result<String> {
    let mut document: toml_edit::DocumentMut = source.parse()?;
    let dependencies = document
        .get_mut("dependencies")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| anyhow::anyhow!("the manifest has no [dependencies] table"))?;

    for name in names {
        if dependencies.contains_key(name) {
            continue;
        }
        let mut entry = toml_edit::Table::new();
        entry.set_dotted(true);
        entry.insert("workspace", toml_edit::value(true));
        dependencies.insert(name, toml_edit::Item::Table(entry));
    }

    Ok(document.to_string())
}

/// Read a file a generator is about to rewrite, reporting its path if it is not there.
///
/// Shared, and shared as the *only* way these generators read an existing file: a bare
/// `read_to_string` fails with `No such file or directory (os error 2)`, which in a command
/// that touches ten files does not say which file was wanted. `add-database`'s tests were the
/// ones that had to learn this, and the three generators RT-38 converts read six files between
/// them — a rule stated once in `api/mod.rs` is a rule none of them can forget.
pub fn read_for_edit(service: &Service, path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("reading {} to rewire it", service.rel(path)))
}

/// Write files whose contents were resolved before anything was committed.
///
/// The second half of the plan-then-write shape: by the time this runs, every refusal this
/// generator can produce has already happened, so the only errors left are I/O errors.
///
/// A file that already says what the generator would write is left exactly as it is. `amend`
/// did this with an early return, and it is the ordinary case rather than an edge case: a
/// service that already has sea-orm in its manifest, or already has the `migrate:` recipe, is
/// something a generator meets. It applies to the rewired files too, which is a change of
/// behaviour taken deliberately — rewriting identical bytes is not a change, and it is not free
/// when a build is watching mtimes.
pub fn write_planned(service: &Service, planned: &[(PathBuf, String)]) -> Result<()> {
    for (path, contents) in planned {
        if std::fs::read(path).is_ok_and(|existing| existing == contents.as_bytes()) {
            continue;
        }
        std::fs::write(path, contents).with_context(|| format!("writing {}", service.rel(path)))?;
    }
    Ok(())
}

/// Format everything that was touched, then report it.
///
/// The **amended** files need rustfmt as much as the new ones, and for a reason that is
/// easy to miss: a spliced line is correct Rust in the wrong shape. Pushing one more entry
/// into a `vec![..]` that fitted on one line makes it a line over the width limit, and
/// `just check` then fails on `cargo fmt --check` with a diff nobody wrote — in a file the
/// generator was only supposed to register something in. Formatting here is what keeps a
/// generated change passing the repository's own gate, which is the first thing anyone
/// does with it.
pub fn report(service: &Service, written: &[PathBuf], amended: &[&Path]) -> Result<()> {
    let touched: Vec<PathBuf> = written
        .iter()
        .cloned()
        .chain(amended.iter().map(|path| (*path).to_path_buf()))
        .collect();
    repo::format_rust(&service.root, &touched)?;

    println!("  ✓ created {} files:", written.len());
    for path in written {
        println!("      {}", service.rel(path));
    }
    if !amended.is_empty() {
        println!("  ✓ wired into:");
        for path in amended {
            println!("      {}", service.rel(path));
        }
    }
    Ok(())
}
