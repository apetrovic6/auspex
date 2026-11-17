//! Repository tooling.
//!
//! The monorepo's conventions are enforced by layout rather than by a registry — a crate
//! is a workspace member because of where it sits, not because something lists it. That
//! keeps adding an app free, but it means a handful of rules have to be checked rather
//! than assumed. This binary is where those checks live.

mod adr;
mod advisory_pr;
mod api;
mod deny;
mod gates;
mod i18n_resources;
mod invariants;
mod locks;
mod platform;
mod rename;
mod repo;
mod scaffold;
mod swift_keywords;
mod versions;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use xtask::{cargo_lock, recipe};

#[derive(Parser)]
#[command(name = "xtask", about = "Repository tooling", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Print what each check contributed to the count, before the summary
    ///
    /// `global` so it can be typed where it reads naturally — `cargo xtask check
    /// --breakdown` — rather than only before the subcommand. A flag rather than an
    /// environment variable because `--help` is where someone looks for it, and the
    /// throwaway versions of this that `XTASK_BREAKDOWN` served were thrown away twice
    /// precisely because nothing advertised them (RT-16).
    #[arg(long, global = true)]
    breakdown: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run every repository check (versions + invariants)
    Check,
    /// Verify Cargo, Gradle and rust-toolchain agree with versions.toml
    CheckVersions,
    /// Verify the workspace layout conventions hold
    CheckInvariants,
    /// Run the supply-chain gate, and on failure say which of its two answers it needs
    ///
    /// `just deny` runs this, and `jobs::supply_chain` runs `just deny` on every push and on the
    /// daily advisories schedule, so the line reaches a laptop and a notification from one
    /// implementation. cargo-deny's own output and exit status pass through untouched and the
    /// triage is added underneath, only on failure: findings counted by kind —
    /// `vulnerability` against `unmaintained`, which is cargo-deny's own `fields.code` — and for
    /// each whether its solution note names something to move to. What it does not do is choose:
    /// which kind gets a `deny.toml` `ignore` entry is prose in `deny.toml`, and re-implementing
    /// that here would be the second parse of one rule this repository keeps paying for. See
    /// `tools/xtask/src/deny.rs`.
    Deny {
        /// Run the bump when the verdict is unambiguous, instead of only reporting it
        ///
        /// Acts only when every finding is a `vulnerability` naming a fix and the triage lost
        /// nothing: one `cargo update -p <crate>` per crate, then the gate again. Green leaves the
        /// new `Cargo.lock` on disk and reports the whole diff's shape; still red restores the
        /// lockfile byte for byte and prints the verdict it would have printed anyway. Anything
        /// else changes nothing, which is the common case — `unmaintained` findings are a
        /// `deny.toml` decision this must not make.
        ///
        /// Off by default, and it never makes a green run say anything.
        #[arg(long)]
        fix: bool,
    },
    /// Turn the bump `deny --fix` left behind into a branch and a pull request
    ///
    /// The second act, and the only step in the nightly that holds a credential: `deny --fix`
    /// rewrites `Cargo.lock` in the checkout and writes its record under `target/`, and this reads
    /// both, deduplicates against the forge's *open* pull requests, pushes a branch and opens one.
    /// It never merges — ADR 0023 settled that, and the pull request body says so where a person
    /// will read it. Needs `ADVISORY_PR_TOKEN` plus the `GITHUB_*` variables a Forgejo job sets;
    /// on a laptop, where the bump is real and the publishing is not, it fails naming what is
    /// missing rather than quietly opening nothing.
    ///
    /// Takes no paths: it reads the record and the lockfile from where `deny --fix` wrote them,
    /// which is the only place they exist.
    AdvisoryPr,
    /// Print one value from versions.toml, e.g. `tools.boltffi`
    Version {
        /// Dotted key path into versions.toml
        key: String,
    },
    /// Generate Android/Apple localization resources from the i18n locale tree
    I18nResources {
        /// The app that owns the platform project
        app: String,
        /// Which platform's resources to generate
        platform: platform::Platform,
    },
    /// Scaffold a migration inside a service, and register it with its `Migrator`
    NewMigration {
        /// The service that owns the schema
        service: String,
        /// Migration name, in `snake_case`, e.g. `create_lakes`
        name: String,
        /// Table to create. Inferred from a `create_<table>` name when omitted.
        #[arg(long)]
        table: Option<String>,
        /// A column, as `name:type` — `string`, `bool`, `int32`, `int64`, `double` or
        /// `bytes`, with `?` for nullable. Repeatable.
        #[arg(long = "field")]
        fields: Vec<String>,
    },
    /// Scaffold a feature slice: contract, domain types, conversions and service impl
    NewFeature {
        /// The service to add it to
        service: String,
        /// Feature name, in kebab-case, e.g. `lake`
        name: String,
        /// Table the rows live in. Defaults to the pluralised feature name.
        #[arg(long)]
        table: Option<String>,
    },
    /// Add one method to an existing feature: rpc, messages, domain types, handler, auth
    NewRpc {
        /// The service that owns the feature
        service: String,
        /// The feature the method belongs to, e.g. `lake`
        feature: String,
        /// The method, in kebab-case — `archive-lake` becomes `rpc ArchiveLake`
        name: String,
        /// A request field, as `name:type` — `string`, `bool`, `int32`, `int64`, `double`
        /// or `bytes`, with `?` for optional and `[]` for repeated. Repeatable.
        ///
        /// From the command line rather than from the entity, unlike `new-feature`: a
        /// method's request is not a row. A `ListLakesRequest` carries a page token.
        #[arg(long = "field")]
        fields: Vec<String>,
        /// A response field, in the same spelling. Repeatable.
        ///
        /// Its own list because a method's answer is not its question. An empty response
        /// message is both legal and common, so there is no default to infer.
        #[arg(long = "response-field")]
        response_fields: Vec<String>,
    },
    /// Scaffold one more DTO on an existing feature, from its contract
    NewDomain {
        /// The service that owns the feature
        service: String,
        /// The feature, e.g. `lake`
        feature: String,
        /// The payload, in kebab-case, named for its RPC — `update-lake` reads
        /// `UpdateLakeRequest`
        name: String,
    },
    /// Install the schema plumbing into a service scaffolded without one
    AddDatabase {
        /// The service
        service: String,
    },
    /// Verify OIDC access tokens in a service, in place of the shipped verifier that accepts nobody
    ///
    /// Writes `crates/oidc` if the repository has none, then wires the service: an `[auth]`
    /// section whose validation stops startup without an issuer, the dependency, the verifier in
    /// `main.rs`, and a test that drives `Whoami` through the real router. Nothing changes in a
    /// product until this is run (`docs/adr/0027`). Readiness is not touched: a provider outage
    /// must not take replicas out of rotation.
    AddAuthOidc {
        /// The service
        service: String,
        /// The development issuer, written to `config/<service>.local.toml`
        ///
        /// Never the committed base file, which is baked into the image: a deployment names its
        /// own issuer in `APP__AUTH__ISSUER`, and the service refuses to start without one.
        #[arg(long)]
        issuer: Option<String>,
        /// What a token's `aud` must contain, when it is not the product's name
        ///
        /// Defaults to `[publish].project` in `versions.toml`.
        #[arg(long)]
        audience: Option<String>,
        /// Install the BFF (Backend For Frontend) for browser login through the service.
        ///
        /// Embeds the `bff` module in `crates/oidc`, wires `/auth/*` routes into the service,
        /// and adds cookie encryption key configuration.
        #[arg(long)]
        web: bool,
    },
    /// Start the next architecture decision record, numbered and wired into its index
    ///
    /// The number is the next free one (`crate::adr::next_number`), the filename is the title
    /// kebab-cased, the sections are the shape the records above it use, and the row in
    /// `docs/adr/README.md` is written in the same breath as the file — because a record whose
    /// index row silently failed to land is a record nobody reads.
    ///
    /// What it does not do is prevent two open branches from picking the same next free number.
    /// Nothing run in one branch can: neither branch knows the other exists. That collision is
    /// caught at merge by `decision_records_are_numbered`, which is the half of this ticket that
    /// earns its place.
    NewAdr {
        /// The title, as its `# NNNN.` heading should read. Kebab-cased into the filename.
        title: String,
        /// The date to stamp it with, as `YYYY-MM-DD`. Today, when omitted.
        ///
        /// For the common case of a record written after the decision it records: every amendment
        /// in `docs/adr/` is dated to the day it was measured, and backdating by editing the file
        /// afterwards is how a date gets copied from the record above it and wrong.
        #[arg(long)]
        date: Option<String>,
    },
}

/// What each check contributed, printed before the summary it adds up to.
///
/// Deliberately `{:>5}` and the name alone: the output is read by diffing two runs — this
/// repository against the copy `check-template` builds, or a branch against its base — and a
/// column that lines up diffs cleanly is worth more here than one that reads prettily.
///
/// The counts are not decomposed below the top level. `crate::gates::check` is one line
/// covering several kinds of assertion, and RT-14's misattributed 12/3 split lived inside
/// exactly that line, so there is a case for going further — but a breakdown of a breakdown
/// wants a shape this does not have yet, and guessing at one before anything needs it is how
/// the shape comes out wrong. Not yet, deliberately.
fn print_breakdown(title: &str, entries: &[(&'static str, usize)]) {
    println!("  {title}, by check:");
    for (name, count) in entries {
        println!("    {count:>5}  {name}");
    }
}

fn report(title: &str, checked: usize, problems: &[String]) -> bool {
    if problems.is_empty() {
        println!("  ✓ {title}: {checked} checks passed");
        return true;
    }
    println!("  ✗ {title}: {} of {checked} checks failed", problems.len());
    for problem in problems {
        println!("      • {problem}");
    }
    false
}

/// Print one check's breakdown when asked for it, then its summary, and say whether it passed.
///
/// Extracted from the dispatch below because the three check arms are the same three steps with
/// a different title, and `Check` is those arms twice with `&` rather than an early bail. A
/// fourth check should mean one more arm and not a fourth copy of the reporting shape — and the
/// breakdown half is the part worth keeping in one place, since `--breakdown` exists precisely
/// because attributing a moving count by patching this file and throwing the patch away was done
/// twice in one week (RT-16).
fn report_check(
    title: &str,
    verbose: bool,
    checked: usize,
    problems: &[String],
    breakdown: &[(&'static str, usize)],
) -> bool {
    if verbose {
        print_breakdown(title, breakdown);
    }
    report(title, checked, problems)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let breakdown = cli.breakdown;
    let recording = matches!(
        cli.command,
        Command::NewMigration { .. }
            | Command::NewFeature { .. }
            | Command::NewRpc { .. }
            | Command::NewDomain { .. }
            | Command::AddDatabase { .. }
            | Command::AddAuthOidc { .. }
    ) && repo::is_product(&repo::root());
    let edits_manifests = edits_manifests(&cli.command);
    if recording {
        recipe::read(&repo::root())?;
    }
    dispatch(cli.command, breakdown)?;
    // Refresh even if recording fails, and record even if resolution fails: the
    // generator's files already exist and a replay needs their invocation.
    let refreshed = if edits_manifests {
        cargo_lock::refresh(&repo::root())
    } else {
        Ok(())
    };
    if recording {
        recipe::append(
            &repo::root(),
            recipe::Tool::Xtask,
            // Check-only presentation flag; keep --issuer and all generation inputs verbatim.
            std::env::args()
                .skip(1)
                .filter(|arg| arg != "--breakdown")
                .collect(),
        )?;
    }
    refreshed
}

/// Source-only generators need no resolution; manifest writers must leave CI's
/// `--locked` workspace complete (RT-76).
const fn edits_manifests(command: &Command) -> bool {
    matches!(
        command,
        Command::NewFeature { .. } | Command::AddDatabase { .. } | Command::AddAuthOidc { .. }
    )
}

fn dispatch(command: Command, breakdown: bool) -> Result<()> {
    // An arm that is one call is one line. Dispatch stays under clippy's line limit:
    // recording belongs around the command, not duplicated in each generator arm.
    match command {
        Command::NewMigration {
            service,
            name,
            table,
            fields,
        } => {
            api::new_migration(&service, &name, table.as_deref(), &fields)?;
        }
        Command::NewFeature {
            service,
            name,
            table,
        } => {
            api::new_feature(&service, &name, table.as_deref())?;
        }
        Command::NewRpc {
            service,
            feature,
            name,
            fields,
            response_fields,
        } => {
            api::new_rpc(&service, &feature, &name, &fields, &response_fields)?;
        }
        Command::NewDomain {
            service,
            feature,
            name,
        } => {
            api::new_domain(&service, &feature, &name)?;
        }
        Command::AddDatabase { service } => api::add_database(&service)?,
        Command::AddAuthOidc {
            service,
            issuer,
            audience,
            web,
        } => api::add_auth_oidc(
            &service,
            &api::AuthOptions {
                issuer,
                audience,
                web,
            },
        )?,
        Command::NewAdr { title, date } => adr::new(&title, date.as_deref())?,
        Command::I18nResources { app, platform } => i18n_resources::generate(&app, platform)?,
        Command::Version { key } => {
            println!("{}", versions::get(&key)?);
        }
        Command::Deny { fix } => deny::run(fix)?,
        Command::AdvisoryPr => advisory_pr::run()?,
        Command::CheckVersions => {
            let r = versions::check()?;
            if !report_check("versions", breakdown, r.checked, &r.problems, &r.breakdown) {
                bail!("version drift detected");
            }
        }
        Command::CheckInvariants => {
            let r = invariants::check()?;
            if !report_check(
                "invariants",
                breakdown,
                r.checked,
                &r.problems,
                &r.breakdown,
            ) {
                bail!("layout invariants violated");
            }
        }
        Command::Check => {
            let v = versions::check()?;
            let i = invariants::check()?;
            // `&` and not `&&`: both reports are printed, because a repository with version drift
            // and a broken layout has two things to fix and one print of each is the whole point
            // of running them together.
            let ok = report_check("versions", breakdown, v.checked, &v.problems, &v.breakdown)
                & report_check(
                    "invariants",
                    breakdown,
                    i.checked,
                    &i.problems,
                    &i.breakdown,
                );
            if !ok {
                bail!("repository checks failed");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("xtask").chain(args.iter().copied()))
            .expect("the arguments parse")
            .command
    }

    #[test]
    fn every_generator_that_writes_a_manifest_refreshes_the_lock() {
        for args in [
            &["new-feature", "api", "lake"][..],
            &["add-database", "api"],
            &["add-auth-oidc", "api"],
        ] {
            assert!(
                edits_manifests(&command(args)),
                "{args:?} writes a Cargo.toml"
            );
        }
    }

    #[test]
    fn source_only_generators_and_checks_leave_the_lock_alone() {
        for args in [
            &["new-migration", "api", "create_lakes"][..],
            &["new-rpc", "api", "lake", "archive-lake"],
            &["new-domain", "api", "lake", "update-lake"],
            &["check"],
        ] {
            assert!(
                !edits_manifests(&command(args)),
                "{args:?} writes no Cargo.toml"
            );
        }
    }
}
