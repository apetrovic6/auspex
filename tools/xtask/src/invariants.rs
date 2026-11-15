//! Structural rules that keep the monorepo's conventions true.
//!
//! These exist because the conventions are enforced by *layout*, not by a registry: a
//! crate in the wrong directory is silently not a workspace member, and a Justfile with a
//! stale relative `mod` path only fails when someone runs that one recipe. Both are cheap
//! to detect and confusing to debug, so they are checked here and in CI.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{adr, rename, repo, swift_keywords};

/// Globs the root `Cargo.toml` must always contain.
///
/// Most are unconditional because their directories always exist: `crates/` and `tools/`
/// ship with a README that keeps the glob valid while empty. Two groups are not:
/// [`APP_MEMBER_GLOBS`], required only while a Crux app exists, and
/// [`SERVICE_MEMBER_GLOBS`], required only while `services/` does.
/// `pub` rather than module-private because `versions` needs the same set: a version
/// requirement can sit in any member manifest, and iterating the members is how it finds them
/// all. (Plain `pub(crate)` here is a clippy error, not a style choice — `redundant_pub_crate`,
/// since `mod invariants` is itself private.)
pub const MEMBER_GLOBS: &[&str] = &[
    "crates/*",
    "services/*",
    "tools/*",
    "apps/*/core",
    "apps/*/shells/*",
];

/// The members of [`MEMBER_GLOBS`] that exist only because an app does.
///
/// Required whenever `apps/` holds a Crux app ([`repo::has_crux_apps`]), and not otherwise: a product
/// generated with `new --drop-example` has none, and `drop_example_app` removes exactly these
/// because Cargo refuses a glob matching nothing. `just add crux-app` puts them back.
const APP_MEMBER_GLOBS: &[&str] = &["apps/*/core", "apps/*/shells/*"];

/// Globs that may or may not be present.
///
/// `apps/*/app` covers single-crate applications. It cannot ship unconditionally: Cargo
/// rejects a glob matching nothing, and `apps/<name>/app` does not exist until the first
/// such app does. `xtask new` adds it then, and removing the last such app means removing
/// it again — see `Archetype::member_glob`.
///
/// `apps/*/client` is the same shape: shell-side code an app's shells share, which most
/// apps will not have. It is in the manifest today because the counter app has one.
const OPTIONAL_MEMBER_GLOBS: &[&str] = &["apps/*/app", "apps/*/client"];

/// The members of [`MEMBER_GLOBS`] that exist only because `services/` does.
///
/// Required whenever the directory is there, and not otherwise. The template ships `services/`
/// with a README guard so the glob is valid before the first service, but a product with no
/// backend at all — one that talks to its machines over SSH, say — removes the directory and
/// the glob together, because Cargo refuses a glob matching nothing. Keying on the directory
/// rather than on a crate inside it keeps the template's own shape checked exactly as before:
/// a `services/` holding only its README still needs the glob, and a crate dropped into a
/// `services/` the manifest does not glob is still reported, rather than silently not being a
/// workspace member.
const SERVICE_MEMBER_GLOBS: &[&str] = &["services/*"];

/// Directories whose `*` glob must always match something, guarded by a README.
///
/// `services` is conditional, for the reason on [`SERVICE_MEMBER_GLOBS`]: its guard is only
/// demanded while the directory or its glob exists. See [`category_dirs_have_guards`].
const CATEGORY_DIRS: &[&str] = &["crates", "services", "tools"];

/// Whether this tree has a `services/` directory, and therefore needs [`SERVICE_MEMBER_GLOBS`].
fn has_services_dir(root: &Path) -> bool {
    root.join("services").is_dir()
}

/// The `[workspace].members` globs as written, or `None` when the manifest has none.
fn manifest_member_globs(root: &Path) -> Result<Option<Vec<String>>> {
    let manifest: toml::Value = toml::from_str(
        &std::fs::read_to_string(root.join("Cargo.toml")).context("reading Cargo.toml")?,
    )?;
    Ok(manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        }))
}

/// Crates a Crux core must never depend on.
///
/// The core stays domain-level (ADR 0002): it asks for `CounterApi::Increment`, and turning
/// that into a call is each shell's job. Everything here is on the other side of that line —
/// the wire, the transport, the database — plus `identity`, which is the reason this check
/// exists at all. "The core needs to know who is logged in" is the most plausible wrong turn
/// available to whoever wires a real authentication into a product, and it stays invisible
/// until the core stops compiling for wasm or a mobile binary grows a protocol stack.
const CORE_FORBIDDEN_DEPENDENCIES: &[&str] = &[
    "axum",
    "connect-web",
    "connectrpc",
    "http",
    "hyper",
    "identity",
    "proto",
    "reqwest",
    "sea-orm",
    "tower",
    "tower-http",
    // auspex's own boundary (docs/plan.md: "core never sees effects"): the adapters and the
    // runtime they run on. Before this entry, the only thing that kept them out of
    // auspex's core was its wasm32 pass, and only by accident. That pass is gone, because
    // auspex has no web shell.
    "auspex-effects",
    "tokio",
];

/// The dependency tables a forbidden crate could be smuggled in through.
const DEPENDENCY_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

/// Where a core lives: every app's, and the one `just add crux-app` renders.
const CORE_GLOBS: &[&str] = &["apps/*/core", "templates/crux-app/apps/__name__/core"];

pub struct Report {
    pub checked: usize,
    pub problems: Vec<String>,
    /// What each check contributed to `checked`, in the order they run.
    ///
    /// The total is what this repository's practice leans on — every change reports its
    /// delta and a reviewer uses it to notice a check added or lost by accident — and until
    /// RT-16 a number that moved unexpectedly could only be attributed by patching this file
    /// and throwing the patch away, which was done twice in one week. This is the same
    /// information kept.
    pub breakdown: Vec<(&'static str, usize)>,
}

pub fn check() -> Result<Report> {
    let root = repo::root();
    let mut problems = Vec::new();
    let mut checked = 0;

    // Every check runs through `step!`, which calls it and records what it added to
    // `checked`. The label comes from `stringify!` on the path rather than a string beside
    // it, so a renamed check cannot keep an old name in the output — the failure mode of
    // the throwaway version of this (RT-9's `f7f53d0`), which carried 26 hand-written
    // literals.
    //
    // A check called directly rather than through the macro would still reach the total and
    // contribute no line. Nothing in the language prevents that; the test
    // `the_breakdown_accounts_for_every_check` does, by refusing a total the lines do not
    // sum to. That test is what makes the macro non-optional, and is the reason this is a
    // kept feature rather than a patch somebody reapplies.
    //
    // `fallible` rather than `try` on the second arm: `try` is a reserved keyword and will
    // not parse as a matcher token.
    let mut breakdown: Vec<(&'static str, usize)> = Vec::new();
    macro_rules! step {
        ($f:path) => {{
            let before = checked;
            $f(&root, &mut checked, &mut problems);
            breakdown.push((stringify!($f), checked - before));
        }};
        (fallible $f:path) => {{
            let before = checked;
            $f(&root, &mut checked, &mut problems)?;
            breakdown.push((stringify!($f), checked - before));
        }};
    }

    step!(fallible workspace_globs_match_manifest);
    step!(fallible category_dirs_have_guards);
    step!(fallible members_inherit_workspace_lints);
    step!(fallible app_metadata_is_valid);
    step!(platforms_contain_no_crates);
    step!(apps_have_justfiles);
    step!(fallible applications_are_not_publishable);
    step!(fallible cores_stay_domain_level);
    step!(fallible just_module_paths_resolve);
    step!(fallible crate::gates::check);
    step!(fallible crate::api::invariants::check);
    step!(locale_files_are_complete);
    step!(fallible apps_have_locale_namespaces);
    step!(translate_with_arguments_match_the_catalog);
    step!(cldr_data_stays_sliced);
    step!(plural_families_reach_a_shell);
    step!(instants_are_formatted_at_the_edge);
    step!(generated_resources_are_wired);
    step!(gradle_build_cache_is_on);
    step!(gpui_labels_are_translated);
    step!(fallible native_identifiers_agree);
    step!(app_paths_outside_apps_are_reachable);
    step!(fallible decision_records_are_indexed);
    step!(fallible decision_records_are_numbered);
    step!(fallible template_only_content_is_absent);
    step!(crate::recipe::check);
    step!(fallible adr_references_do_not_dangle);
    step!(fallible nix_modules_are_reachable);
    step!(fallible crate::locks::check);
    step!(config_keys_avoid_the_reserved_file_suffix);
    step!(telemetry_guards_are_bound);
    step!(the_scheduled_checkout_persists_no_credentials);
    step!(the_publish_step_receives_its_secrets);
    step!(swift_keywords::facet_names_are_legal_swift_identifiers);

    Ok(Report {
        checked,
        problems,
        breakdown,
    })
}

/// The glob list this file reasons about must be the one Cargo actually uses.
///
/// Checked as containment rather than equality, because some globs are conditional: every
/// required glob must be present, and every present glob must be one this tool knows
/// about. An unknown glob means the checks below silently skip a whole category of crate.
fn workspace_globs_match_manifest(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    *checked += 1;
    let Some(members) = manifest_member_globs(root)? else {
        problems.push("Cargo.toml: [workspace].members is missing".to_owned());
        return Ok(());
    };
    let actual: Vec<&str> = members.iter().map(String::as_str).collect();

    let has_apps = repo::has_crux_apps(root);
    let has_services = has_services_dir(root);
    for required in MEMBER_GLOBS {
        if APP_MEMBER_GLOBS.contains(required) && !has_apps {
            continue;
        }
        if SERVICE_MEMBER_GLOBS.contains(required) && !has_services {
            continue;
        }
        if !actual.contains(required) {
            problems.push(format!(
                "Cargo.toml [workspace].members is missing the required glob {required:?}"
            ));
        }
    }
    for present in &actual {
        if !MEMBER_GLOBS.contains(present) && !OPTIONAL_MEMBER_GLOBS.contains(present) {
            problems.push(format!(
                "Cargo.toml [workspace].members has glob {present:?}, which xtask does not \
                 know about — add it to MEMBER_GLOBS or OPTIONAL_MEMBER_GLOBS so the layout \
                 checks cover it"
            ));
        }
    }
    Ok(())
}

/// A `members` glob that matches nothing is a hard Cargo error, so each category
/// directory keeps a file for the glob to match while it holds no crates.
///
/// `services/` is the exception: a product with no backend has neither the directory nor its
/// glob ([`SERVICE_MEMBER_GLOBS`]), and there is nothing to guard. It is skipped only when
/// both are absent — a `services/*` glob with no directory behind it is the hard Cargo error
/// this check exists to name, and is still reported.
fn category_dirs_have_guards(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let globbed = manifest_member_globs(root)?.unwrap_or_default();
    for dir in CATEGORY_DIRS {
        let path = root.join(dir);
        let glob = format!("{dir}/*");
        if SERVICE_MEMBER_GLOBS.contains(&glob.as_str())
            && !path.is_dir()
            && !globbed.contains(&glob)
        {
            continue;
        }
        *checked += 1;
        if !path.is_dir() {
            problems.push(format!(
                "{dir}/ is missing — the `{dir}/*` glob will not resolve"
            ));
        } else if !path.join("README.md").is_file() {
            problems.push(format!(
                "{dir}/README.md is missing — it is the guard that keeps `{dir}/*` valid \
                 while the directory holds no crates"
            ));
        }
    }
    Ok(())
}

/// Workspace members exempt from `[lints] workspace = true`, with the reason each one cannot
/// inherit and what it declares instead.
///
/// An exemption is deliberately narrow and *reported* by the check below, because "this crate
/// opts out of the workspace lint set" is the exact sentence that turns a linted workspace
/// into an unlinted one. So an exempt crate must declare a `[lints.rust]` table of its own —
/// exempt from inheriting, never from being linted — and an exemption naming a directory that
/// is no longer a workspace member is itself a problem.
///
/// `crates/i18n-data` is the only one, and it exists because of two rules that collide: the
/// generated CLDR payload is databake output, and databake materialises a payload it verified
/// at generation time with `ZeroSlice::from_bytes_unchecked` — 174 `unsafe` blocks for nine
/// markers. The workspace lints set `unsafe_code = forbid`; Cargo refuses to let a member
/// inherit `[workspace.lints]` and override a lint from it in the same manifest; and an
/// in-source `#[allow(unsafe_code)]` cannot lift a `forbid`. There is no way to have the
/// generated data and the workspace lint set in one crate.
///
/// The alternative was to relax `unsafe_code` in `i18n` and keep the data in there, which was
/// rejected in ADR 0021: it would put permissive unsafe over the hand-written formatting code
/// the rule exists to protect, silently, for whoever edits that file next. An exception with
/// its own directory has a boundary; a lint flag on a crate does not.
const LINT_INHERITANCE_EXEMPTIONS: &[(&str, &str)] = &[(
    "crates/i18n-data",
    "the workspace forbids `unsafe` and its committed CLDR payload is databake output, which \
     deserialises itself with `from_bytes_unchecked` (ADR 0021) — it declares `[lints.rust]` \
     instead of inheriting",
)];

/// Lints are centralised in `[workspace.lints]`; a crate that forgets to opt in is
/// silently unlinted (which is how the desktop shell accumulated clippy errors).
fn members_inherit_workspace_lints(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let mut exempt_seen = BTreeSet::new();

    for glob in MEMBER_GLOBS.iter().chain(OPTIONAL_MEMBER_GLOBS) {
        for dir in repo::expand(root, glob) {
            let manifest_path = dir.join("Cargo.toml");
            if !manifest_path.is_file() {
                continue;
            }
            *checked += 1;
            let relative = repo::rel(root, &dir);
            let source = std::fs::read_to_string(&manifest_path)
                .with_context(|| format!("reading {}", repo::rel(root, &manifest_path)))?;
            let manifest: toml::Value = toml::from_str(&source)
                .with_context(|| format!("parsing {}", repo::rel(root, &manifest_path)))?;
            let inherits = manifest
                .get("lints")
                .and_then(|l| l.get("workspace"))
                .and_then(toml::Value::as_bool)
                == Some(true);
            if inherits {
                continue;
            }
            if let Some((_, why)) = LINT_INHERITANCE_EXEMPTIONS
                .iter()
                .find(|(exempt, _)| *exempt == relative)
            {
                exempt_seen.insert(relative.clone());
                // The exemption is from *inheriting*, never from being linted: an exempt crate
                // has to say what its own lint set is, in the manifest, or the exemption is
                // indistinguishable from the omission this function exists to catch.
                let declares = manifest
                    .get("lints")
                    .and_then(|l| l.get("rust"))
                    .and_then(toml::Value::as_table)
                    .is_some_and(|table| !table.is_empty());
                if !declares {
                    problems.push(format!(
                        "{relative}/Cargo.toml: exempted from `[lints] workspace = true` ({why}) \
                         but declares no `[lints.rust]` of its own — an exempt crate must state \
                         the lint set it uses instead",
                    ));
                }
                continue;
            }
            problems.push(format!(
                "{}: missing `[lints] workspace = true` — this crate is not linted",
                repo::rel(root, &manifest_path)
            ));
        }
    }

    // A stale exemption is a slow leak: the crate is deleted or fixed, and the following
    // generation keeps a hole in the lint set that nothing occupies.
    for (exempt, _) in LINT_INHERITANCE_EXEMPTIONS {
        if !exempt_seen.contains(*exempt) {
            problems.push(format!(
                "{exempt}: listed in LINT_INHERITANCE_EXEMPTIONS but not a workspace member — \
                 it inherits the workspace lints today, so remove the exemption"
            ));
        }
    }
    Ok(())
}

/// The `[package.metadata.app]` table is the pipeline's discovery contract —
/// nix/pipeline/discovery.nix reads it to decide which crates get the wasm pass, the
/// native-OFF clippy/deps pass, trunk, and which apps have platforms. A misspelled kind
/// or key is a crate that silently does not join the passes it meant to, so both are
/// rejected here rather than guessed at. Every crate under `apps/` and `services/` must
/// carry a kind; a crate under `crates/` or `tools/` is a plain library and carries none.
fn app_metadata_is_valid(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    // Read every member once before resolving references, including forward references.
    let mut members = Vec::new();
    for glob in MEMBER_GLOBS.iter().chain(OPTIONAL_MEMBER_GLOBS) {
        for dir in repo::expand(root, glob) {
            let manifest_path = dir.join("Cargo.toml");
            if !manifest_path.is_file() {
                continue;
            }
            let source = std::fs::read_to_string(&manifest_path)
                .with_context(|| format!("reading {}", repo::rel(root, &manifest_path)))?;
            let manifest: toml::Value = toml::from_str(&source)
                .with_context(|| format!("parsing {}", repo::rel(root, &manifest_path)))?;
            members.push((dir, manifest));
        }
    }
    for (dir, manifest) in &members {
        *checked += 1;
        let relative = repo::rel(root, dir);
        let required = relative.starts_with("apps/") || relative.starts_with("services/");
        let metadata = manifest
            .get("package")
            .and_then(|p| p.get("metadata"))
            .and_then(|m| m.get("app"));
        let Some(table) = metadata.and_then(toml::Value::as_table) else {
            if required {
                problems.push(format!(
                    "{relative}/Cargo.toml: missing `[package.metadata.app].kind` — every crate \
                     under apps/ or services/ must declare how the pipeline treats it, or it \
                     silently drops out of the wasm/native-OFF/trunk passes",
                ));
            }
            continue;
        };
        metadata_fields_are_valid(&relative, table, required, problems);
        web_bundle_metadata_is_valid(root, dir, table, &members, problems);
    }
    Ok(())
}

fn metadata_fields_are_valid(
    relative: &str,
    table: &toml::map::Map<String, toml::Value>,
    required: bool,
    problems: &mut Vec<String>,
) {
    const KINDS: &[&str] = &["core", "client", "shell", "app", "service", "worker"];
    const KEYS: &[&str] = &[
        "kind",
        "wasm",
        "host",
        "trunk",
        "native-feature",
        "spa",
        "web-shell",
    ];
    for key in table.keys() {
        if !KEYS.contains(&key.as_str()) {
            problems.push(format!(
                "{relative}/Cargo.toml: unknown `[package.metadata.app]` key `{key}` — \
                 known keys are {} (see nix/pipeline/discovery.nix)",
                KEYS.join(", ")
            ));
        }
    }
    match table.get("kind").and_then(toml::Value::as_str) {
        None if required => problems.push(format!(
            "{relative}/Cargo.toml: missing `[package.metadata.app].kind` — every crate \
             under apps/ or services/ must declare one of {}",
            KINDS.join(", ")
        )),
        Some(kind) if !KINDS.contains(&kind) => problems.push(format!(
            "{relative}/Cargo.toml: unknown `[package.metadata.app]` kind `{kind}` — \
             known kinds are {}",
            KINDS.join(", ")
        )),
        _ => {}
    }
    // Type-check discovery facts rather than treating `wasm = "yes"` as truthy.
    for key in ["wasm", "host", "trunk"] {
        if table.contains_key(key) && table.get(key).and_then(toml::Value::as_bool).is_none() {
            problems.push(format!(
                "{relative}/Cargo.toml: `[package.metadata.app].{key}` must be a boolean"
            ));
        }
    }
    if table.contains_key("native-feature")
        && table
            .get("native-feature")
            .and_then(toml::Value::as_str)
            .is_none()
    {
        problems.push(format!(
            "{relative}/Cargo.toml: `[package.metadata.app].native-feature` must be a string"
        ));
    }
}

fn web_bundle_metadata_is_valid(
    root: &Path,
    dir: &Path,
    table: &toml::map::Map<String, toml::Value>,
    members: &[(PathBuf, toml::Value)],
    problems: &mut Vec<String>,
) {
    let relative = repo::rel(root, dir);
    // A missing fallback directory otherwise becomes an opaque Nix path error at build time.
    if let Some(spa) = table.get("spa") {
        match spa.as_str() {
            None => problems.push(format!(
                "{relative}/Cargo.toml: `[package.metadata.app].spa` must be a string naming \
                 a directory inside the crate"
            )),
            Some(path) if !dir.join(path).is_dir() => problems.push(format!(
                "{relative}/Cargo.toml: `[package.metadata.app].spa = {path:?}` is not a \
                 directory — the pipeline installs it into the package and the image bakes \
                 it, so a missing bundle is a build that fails far from this line"
            )),
            Some(_) => {}
        }
    }
    // Discovery and rename/drop consume this key only on service members.
    if !relative.starts_with("services/") {
        return;
    }
    let Some(web_shell) = table.get("web-shell") else {
        return;
    };
    let Some(name) = web_shell.as_str() else {
        problems.push(format!(
            "{relative}/Cargo.toml: `[package.metadata.app].web-shell` must be a string \
             naming a workspace member with `trunk = true`"
        ));
        return;
    };
    let target = members.iter().find(|(_, manifest)| {
        manifest
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
            == Some(name)
    });
    match target {
        None => problems.push(format!(
            "{relative}/Cargo.toml: `[package.metadata.app].web-shell = {name:?}` names no \
             workspace member — refusing to silently serve the demo bundle"
        )),
        Some((_, manifest))
            if manifest
                .get("package")
                .and_then(|p| p.get("metadata"))
                .and_then(|m| m.get("app"))
                .and_then(|a| a.get("trunk"))
                .and_then(toml::Value::as_bool)
                != Some(true) =>
        {
            problems.push(format!(
                "{relative}/Cargo.toml: `[package.metadata.app].web-shell = {name:?}` must \
                 name a workspace member with `trunk = true`"
            ));
        }
        Some(_) => {}
    }
}

/// A Cargo.toml under `platforms/` would not be a workspace member (no glob reaches it)
/// and would be silently excluded from every build, lint and test.
fn platforms_contain_no_crates(root: &Path, checked: &mut usize, problems: &mut Vec<String>) {
    for platforms in repo::expand(root, "apps/*/platforms") {
        *checked += 1;
        let mut found = Vec::new();
        repo::find_files(&platforms, "Cargo.toml", &mut found);
        for manifest in found {
            problems.push(format!(
                "{}: Cargo.toml under platforms/ is NOT a workspace member — move the crate \
                 to apps/<app>/shells/ if it should be built by cargo",
                repo::rel(root, &manifest)
            ));
        }
    }
}

/// An app needs a Justfile only when it has work the workspace pass cannot do.
///
/// Aggregate recipes discover modules by globbing `apps/*/Justfile`, but that pass exists
/// for what cargo alone cannot reach: wasm32 clippy for a web shell, Gradle lint,
/// swiftlint. A single-crate app has none of that — it is an ordinary member of
/// `default-members` and is fully covered by the workspace lint and test pass, so a
/// Justfile for it would be boilerplate restating what already happens.
fn apps_have_justfiles(root: &Path, checked: &mut usize, problems: &mut Vec<String>) {
    for app in repo::expand(root, "apps/*") {
        // A Trunk.toml means a wasm pipeline: `trunk build` and a wasm32 clippy pass,
        // neither of which the workspace cargo pass reaches — so a single-crate web app
        // needs a Justfile even though a single-crate CLI does not.
        let mut trunk = Vec::new();
        repo::find_files(&app, "Trunk.toml", &mut trunk);
        let needs_one =
            app.join("shells").is_dir() || app.join("platforms").is_dir() || !trunk.is_empty();
        if !needs_one {
            continue;
        }
        *checked += 1;
        if !app.join("Justfile").is_file() {
            problems.push(format!(
                "{}: has shells/, platforms/ or a Trunk.toml but no Justfile — \
                 `just check`/`just ci` would skip its wasm, Gradle and Xcode steps \
                 entirely",
                repo::rel(root, &app)
            ));
        }
    }
}

/// Application crates must set `publish = false`.
///
/// Two reasons: nothing under `apps/` or `services/` belongs on crates.io, and
/// cargo-deny's wildcard rule rejects `{ path = ... }` dependencies on any crate it
/// considers publishable — so a missing `publish = false` fails `just deny` with a
/// message about wildcards that does not obviously point here.
fn applications_are_not_publishable(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    for glob in [
        "apps/*/core",
        "apps/*/client",
        "apps/*/shells/*",
        "apps/*/app",
        "services/*",
    ] {
        for dir in repo::expand(root, glob) {
            let manifest_path = dir.join("Cargo.toml");
            if !manifest_path.is_file() {
                continue;
            }
            *checked += 1;
            let source = std::fs::read_to_string(&manifest_path)
                .with_context(|| format!("reading {}", repo::rel(root, &manifest_path)))?;
            let manifest: toml::Value = toml::from_str(&source)
                .with_context(|| format!("parsing {}", repo::rel(root, &manifest_path)))?;
            let publishable = manifest
                .get("package")
                .and_then(|p| p.get("publish"))
                .and_then(toml::Value::as_bool)
                != Some(false);
            if publishable {
                problems.push(format!(
                    "{}: application crates must set `publish = false`",
                    repo::rel(root, &manifest_path)
                ));
            }
        }
    }
    Ok(())
}

/// A Crux core depends on nothing that knows about the wire.
///
/// The enforceable half of ADR 0002, which was prose until `crates/identity` gave it a shape
/// worth reaching for: a core that grows an `identity` (or `proto`, or `connectrpc`)
/// dependency still compiles, still passes its own tests, and quietly ends the property that
/// lets five shells share one set of decisions. The transport lives in `apps/<app>/client`
/// and the platform clients, which is where a credential belongs too.
///
/// Stated limit: this reads the three dependency tables and nothing else, so a
/// `[target.'cfg(..)'.dependencies]` entry would slip past. No core in this repository uses
/// one, and a scan that pretended to cover them would be guessing at a TOML shape it does not
/// parse.
fn cores_stay_domain_level(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    for glob in CORE_GLOBS {
        for dir in repo::expand(root, glob) {
            let manifest_path = dir.join("Cargo.toml");
            if !manifest_path.is_file() {
                continue;
            }
            let source = std::fs::read_to_string(&manifest_path)
                .with_context(|| format!("reading {}", repo::rel(root, &manifest_path)))?;
            let manifest: toml::Value = toml::from_str(&source)
                .with_context(|| format!("parsing {}", repo::rel(root, &manifest_path)))?;

            for table in DEPENDENCY_TABLES {
                let Some(dependencies) = manifest.get(table).and_then(toml::Value::as_table) else {
                    continue;
                };
                for (key, spec) in dependencies {
                    *checked += 1;
                    // The package, not the key: `fx = { package = "auspex-effects" }` is the
                    // same dependency under another name.
                    let dependency = spec
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(key);
                    if CORE_FORBIDDEN_DEPENDENCIES.contains(&dependency) {
                        problems.push(format!(
                            "{}: a Crux core must not depend on `{dependency}` — the core stays \
                             domain-level (ADR 0002) and the wire lives in apps/<app>/client. \
                             A credential, a path and an encoding are all transport; if the core \
                             needs to know who is logged in, it needs an operation that says so \
                             in its own vocabulary, fulfilled by each shell.",
                            repo::rel(root, &manifest_path)
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Gradle's build cache is on, in every Android project and in the skeleton.
///
/// The cache lives at `~/.gradle/caches/build-cache-1`, which persists per runner, while the
/// checkout does not — so without it every CI run recompiles every Gradle task from nothing,
/// and with it a run whose inputs have not moved restores task outputs instead. Measured on
/// the counter app's full lint/build/test path with a clean project and a warm dependency
/// cache, at two cores to match a capped runner: 97.9 s with the cache off, 36.5 s with it on
/// and populated — 2.7x, and 37 of the lint invocation's 73 tasks restored rather than run.
///
/// The skeleton is checked beside the apps rather than separately, because `add-platform`
/// copies from it: a skeleton without the line ships every future product a project that
/// rebuilds from nothing, and nothing inside this repository would notice, since this
/// repository's own app would still have it. That is the same reason
/// `generated_resources_are_wired` walks both.
///
/// `org.gradle.parallel` is deliberately NOT asserted, and was measured rather than assumed:
/// 35.0 s against 36.5 s at two cores, inside run-to-run variance (a single earlier pair had
/// it slower, 38.4 s against 37.8 s), for a flag this file's own `gradle.properties` comment
/// says is only for decoupled projects. `:app` depends on `:shared`, so there is little to
/// parallelise and something to get wrong.
fn gradle_build_cache_is_on(root: &Path, checked: &mut usize, problems: &mut Vec<String>) {
    let mut projects = repo::expand(root, "apps/*/platforms/android");
    let skeleton = root.join("templates/platform-android/apps/__name__/platforms/android");
    if skeleton.is_dir() {
        projects.push(skeleton.clone());
    }

    for project in projects {
        let path = project.join("gradle.properties");
        let Ok(text) = std::fs::read_to_string(&path) else {
            // A project with no gradle.properties has no cache setting to be wrong about,
            // and `platform_templates_are_shaped_right` is what notices a skeleton that is
            // missing files it should have.
            continue;
        };
        *checked += 1;

        // Parsed as a property rather than grepped, so a commented-out line — which is how
        // this file ships `org.gradle.parallel` — does not read as enabled.
        let enabled = text.lines().any(|line| {
            let line = line.trim();
            !line.starts_with('#') && !line.starts_with('!') && line == "org.gradle.caching=true"
        });
        if !enabled {
            problems.push(format!(
                "{}: no uncommented `org.gradle.caching=true`, so every Gradle task is rebuilt \
                 from nothing on every run. The build cache lives in ~/.gradle, which persists \
                 per runner while the checkout does not — measured 2.7x on this project's \
                 lint/build/test path at two cores. Add the line here{}.",
                repo::rel(root, &path),
                if path.starts_with(&skeleton) {
                    " and in every app's copy: `add-platform` copies the skeleton, so a product \
                     scaffolded from this one inherits whatever it says"
                } else {
                    ""
                },
            ));
        }
    }
}

/// The generated localization resources are compiled in, everywhere.
///
/// Three rules, all of the "this directory has content, so it must also have X"
/// shape of `apps_have_justfiles`, because the failure they prevent has no other
/// symptom. `cargo xtask i18n-resources` writes the resource files and prints a
/// checkmark; if nothing consumes them the app renders the fallback language
/// forever, on every device, with nothing failing anywhere — the same silent
/// shape as a missing locale file, one step further down the pipeline.
///
/// - Every Gradle shared module that compiles the generated Kotlin (`kotlin.srcDirs`
///   reaching `../generated`) must also declare `res.srcDirs` for the generated `res/`.
///   Apps and the `templates/platform-android` skeleton alike: `add-platform` copies
///   from either, so a skeleton missing the line reproduces the bug in every future app.
/// - Every `project.yml` with a generated `Shared` package must list
///   `Localizable.xcstrings` under its sources — same rule on the platform where CI
///   cannot build to find out.
/// - The generated Android resource names must not collide with the committed ones.
///   The generator refuses to write past a collision, but only for the app it is
///   regenerating, and only when someone regenerates; this checks every app in the
///   repository on every `cargo xtask check`, reusing the emitter's own rules rather
///   than restating them. Per app, and with the same per-app filter the generator applies:
///   a check that runs the unfiltered table asks an app whether its resources collide with
///   keys that app will never receive.
fn generated_resources_are_wired(root: &Path, checked: &mut usize, problems: &mut Vec<String>) {
    // Gradle: the marker of a boltffi shared module is the generated Kotlin srcDir.
    let mut gradle = Vec::new();
    repo::find_files(root, "build.gradle.kts", &mut gradle);
    for manifest in gradle {
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        if !text.contains("kotlin.srcDirs(") || !text.contains("${projectDir}/../generated") {
            continue;
        }
        *checked += 1;
        if !text.contains("res.srcDirs(\"${projectDir}/../generated/res\")") {
            problems.push(format!(
                "{}: compiles the generated Kotlin but not the generated resources — \
                 `cargo xtask i18n-resources` writes them and this module ignores them, so \
                 the app renders the fallback language forever with nothing failing. Add \
                 `res.srcDirs(\"${{projectDir}}/../generated/res\")`",
                repo::rel(root, &manifest)
            ));
        }
    }

    // XcodeGen: the marker of a boltffi Apple project is the generated Shared package.
    let mut specs = Vec::new();
    repo::find_files(root, "project.yml", &mut specs);
    for spec in specs {
        let Ok(text) = std::fs::read_to_string(&spec) else {
            continue;
        };
        if !text.contains("generated/Shared") {
            continue;
        }
        *checked += 1;
        if !text.contains("generated/Localizable.xcstrings") {
            problems.push(format!(
                "{}: has a generated Shared package but no generated String Catalog under \
                 `sources` — the catalog is produced by `cargo xtask i18n-resources` and \
                 never compiled in, so the app has no translations to pick from and nothing \
                 says so",
                repo::rel(root, &spec)
            ));
        }
    }

    // Collisions, against every app rather than only the one being regenerated.
    let full = crate::i18n_resources::translation_table();
    for app in repo::expand(root, "apps/*") {
        let Some(name) = app.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let res = app.join("platforms/android/app/src/main/res");
        if !res.is_dir() {
            continue;
        }
        *checked += 1;
        let committed = match crate::i18n_resources::committed_resource_names(&res) {
            Ok(names) => names,
            Err(problem) => {
                problems.push(format!("{problem}"));
                continue;
            }
        };
        // Filtered with the app this loop is already iterating, which is the whole point of
        // `for_app`: unfiltered, this asks `billing` whether its committed `res/` collides
        // with keys `billing` will never be given, and reports a collision that could not
        // ship while crowding out the ones that could.
        let table = i18n_keygen::resources::for_app(&full, name);

        // Running the emitter is the check: it refuses on any collision, generated or
        // committed. The output is discarded; if it can be produced, it is safe to.
        if let Err(collision) =
            i18n_keygen::resources::android(&table, i18n::FALLBACK_LOCALE, &committed)
        {
            problems.push(format!(
                "{}: {}",
                repo::rel(root, &res.join("values")),
                collision
            ));
        }
    }
}

/// A gpui `.label(..)` never takes a string literal.
///
/// The one construct in this repository where a literal is unambiguously a word the user
/// reads. In gpui-kit `Button::new(id)` takes the element id — which stays a literal on
/// purpose, since translating it renames the element every time the locale changes — and
/// `.label(..)` takes the text. ADR 0014 declined a general "no user-facing string literal
/// in view code" scan because a text scan cannot tell `"Increment"` from a CSS class or a
/// fixture; this one does not have to guess, because the framework already split the two
/// into separate calls. It is deliberately narrow for the same reason: the rule is only
/// sound where a framework makes that distinction, so it covers `.label(` and nothing else.
///
/// It exists because the crux-app template's desktop skeleton still had them. That file
/// dates from 71aa169 (2026-09-24), five days before `crates/i18n` existed (b5b8326) and
/// before 9aae188 made the example app's tui and desktop read the catalog; nothing has
/// visited the skeleton since, because nothing compiles `templates/` (the workspace
/// `exclude`s it), `just new` copies it verbatim, and a hardcoded label is a defect that
/// builds and renders. Same silent shape as the missing resource wiring that
/// `generated_resources_are_wired` was written for, one file further along — and the same
/// promise: `fcc18fd` ported the platform skeletons so that "every future app" would read
/// the catalog. This covers the shells that commit's message did not reach.
///
/// Stated limits, both of them self-inflicted and both cheap to state:
///
/// - An occurrence on a line that *begins* with `//` is prose, not a call site, so it is
///   skipped — the guard and its reasoning are `telemetry_guards_are_bound`'s. Needed, not
///   merely tidy: this check's own doc comment below quotes `.label(..)`. Only a line that
///   begins with one: a trailing comment is not lexed, for the reason under the limits below.
/// - An occurrence with an odd number of `"` before it on its own line is inside a string
///   literal. Also needed, not merely tidy: the code that searches, and the message it
///   prints, both contain the needle, so without this the check flags its own file. A real
///   call site is unaffected — `Button::new("reset").label(…)` has two quotes before `.label(`,
///   and two is even.
///
/// What is out of reach, and stays out honestly: a label assembled by `concat!` or a macro,
/// a multi-line string literal, and a label produced one call away from the widget. Those
/// need semantic analysis; every scan in this file has the same boundary. A label written on
/// the line *below* `.label(` is not out of reach — the argument is found across the newline.
///
/// One known false positive, kept deliberately: `.label("…")` quoted inside a *trailing*
/// comment — `let x = 1; // .label("Reset")` — is reported as a call site, because the guard
/// above tests the start of the line rather than lexing it. The obvious repair, skipping any
/// line that contains `//`, trades the false positive for a false negative: a URL in a string
/// before a real call (`"https://…"`) would then be skipped, and a check that silently misses
/// is worse than one that over-reports. Telling those apart needs a lexer, which is
/// disproportionate to six call sites. Writing `.label(…)` in prose avoids it entirely.
fn gpui_labels_are_translated(root: &Path, checked: &mut usize, problems: &mut Vec<String>) {
    let mut sources = Vec::new();
    // Every .rs file in the repository, not only the two desktop shells: the rule is about
    // the call, and a third shell or a widget library under `crates/` would otherwise sit
    // outside it for being in a directory this check never looks in. `walk` skips build
    // output; templates are not build output, and covering them is the point.
    rename::walk(root, &mut sources);
    sources.retain(|path| path.extension().is_some_and(|ext| ext == "rs"));
    sources.sort(); // stable diagnostics, since the walk order is the filesystem's

    for path in sources {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // not UTF-8, and no gpui shell is written in that
        };
        for (start, _) in text.match_indices(".label(") {
            let before_on_line = before_line_start(&text, start);
            if mentioned_in_prose(&text, start) {
                continue;
            }
            if before_on_line.matches('"').count() % 2 == 1 {
                continue;
            }
            // A call site is one check whether or not it passes, so a clean tree still
            // reports every label that was looked at.
            *checked += 1;
            // `trim_start` because the argument may be written on the line below the call.
            if !text[start + ".label(".len()..]
                .trim_start()
                .starts_with('"')
            {
                continue;
            }
            problems.push(format!(
                "{}:{}: a label is a word the user reads, so it comes from crates/i18n as \
                 `translate(Key::…, self.locale)` — the `Button::new(..)` id above it is a \
                 string literal on purpose and stays one",
                repo::rel(root, &path),
                text[..start].matches('\n').count() + 1,
            ));
        }
    }
}

/// Every directory under `crates/i18n/locales/` that holds a locale file must hold one
/// per locale — `common/` needs an `en.toml` and an `hr.toml`, and so does every
/// `pages/*`.
///
/// This is the case the generator cannot catch: `i18n-keygen` compares keys *across the
/// files that exist*, so a whole missing `hr.toml` is not missing keys, it is a missing
/// locale — rust-i18n globs the directory, renders every key in the fallback language,
/// and nothing anywhere warns. A directory listing is enough here, which is what makes it
/// checkable the way `platform_templates_are_shaped_right` is. (Negative-tested by
/// deleting `locales/common/hr.toml`: the check fires; the build does not.)
fn locale_files_are_complete(root: &Path, checked: &mut usize, problems: &mut Vec<String>) {
    let locales = root.join("crates/i18n/locales");
    if !locales.is_dir() {
        // The crate owns this directory; if it is gone the crate's own build fails, so
        // there is nothing left for this check to say.
        return;
    }
    *checked += 1;

    // Every locale code anywhere in the tree — the union is the locale set, because
    // `i18n!` globs `locales/**/*.toml` and a code appearing once does define a locale.
    let mut directories = Vec::new();
    collect_locale_directories(&locales, &mut directories);

    let mut locales_found: Vec<&str> = directories
        .iter()
        .flat_map(|(_, files)| files.iter().map(String::as_str))
        .collect();
    locales_found.sort_unstable();
    locales_found.dedup();

    for (directory, files) in &directories {
        for locale in &locales_found {
            if !files.iter().any(|file| file == locale) {
                problems.push(format!(
                    "{} has no `{locale}.toml` — every key in this directory would render \
                     in the fallback language",
                    directory.display()
                ));
            }
        }
    }
}

/// The `icu_*_data` crates this workspace tolerates in `Cargo.lock`, and why each is not the
/// blob that ADR 0021 sliced away.
const ALLOWED_ICU_DATA_CRATES: &[(&str, &str)] = &[
    // The parent-locale rules. `crates/i18n-data` turns this feature ON deliberately: the
    // generated provider falls back internally, and the rules are the same size however many
    // locales ship, so they are not what the slice is about (ADR 0021).
    (
        "icu_locale_fallback_data",
        "the fallback chains, not per-locale data — enabled on purpose by `crates/i18n-data`",
    ),
    // Both belong to `icu_normalizer`, which `idna` pulls through `url`: they were in every
    // binary before `i18n` existed and they are Unicode character tables, not CLDR.
    (
        "icu_normalizer_data",
        "character tables behind `idna`/`url`, not CLDR",
    ),
    (
        "icu_properties_data",
        "character properties behind `idna`/`url`, not CLDR",
    ),
    // `i18n-keygen` links `icu_plurals` with its defaults to work out, at build time, which
    // plural categories a translation's arguments need. It runs on the host while `i18n` is
    // being compiled and is never in a shipped binary, so it may have the whole table; the
    // runtime crate ships the slice instead (ADR 0021).
    (
        "icu_plurals_data",
        "host-only: `i18n-keygen` is a build dependency, so this never reaches a binary",
    ),
];

/// The CLDR payload stays sliced (ADR 0021).
///
/// `crates/i18n-data` is 124 KB of CLDR — the two locales `crates/i18n/locales` ships, nine
/// markers — and the saving is only real while nothing else in the graph links the same data
/// whole. Cargo unifies features across the entire dependency graph, so one ordinary line in
/// any crate, `icu_datetime = "2.3"` with its default `compiled_data` feature, re-links all
/// of CLDR into every binary that contains `i18n` and the slice becomes code nobody loads.
/// The failure is quiet in exactly the way this repository dislikes: the build succeeds, every
/// formatter returns `Ok`, the output is identical, and a few hundred kilobytes per binary
/// change sides.
///
/// Neither existing check sees it. `just i18n::check-data` compares the committed payload with
/// what the generator produces — a whole-CLDR link leaves those bytes untouched. The locale
/// invariant in `crates/i18n/src/lib.rs` reads the slice, which is still correct and present.
/// Only a measurement would notice, and a measurement is not a gate.
///
/// `Cargo.lock` witnesses it. `icu_<crate>_data` appears as a dependency of `icu_<crate>` if
/// and only if something in the graph resolved that crate's `compiled_data` feature, so the
/// lockfile says, in text, whether the payload is sliced — and it does so without building
/// anything, which for a 1.1k-package graph is the difference between a check that runs on
/// every push and one that does not.
fn cldr_data_stays_sliced(root: &Path, checked: &mut usize, problems: &mut Vec<String>) {
    let Ok(source) = std::fs::read_to_string(root.join("Cargo.lock")) else {
        // No lockfile means nothing has been resolved yet; there is no feature set to assert
        // about, and `cargo` will write one on the next build.
        return;
    };
    *checked += 1;
    // `toml::from_str`, not `source.parse()`: `FromStr for toml::Value` parses a TOML *value*
    // (an inline table), not a document, so it rejects the file's first comment with
    // "expected nothing".
    let lock = match toml::from_str::<toml::Value>(&source) {
        Ok(lock) => lock,
        Err(error) => {
            problems.push(format!(
                "Cargo.lock: does not parse ({error}) — cannot tell whether the CLDR payload is \
                 sliced"
            ));
            return;
        }
    };

    let mut found: Vec<String> = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|package| package.get("name").and_then(toml::Value::as_str))
        .filter(|name| name.starts_with("icu_") && name.ends_with("_data"))
        .map(ToOwned::to_owned)
        .collect();
    found.sort_unstable();

    for name in found {
        *checked += 1;
        if ALLOWED_ICU_DATA_CRATES
            .iter()
            .any(|(listed, _)| *listed == name)
        {
            continue;
        }
        // The crate whose feature this is, spelled the way `cargo tree` takes it.
        let owner = name.trim_end_matches("_data");
        problems.push(format!(
            "Cargo.lock: {name} is linked, so something enables `{owner}/compiled_data` — all of \
             CLDR that crate covers goes into every binary containing `i18n`, on top of the \
             sliced copy in crates/i18n-data (ADR 0021). Find it with \
             `cargo tree -i -p {name}` and turn the default features off there; if this data \
             crate genuinely belongs, add it to ALLOWED_ICU_DATA_CRATES with why it is not \
             per-locale CLDR."
        ));
    }
}

/// Recursively collects `(directory, *.toml file stems)` for every directory holding
/// at least one locale file.
fn collect_locale_directories(dir: &Path, found: &mut Vec<(PathBuf, Vec<String>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut files = entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "toml"))
        .map(|entry| {
            entry
                .path()
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();

    files.sort();
    if !files.is_empty() {
        found.push((dir.to_path_buf(), files));
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            collect_locale_directories(&entry.path(), found);
        }
    }
}

/// Every app that links the translations has a namespace of its own under
/// `crates/i18n/locales/pages/`.
///
/// The gap this closes has no symptom, which is why it survived. `just add crux-app
/// payments` used to produce an app with nowhere to put its strings, so the next move was
/// to put `payments.title` into `locales/common/` — which ADR 0014 reserves for "words
/// that belong to no single page" — and every gate stayed green, because nothing was
/// broken. Missing, not wrong: the same shape as `b4dfb9b`, where the example app had a
/// thing the scaffolded app did not.
///
/// It does double duty, and that is the argument for this shape over "every `pages/<x>`
/// must name an app" — which is wrong in general, since `pages/` is a filing decision and
/// a product may legitimately have a `pages/login/` with no app called `login`:
///
/// - a hand-rolled app that links `i18n` and has no namespace is named;
/// - **a rename that forgot to move the namespace is named** — the old directory survives
///   and the new one is absent, so this fires on the renamed app. That is the regression
///   `rename::move_locale_namespace` exists to prevent, and this is what stops that fix
///   rotting. `check-template`'s "no file still names `apps/<old>`" grep cannot stand in
///   for it: the stale path is `locales/pages/<old>`, which contains no `apps/`.
///
/// Scoped to apps that link `i18n`, because that is the rule the scaffolder applies (see
/// `Archetype::locale_namespace`) and the archetype is not recoverable from a tree that
/// already exists. A `cli-app` under `apps/<name>/app` renders no crate that depends on
/// `i18n`, so demanding two near-empty files of it would be demanding them of an app with
/// nothing to translate. The cost of the rule that *is* enforced, stated rather than
/// hidden: an app with genuinely no strings of its own still carries one empty file per
/// locale. A file per locale is cheaper than a rule nobody checks.
///
/// Asks for the fallback locale's file and not the whole set. Completeness is
/// `locale_files_are_complete`'s job and the two interlock rather than overlap: this says
/// the namespace must exist, that one says it must hold every locale. Deleting one locale
/// file from a scaffolded namespace fires that check and not this one, which is the
/// negative test that proves they are two checks rather than one written twice.
fn apps_have_locale_namespaces(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    if !root.join("crates/i18n/locales").is_dir() {
        // No translations crate, so nothing to have a namespace in — the same early return
        // `locale_files_are_complete` makes.
        return Ok(());
    }

    for app in repo::expand(root, "apps/*") {
        let Some(name) = app.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !links_translations(root, &app)? {
            continue;
        }
        *checked += 1;

        let namespace = repo::locale_namespace(root, name);
        // The fallback code is read from the compiled crate rather than spelled "en": it is
        // already written twice — `i18n!`'s literal and `build.rs`'s `.fallback(..)` — and
        // needs a test to tie those together. A third literal here would be a fourth place
        // to forget.
        let fallback = namespace.join(format!("{}.toml", i18n::FALLBACK_LOCALE));
        if !fallback.is_file() {
            problems.push(format!(
                "{} links `i18n` but has no locale namespace at {} — its own strings have \
                 nowhere to go, and the next one written lands in `locales/common/`, which \
                 ADR 0014 reserves for words that belong to no single page. `just new` \
                 creates one; `cargo xtask rename app` moves it with the app.",
                repo::rel(root, &app),
                repo::rel(root, &namespace)
            ));
        }
    }
    Ok(())
}

/// Whether any crate under `app` depends on the translations crate.
///
/// Stated limit, `cores_stay_domain_level`'s: this reads the three ordinary dependency
/// tables, so a `[target.'cfg(..)'.dependencies]` entry would slip past. No app in this
/// repository uses one, and a scan pretending to cover them would be guessing at a TOML
/// shape it does not parse.
fn links_translations(root: &Path, app: &Path) -> Result<bool> {
    let mut manifests = Vec::new();
    repo::find_files(app, "Cargo.toml", &mut manifests);

    for manifest in manifests {
        let relative = repo::rel(root, &manifest);
        let source =
            std::fs::read_to_string(&manifest).with_context(|| format!("reading {relative}"))?;
        let parsed: toml::Value =
            toml::from_str(&source).with_context(|| format!("parsing {relative}"))?;
        for table in DEPENDENCY_TABLES {
            if parsed
                .get(*table)
                .and_then(|dependencies| dependencies.get("i18n"))
                .is_some()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Every `translate_with!(Key::…)` call hands the catalog exactly the arguments its
/// `%{…}` placeholders declare — in both directions, in every locale.
///
/// The gap this closes was named by the first interpolated call site itself: the macro
/// takes `$key:expr` and arbitrary `name = value` pairs, and nothing related either to
/// the catalog. Both failure modes render for the user and pass every gate — a typo'd
/// argument name drops its value and the sentence renders as `Count is: %{count}`, and
/// an argument no placeholder declares evaporates. Since ADR 0017 made this call the
/// path a formatted number takes to the screen, the uninterpolated placeholder is the
/// louder of the two bugs a formatting change can leave behind.
///
/// Placeholders are the union across **all** locales, from the compiled crate rather
/// than a re-parse of `locales/` — `i18n_resources`'s one-writer reasoning. A locale
/// copy that renames a placeholder is caught even though the fallback renders fine,
/// which is the case an English-only check would ship.
///
/// Stated limits, `gpui_labels_are_translated`'s in kind: only a key spelled `Key::…`
/// is checked — a literal key (the crate's own tests, the generator's doctests) has no
/// catalog entry to compare with, and a key behind a variable is invisible to a scan.
/// The macro admits both; every rendered sentence in the tree names a variant. A key
/// spelled `plural_key(Key::…)` counts as that variant, and additionally has to name a
/// plural family: `plural_key` takes any `Key`, so `plural_key(Key::Increment, n, l)`
/// type-checks into `increment.other` and renders the key at the reader (ADR 0018).
fn translate_with_arguments_match_the_catalog(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) {
    let catalog = catalog_placeholders();

    let mut sources = Vec::new();
    // Every `.rs` file, templates included — the skeleton's shells call the same macro
    // against the same catalog, and drift between the two copies is this repository's
    // most repeated defect.
    rename::walk(root, &mut sources);
    sources.retain(|path| path.extension().is_some_and(|ext| ext == "rs"));
    sources.sort();

    for path in sources {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (start, body) in translate_with_calls(&text) {
            // A mention in a comment is prose, not a call site — the guard
            // `gpui_labels_are_translated` uses, for the same false positive.
            if mentioned_in_prose(&text, start) {
                continue;
            }
            // A literal key or a shape this scanner misread: not a catalog call, so not
            // a check — counting it would inflate the pass total with a comparison that
            // never ran.
            let Some((variant, given)) = parse_translate_with(&body) else {
                continue;
            };
            *checked += 1;
            let location = format!(
                "{}:{}",
                repo::rel(root, &path),
                text[..start].matches('\n').count() + 1
            );

            // `plural_key` takes any `Key`, so it accepts a key that is not a family and
            // returns `increment.other` — a string no locale defines, which renders the
            // key at the reader. Nothing else about the call is wrong-looking.
            if is_plural_key(&body)
                && !i18n::Key::PLURAL_FAMILIES
                    .iter()
                    .any(|family| i18n_keygen::variant_name(family) == variant)
            {
                problems.push(format!(
                    "{location}: `plural_key(Key::{variant}, ..)` names no plural family — \
                     it builds `{variant_lower}.other`, which no locale defines",
                    variant_lower = variant.to_lowercase()
                ));
                continue;
            }

            let Some((key, placeholders)) = catalog.get(&variant) else {
                problems.push(format!(
                    "{location}: `Key::{variant}` is not a key the catalog generates"
                ));
                continue;
            };

            for missing in placeholders.difference(&given) {
                problems.push(format!(
                    "{location}: `%{{{missing}}}` in `{key}` gets no argument here — the \
                     sentence renders with its placeholder still in it"
                ));
            }
            for extra in given.difference(placeholders) {
                problems.push(format!(
                    "{location}: argument `{extra}` matches no `%{{…}}` placeholder of \
                     `{key}` in any locale — its value is silently dropped"
                ));
            }
        }
    }
}

/// The text between the start of `at`'s line and `at` itself.
fn before_line_start(text: &str, at: usize) -> &str {
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    &text[line_start..at]
}

/// Whether `at` is a name mentioned in prose rather than a call site.
///
/// Shared by every check that reads call sites out of source as text
/// (`translate_with_arguments_match_the_catalog`, `gpui_labels_are_translated`,
/// `telemetry_guards_are_bound`, `plural_families_reach_a_shell`) because this repository
/// explains its own rules in comments, often by quoting the very call it is talking about,
/// and a check that counted a sentence about a call as a call would pass on documentation
/// alone. It is the same four characters of policy written down four times before; the
/// fourth copy is the one that noticed.
fn mentioned_in_prose(text: &str, at: usize) -> bool {
    before_line_start(text, at).trim_start().starts_with("//")
}

/// The trees whose Rust shells are held to the plural rule: the example app's and the
/// skeleton's, both globbed so they mean the same thing in a generated product where the
/// example app has been renamed.
const PLURAL_SHELL_TREES: &[&str] = &["apps/*/shells", "templates/crux-app/apps/__name__/shells"];

/// Every plural family the fallback declares is named by a `plural_key` call in each app's
/// shells and in the skeleton's.
///
/// The failure this exists for is not a broken feature but a missing one, and this
/// repository shipped it: ADR 0018 added the demonstrator family `apples.*`, the resource
/// driver emitted `<plurals name="apples">` for Android and `variations.plural` entries for
/// Apple, fourteen of `crates/i18n`'s twenty-three translation assertions were about
/// plurals — and no shell in the tree named the family, so it rendered on no screen and
/// every gate stayed green. A catalog entry proves a string exists; only a call site proves
/// anyone is asking for it. `cargo xtask check` cannot see a framebuffer, but it can see
/// that nothing asks, and that is the half of the failure it owns.
///
/// The families are `Key::PLURAL_FAMILIES` — the generated constant, i.e. the same list the
/// resource driver enumerated when it decided what to emit — mapped through
/// `i18n_keygen::variant_name`, the generator's own rule for what a family is called, so the
/// check cannot pass on a family the emitters never saw. The call sites are read by
/// `translate_with_calls` and the comment guard
/// `translate_with_arguments_match_the_catalog` already needs (ADR 0017): one rule about what
/// a call is, no second implementation of it, and no plural family counted because somebody
/// wrote about one.
///
/// Two trees, and the second is what makes the rule outlive this repository. `apps/*/shells`
/// is every app's own, so a product that renamed the example app is still checked; the
/// crux-app skeleton is checked because a product's first app is a copy of it, and a skeleton
/// that dropped the call site would produce apps that fail this on the day they are created,
/// with nothing in the diff that dropped it to explain the check.
fn plural_families_reach_a_shell(root: &Path, checked: &mut usize, problems: &mut Vec<String>) {
    let families = i18n::Key::PLURAL_FAMILIES;

    for tree in PLURAL_SHELL_TREES {
        // A product has no `templates/` — the skeleton tree is the template's own, and an
        // absent skeleton is not a silent-pass defect there, it is the layout working.
        if tree.starts_with("templates/") && !root.join("templates").is_dir() {
            continue;
        }
        // Likewise a product with no app yet (`new --drop-example`): there are no shells to
        // check. Keyed on `has_crux_apps`, never on the glob matching nothing — with one app
        // present an empty match is still the defect below.
        if tree.starts_with("apps/") && !repo::has_crux_apps(root) {
            continue;
        }
        let shells = repo::expand(root, tree);
        // A glob that matches nothing is a silent pass, which is the exact shape of the bug
        // above. `archetypes_and_templates_agree` guards the skeleton's existence; this says
        // whether anything was actually looked at.
        if shells.is_empty() {
            problems.push(format!(
                "{tree}: matched no shell directory, so no plural family is checked against it"
            ));
            continue;
        }

        for shells in &shells {
            let named = plural_families_named_in(shells);

            for family in families {
                let variant = i18n_keygen::variant_name(family);
                *checked += 1;
                if named.contains(&variant) {
                    continue;
                }
                problems.push(format!(
                    "{}: nothing here calls `plural_key(Key::{variant}, ..)`, so `{family}.*` \
                     ships in this app's catalog and in its generated Android and Apple \
                     resources and renders nowhere (ADR 0018)",
                    repo::rel(root, shells),
                ));
            }
        }
    }
}

/// The plural families named by live `plural_key` call sites under `dir`, as variant names.
///
/// The scanner and the comment guard are `translate_with_arguments_match_the_catalog`'s, for
/// that check's reason as much as its own: this file already knows what a call site is, and a
/// second answer to that question is one fewer answer than the two of them agreeing.
fn plural_families_named_in(dir: &Path) -> BTreeSet<String> {
    let mut named = BTreeSet::new();

    let mut sources = Vec::new();
    rename::walk(dir, &mut sources);
    sources.retain(|path| path.extension().is_some_and(|ext| ext == "rs"));
    sources.sort(); // stable diagnostics, since the walk order is the filesystem's

    for path in sources {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // not UTF-8, and no shell is written in that
        };
        for (start, body) in translate_with_calls(&text) {
            if mentioned_in_prose(&text, start) {
                continue;
            }
            // Only the key position counts: `plural_key` used to build an *argument* names no
            // family anybody renders, and `is_plural_key` already says which calls do.
            if !is_plural_key(&body) {
                continue;
            }
            if let Some((variant, _)) = parse_translate_with(&body) {
                named.insert(variant);
            }
        }
    }

    named
}

/// Every `%{when}` a catalog declares is filled by a `datetime` call at every call site.
///
/// ADR 0017 gave numbers the rule that `Display` on an `i64` writes no locale, so digits
/// must pass through `number` before they pass into a sentence; an instant is the same
/// value in a worse costume. `1_759_478_400_123` interpolates without complaint, looks
/// almost like an epoch someone might have meant, and tells the reader nothing — it is not
/// even a date in *any* calendar until a clock zone has been told which one, and the wire
/// carries no zone (ADR 0020). So the rule for this value is stronger than "pass it
/// through the formatter": the only argument that may fill `%{when}` is an expression that
/// is already a `datetime(..)` call.
///
/// The shape of the failure decides the shape of the check. A dropped `number` argument
/// still renders a number, and `translate_with_arguments_match_the_catalog` catches only
/// the argument being *missing*; this one catches the argument being present and raw,
/// which no compiler can see because the macro accepts any `Display`. It reads the same
/// call bodies as every other check here — `translate_with_calls`, the comment guard, and
/// `parse_translate_with`'s catalog pairing, so a key renamed away from the catalog stops
/// being checked *and* fails the check next door — and it asks one question of the `when`
/// segment: does that expression make the `datetime` call itself. Deliberately not a
/// type, and deliberately a rule about the argument rather than about the function holding
/// it: a helper that hands a pre-formatted string to a sentence is exactly how an instant
/// travels unformatted into one, and the argument position is the only place the call can
/// be guaranteed.
///
/// What it cannot see is the platform shells: Kotlin and Swift fill their own resources'
/// placeholders through `java.time` and `Date.formatted`, and no Rust scanner reads them.
/// Same limit as every other check here, stated rather than worked around.
fn instants_are_formatted_at_the_edge(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) {
    let catalog = catalog_placeholders();

    let mut sources = Vec::new();
    rename::walk(root, &mut sources);
    sources.retain(|path| path.extension().is_some_and(|ext| ext == "rs"));
    sources.sort();

    for path in sources {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (start, body) in translate_with_calls(&text) {
            if mentioned_in_prose(&text, start) {
                continue;
            }
            let Some((variant, _)) = parse_translate_with(&body) else {
                continue;
            };
            // A key with no `%{when}` owes this check nothing; one the catalog does not
            // know at all is `translate_with_arguments_match_the_catalog`'s problem, which
            // it fails loudly. This check only answers questions about instants.
            let Some((key, placeholders)) = catalog.get(&variant) else {
                continue;
            };
            if !placeholders.contains("when") {
                continue;
            }
            *checked += 1;
            let location = format!(
                "{}:{}",
                repo::rel(root, &path),
                text[..start].matches('\n').count() + 1
            );

            let formatted = split_arguments(&body).into_iter().any(|argument| {
                leading_ident(&argument) == "when" && argument.contains("datetime(")
            });
            if !formatted {
                problems.push(format!(
                    "{location}: `%{{when}}` in `{key}` is filled without a `datetime` call — \
                     an instant reaching a sentence unformatted renders as milliseconds (ADR 0020)",
                ));
            }
        }
    }
}

/// `Key` variant name → (catalog key, its `%{…}` placeholders in any locale), read from
/// the compiled crate. The variant name comes from the generator's own rule rather than
/// a second implementation of it: `i18n_keygen::variant_name` is public for exactly
/// this.
///
/// Enumerated over `Key::CATALOG` (ADR 0018), and a plural family's members are merged
/// into that one entry: `apples` carries the placeholders of `apples.one`, `apples.few`
/// and `apples.other` together. That is the contract a call site actually has, since it
/// passes `plural_key(Key::Apples, count, locale)` and gets whichever category the locale's
/// rules select — one set of arguments for an unknown category. It is also stricter than
/// checking the selected member, and deliberately so: when two categories of one family
/// disagree about a placeholder name, the call site has to satisfy both, and that is the
/// moment to be told about it rather than to discover which category the tester happened
/// to type.
fn catalog_placeholders() -> BTreeMap<String, (String, BTreeSet<String>)> {
    let mut catalog = BTreeMap::new();

    for (key, family) in i18n::Key::CATALOG {
        let mut placeholders = BTreeSet::new();

        for locale in i18n::Locale::ALL {
            // `try_translate`, not `translate`: a locale that lacks the key must
            // contribute nothing rather than the fallback's placeholders a second time.
            // (Today they would be the same set; the day they are not is the day this
            // distinction matters.)
            if let Some(template) = i18n::try_translate(key, locale) {
                placeholders.extend(placeholder_names(&template));
            }
        }

        let entry = catalog
            .entry(i18n_keygen::variant_name(family))
            .or_insert_with(|| ((*family).to_owned(), BTreeSet::new()));

        entry.1.extend(placeholders);
    }

    catalog
}

/// The `%{name}` placeholder names in one translated template. A `%{name:spec}`
/// contributes `name`: rust-i18n's own syntax allows a specifier the generated macro
/// does not consume, and the name is the part both agree on. A `%{` that does not close
/// on an identifier is prose and contributes nothing.
fn placeholder_names(template: &str) -> Vec<String> {
    template
        .match_indices("%{")
        .filter_map(|(at, _)| {
            let rest = &template[at + "%{".len()..];
            let inner = &rest[..rest.find('}')?];
            let name: String = inner
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let specifier = inner[name.len()..].strip_prefix(':');
            let closed = inner.len() == name.len() || specifier.is_some();
            (!name.is_empty() && closed).then_some(name)
        })
        .collect()
}

/// Every `translate_with!(…)` call body in `text`, as `(offset of the call, text
/// between the outer parentheses)`.
fn translate_with_calls(text: &str) -> Vec<(usize, String)> {
    let needle = "translate_with!(";
    let mut calls = Vec::new();
    for (start, _) in text.match_indices(needle) {
        let body_start = start + needle.len();
        if let Some(end) = closing_paren(&text[body_start..]) {
            calls.push((start, text[body_start..body_start + end].to_owned()));
        }
    }
    calls
}

/// The offset of the `)` closing a span that begins *inside* the parentheses, so at
/// depth zero. String and char literals are skipped: a `')'` in one is an argument's
/// value, not the call's end.
///
/// `pub` (clippy's `redundant_pub_crate` is right that a private module cannot make it
/// narrower) because `swift_keywords` brackets declarations with it as well: one rule about
/// where a nested span ends, two readers, and a second copy of it would drift.
pub fn closing_paren(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut chars = text.char_indices();
    while let Some((at, ch)) = chars.next() {
        match ch {
            '"' => skip_string(&mut chars),
            '\'' => skip_char(&mut chars),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                if depth == 0 {
                    return Some(at);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// Advance past the closing `"` of a string literal whose opening quote is consumed.
fn skip_string(chars: &mut std::str::CharIndices<'_>) {
    while let Some((_, ch)) = chars.next() {
        match ch {
            '\\' => {
                chars.next();
            }
            '"' => return,
            _ => {}
        }
    }
}

/// Advance past a `'\''`-style char literal whose opening quote is consumed. A `'` that
/// turns out to open a lifetime instead (`'a`) is left where it is — a lifetime has no
/// brackets to unbalance, and guessing otherwise would eat the rest of the call.
fn skip_char(chars: &mut std::str::CharIndices<'_>) {
    let Some((_, first)) = chars.next() else {
        return;
    };
    if first == '\\' {
        chars.next();
    }
    // The closing quote is what makes it a literal; without one this was a lifetime and
    // the consumed characters were harmless.
    if let Some((_, '\'')) = chars.clone().next() {
        chars.next();
    }
}

/// Whether the key argument of a `translate_with!` body is a `plural_key(..)` call,
/// however qualified — `plural_key(..)` and `i18n::plural_key(..)` are the same call to
/// the compiler and to this scanner.
fn is_plural_key(body: &str) -> bool {
    split_arguments(body)
        .first()
        .is_some_and(|key| key.contains("plural_key("))
}

/// `(key variant, argument names)` of one `translate_with!` body, when the key is a
/// generated `Key::…`; `None` when the key is anything else — a literal, a variable —
/// which has no catalog entry to compare arguments against.
///
/// A `plural_key(Key::Apples, count, locale)` key is read as `Apples`: the split is
/// depth-aware, so the nested commas stay inside the first segment, and the variant is
/// what follows the last `Key::` in it.
fn parse_translate_with(body: &str) -> Option<(String, BTreeSet<String>)> {
    let segments = split_arguments(body);
    let variant = leading_ident(segments.first()?.rsplit_once("Key::")?.1);
    if variant.is_empty() {
        return None;
    }

    let mut names = BTreeSet::new();
    for segment in segments.get(2..).unwrap_or_default() {
        // The macro's grammar is `name = value`, enforced by the compiler; a segment
        // with no leading identifier is a shape this scanner does not understand, and
        // inventing a name for it would be worse than leaving it out.
        let name = leading_ident(segment);
        if !name.is_empty() {
            names.insert(name);
        }
    }
    Some((variant, names))
}

/// Split a call body on its top-level commas. Brackets nest and literals may contain
/// anything, so both are walked rather than split on. A literal's *contents* are not
/// preserved in the segment — every reader of these segments asks for its leading
/// identifier, and a comma inside a string must not decide where an argument ends.
///
/// `pub` for `swift_keywords`, which reads a derive's and a `cfg_attr`'s argument list with it.
pub fn split_arguments(body: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut chars = body.char_indices();
    while let Some((_, ch)) = chars.next() {
        match ch {
            '"' => {
                current.push(ch);
                skip_string(&mut chars);
            }
            '\'' => {
                current.push(ch);
                skip_char(&mut chars);
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => segments.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    segments.push(current);
    segments
}

/// The identifier a fragment starts with, ignoring leading whitespace.
///
/// `pub` for `swift_keywords`, which reads an item header with it. A raw identifier is the
/// exception, and `swift_keywords::leading_raw_ident` is why: this stops at the `#`.
pub fn leading_ident(fragment: &str) -> String {
    fragment
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// `mod <name> '<relative path>'` is depth-sensitive, and a wrong path only surfaces when
/// someone runs that particular recipe.
///
/// Template Justfiles are checked too, but against where they will LAND rather than where
/// they sit: a template mirrors the repository root, so
/// `templates/service/services/__name__/Justfile` is resolved as if it were
/// `services/__name__/Justfile`. Otherwise a template with the wrong depth stays invisible
/// until someone generates from it and runs a recipe.
fn just_module_paths_resolve(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let mut justfiles = Vec::new();
    repo::find_files(root, "Justfile", &mut justfiles);

    for justfile in justfiles {
        let source = std::fs::read_to_string(&justfile)
            .with_context(|| format!("reading {}", repo::rel(root, &justfile)))?;
        let directory = effective_directory(root, &justfile);

        for line in source.lines() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.strip_prefix("mod ") else {
                continue;
            };
            let Some(start) = rest.find('\'') else {
                continue;
            };
            let Some(len) = rest[start + 1..].find('\'') else {
                continue;
            };
            let target = &rest[start + 1..start + 1 + len];
            *checked += 1;
            if !normalize(&directory.join(target)).exists() {
                problems.push(format!(
                    "{}: `mod` path {target:?} does not resolve",
                    repo::rel(root, &justfile)
                ));
            }
        }
    }
    Ok(())
}

/// Where a Justfile's relative paths should be resolved from.
///
/// For a real one, its own directory. For one inside a template, the directory it will
/// occupy once generated — `templates/<archetype>/` is stripped, since a template mirrors
/// the repository root.
fn effective_directory(root: &Path, justfile: &Path) -> PathBuf {
    let parent = justfile.parent().unwrap_or(root);
    let Ok(relative) = parent.strip_prefix(root.join("templates")) else {
        return parent.to_path_buf();
    };
    // Drop the archetype directory; what remains is the repo-relative destination.
    let mut components = relative.components();
    components.next();
    root.join(components.as_path())
}

/// Resolve `.` and `..` lexically.
///
/// `Path::exists` resolves `..` through the filesystem, which fails for a template
/// destination such as `services/__name__/..` where the intermediate directory does not
/// exist. The check is about the path's shape, so resolving it textually is both correct
/// and what is actually meant.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The first double-quoted string on the first line that holds both `needle` and a quote.
///
/// Lines are the unit because the files this reads are written by this repository's own
/// generators and templates, one declaration per line; a needle on a line without a
/// string — the `Language::Kotlin => {` match arm below the one that names the package —
/// is skipped rather than misread.
fn quoted_after<'a>(text: &'a str, needle: &str) -> Option<&'a str> {
    for line in text.lines() {
        let Some(start) = line.find(needle) else {
            continue;
        };
        let rest = &line[start + needle.len()..];
        let Some(open) = rest.find('"') else { continue };
        let Some(close) = rest[open + 1..].find('"') else {
            continue;
        };
        return Some(&rest[open + 1..open + 1 + close]);
    }
    None
}

/// One app's native identifiers must all name the same Kotlin package.
///
/// Three code paths write them and none of them sees the others: `xtask new crux-app`
/// renders `core/boltffi.toml` and `core/src/bin/codegen.rs` from the templates, and
/// `xtask add-platform` copies the Gradle project from a donor or the skeleton. Two
/// generators — boltffi and the Crux typegen — write into the package the core declares,
/// and Gradle compiles the app module against the namespace *it* declares. A split is
/// therefore invisible from any one file and fatal at build time, surfacing as Kotlin
/// unable to resolve its own generated types: an error that names neither half.
///
/// The `boltffi.toml` comment saying "Must match the Kotlin name in codegen.rs" was
/// exactly the kind of comment that should have been a check. This is the check, and it
/// is why the templates are allowed to disagree with the repository around them: adoption
/// reconciles at scaffold and copy time, and this fails when adoption is ever skipped.
fn native_identifiers_agree(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    for core in repo::expand(root, "apps/*/core") {
        let Some(app) = core.parent().and_then(Path::file_name) else {
            continue;
        };
        let config_path = core.join("boltffi.toml");
        let Ok(text) = std::fs::read_to_string(&config_path) else {
            continue; // not a Crux core, or one with no native targets — nothing to agree
        };
        let config: toml::Value = toml::from_str(&text)
            .with_context(|| format!("parsing {}", repo::rel(root, &config_path)))?;
        let Some(package) = config
            .get("targets")
            .and_then(|t| t.get("android"))
            .and_then(|a| a.get("kotlin"))
            .and_then(|k| k.get("package"))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        *checked += 1;

        // The Crux typegen's Kotlin package, from the match arm in codegen.rs. Absent
        // only in a core that has no typegen binary, which is a shape nothing generates.
        let codegen = core.join("src/bin/codegen.rs");
        if codegen.is_file() {
            *checked += 1;
            let source = std::fs::read_to_string(&codegen)
                .with_context(|| format!("reading {}", repo::rel(root, &codegen)))?;
            match quoted_after(&source, "Language::Kotlin =>") {
                Some(kotlin) if kotlin == package => {}
                Some(kotlin) => problems.push(format!(
                    "{}: the typegen writes Kotlin into {kotlin:?} but boltffi.toml declares \
                     {package:?} — two generators, one package, and Gradle compiles neither \
                     half",
                    repo::rel(root, &codegen)
                )),
                None => problems.push(format!(
                    "{}: no `Language::Kotlin => \"…\"` string — the typegen has no Kotlin \
                     package to agree with boltffi.toml's {package:?}",
                    repo::rel(root, &codegen)
                )),
            }
        }

        // The Gradle project, when the app has one. Namespace and applicationId are both
        // checked: the namespace is what the app module compiles against, the
        // applicationId is what the device and the store see, and a project copied from
        // elsewhere can carry either half of someone else's.
        let gradle = root
            .join("apps")
            .join(app)
            .join("platforms/android/app/build.gradle.kts");
        if gradle.is_file() {
            let source = std::fs::read_to_string(&gradle)
                .with_context(|| format!("reading {}", repo::rel(root, &gradle)))?;
            for field in ["namespace", "applicationId"] {
                *checked += 1;
                match quoted_after(&source, field) {
                    Some(value) if value == package => {}
                    Some(value) => problems.push(format!(
                        "{}: {field} is {value:?} but the core's boltffi.toml declares \
                         {package:?} — the bindings land in a package this project cannot \
                         see",
                        repo::rel(root, &gradle)
                    )),
                    None => problems.push(format!(
                        "{}: declares no {field} — the core's boltffi.toml says {package:?}, \
                         and nothing here agrees or disagrees with it",
                        repo::rel(root, &gradle)
                    )),
                }
            }
        }
    }
    Ok(())
}

/// Files exempt from the app-path rule, by repository-relative path.
///
/// `rename.rs` holds its `apps/<app>` literals as test *inputs*: they are the strings the
/// substitution assertions feed in and compare against, so a rename that reached them
/// would rewrite the proof that renaming works. Nothing else is exempt, and the root
/// `Justfile` deliberately needs no exemption: its `check-template` recipe carries its
/// probe names as variables, because inside a generated product those names become real
/// apps and this same invariant scans that copy's `Justfile` like any other file.
///
/// Because this file's literals name this repository's example app, it matches the
/// invariant's needle here and matches nothing in a renamed product — the reason the
/// invariant counts one lower in a product; see its doc comment and RT-9.
const APP_PATH_EXEMPT: &[&str] = &["tools/cli/src/rename.rs"];

/// Every `apps/<app>` reference outside `apps/` must be one a rename can reach.
///
/// The dual of the assertion `just check-template` runs inside a generated product: there,
/// no file may name the pre-`init` app at all, which proves the rename reached everything;
/// here, every file that names an app must sit on one of `rename`'s lists — `PROSE`,
/// `PATHS_ONLY`, or under a `PATHS_ONLY_TREES` entry — so that a rename rewrites it. A
/// reference no list reaches is silent in this repository, where the example app really is
/// called what the reference says, and ships in every product as guidance pointing at a
/// directory that does not exist. Both halves are needed: this one catches the stale
/// reference on the day it is written, that one catches a list that stopped covering a
/// file. The rule itself — a file outside `apps/` may name an app, but only where rename
/// can see it — is stated in `PROSE`'s doc comment.
///
/// **This check's count is one lower in a generated product than here, correctly and
/// permanently — it is the whole of the 417-vs-416 gap RT-9 was opened for.** The check
/// counts once per (app, file) pair whose text contains the needle `apps/<app>`, and the
/// one file in `APP_PATH_EXEMPT` matches this repository's needle only because its test
/// fixtures name this repository's example app. In a product the example app is renamed,
/// the fixtures still name the pre-rename app — they are the inputs to the assertions that
/// prove rename works — so the exempt file matches no app's needle there and contributes
/// nothing to count. Settled by per-check breakdown (RT-9): every other top-level check
/// contributes an identical count in both trees. Do not "fix" the numbers to match —
/// rewriting those fixtures to the renamed app, or re-exempting the file, would rewrite
/// the proof that renaming works, which is what holds this rule up.
fn app_paths_outside_apps_are_reachable(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) {
    let apps_dir = root.join("apps");
    let mut files = Vec::new();
    rename::walk(root, &mut files);
    // An app's own tree may name itself freely — `rename app` walks it wholesale — and
    // `walk` has already skipped everything that is build output rather than source.
    files.retain(|file| !file.starts_with(&apps_dir));

    for app in repo::expand(root, "apps/*") {
        let Some(name) = app.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let needle = format!("apps/{name}");
        for file in &files {
            // Read rather than grepped: unreadable files are icons and wrapper jars, and
            // those hold no paths a reader would follow.
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            if !text.contains(&needle) {
                continue;
            }
            *checked += 1;
            let relative = repo::rel(root, file);
            let reachable = rename::PROSE.contains(&relative.as_str())
                || rename::PATHS_ONLY.contains(&relative.as_str())
                || APP_PATH_EXEMPT.contains(&relative.as_str())
                || rename::PATHS_ONLY_TREES
                    .iter()
                    .any(|tree| relative.starts_with(&format!("{tree}/")));
            if !reachable {
                problems.push(format!(
                    "{relative}: names {needle}, but no rename list reaches it — \
                     `rename app` would leave it pointing at a directory that no longer \
                     exists, in this repository and in every product generated from it. \
                     Add the file to PROSE (prose, substituted wholesale), PATHS_ONLY \
                     (paths rewritten, everything else untouched), or a PATHS_ONLY_TREES \
                     entry — or say the app's name some other way"
                ));
            }
        }
    }
}

/// Every decision record appears in `docs/adr/README.md`, and every link resolves.
///
/// An index maintained by hand is an index that silently stops being one. The failure is
/// quiet in both directions — a record nobody linked is a record nobody reads, and a link to
/// a file that was renamed is worse than no link — so both are checked rather than trusted.
fn decision_records_are_indexed(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let dir = root.join("docs/adr");
    let index_path = dir.join("README.md");
    if !dir.is_dir() {
        return Ok(());
    }

    *checked += 1;
    if !index_path.is_file() {
        problems.push("docs/adr/README.md is missing — it is the index".to_owned());
        return Ok(());
    }
    let index = std::fs::read_to_string(&index_path).context("reading docs/adr/README.md")?;

    let mut records: Vec<String> = std::fs::read_dir(&dir)
        .context("reading docs/adr")?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(ToOwned::to_owned))
        .filter(|name| {
            Path::new(name).extension().is_some_and(|ext| ext == "md") && name != "README.md"
        })
        .collect();
    records.sort();

    for record in &records {
        *checked += 1;
        if !index.contains(record.as_str()) {
            problems.push(format!(
                "docs/adr/{record} is not linked from docs/adr/README.md"
            ));
        }
    }

    // And the other direction: a link naming a file that is not there.
    //
    // The rows come from `crate::adr`, shared with `decision_records_are_numbered` below: both
    // checks read the same table, one asking whether each target resolves and the other whether
    // each label agrees with it. Two link walks of one file is the drift RT-18 names — one of
    // them quietly reading a row the other never saw — so there is one, and the filter it has
    // always applied (a target with no `.md`, or one carrying a `/`, is not a row of this table)
    // moved into it rather than being re-derived here.
    for row in adr::index_rows(&index) {
        *checked += 1;
        if !dir.join(&row.target).is_file() {
            problems.push(format!(
                "docs/adr/README.md links {}, which does not exist",
                row.target
            ));
        }
    }
    Ok(())
}

/// Records inherited by products. Kept as a literal list so `cargo cli check` can compare
/// this contract with `new::DROPPED_ADRS` and the template's live `docs/adr/` directory.
const INHERITED_ADRS: &[u16] = &[
    1, 2, 3, 4, 5, 9, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 27, 28, 29, 31,
];

/// A product's ADR sequence: the inherited kept set (0001-0005, 0009, 0011-0023, 0027-0029,
/// 0031) must be exactly present, and the product's own records run contiguously from 0100.
fn product_adr_sequence(
    by_number: &BTreeMap<u16, Vec<&adr::Record>>,
    highest: Option<u16>,
    problems: &mut Vec<String>,
) {
    for &n in INHERITED_ADRS {
        if !by_number.contains_key(&n) {
            problems.push(format!(
                "docs/adr has no record numbered {} — a product inherits the kept set",
                adr::spelled(n),
            ));
        }
    }
    for number in by_number.keys().copied() {
        if number < 100 && !INHERITED_ADRS.contains(&number) {
            problems.push(format!(
                "docs/adr/{} is numbered {} — a product's inherited records are exactly \
                 the kept set, and {} is template-only and must not reach a product",
                by_number[&number][0].file_name,
                adr::spelled(number),
                adr::spelled(number)
            ));
        }
    }
    for number in 100..highest.unwrap_or(99) {
        if !by_number.contains_key(&number) {
            problems.push(format!(
                "docs/adr has no record numbered {} — a product's own records run \
                 contiguously from 0100",
                adr::spelled(number)
            ));
        }
    }
}

/// The numbers themselves: contiguous from 0001, never twice, and agreeing with the file that
/// carries them in its name, in its heading, and in the label of its index row.
///
/// The defect this exists for has a name. Two branches opened in the same week both look at
/// `docs/adr/`, both take "the next free number", and one of them renumbers at merge — which is a
/// repository-wide find-and-replace that nothing verified, against references that are all by
/// number. Measured at the time of writing: 267 citations of the form `ADR NNNN` outside
/// `docs/adr/` itself — AGENTS.md, `crates/identity`, `gates.toml`, most of xtask's own check
/// docs — plus 33 more that cite a record by its path. `cargo xtask new-adr` makes the common case
/// one command; this is what turns the collision into a failing check, which is where it actually
/// happens.
///
/// A **second** function rather than more lines inside `decision_records_are_indexed` above it,
/// which already covers the index in both directions. The breakdown attributes each contribution
/// through `stringify!` on the `step!` path (see `check`), so a new name is a new line and a
/// moving count is attributable to something. Extending the neighbour would raise one number with
/// no new name against it — the exact defect RT-16 added `--breakdown` to make visible, and the
/// reason RT-9's per-check numbers are worth keeping.
///
/// The parse is `crate::adr`'s, read by this check and by the generator that needs the next free
/// number: one read of the directory, not two (RT-18's rule, and RT-25's shared `SENTINELS`,
/// both about drifting in the gap between two parses of one file).
///
/// Counted the way its neighbour counts: one for the sequence the set must form, one per record
/// for its heading, one per index row for its label.
///
/// What is **not** asserted is the section structure. 0020 argues under "Measured first, decided
/// second" instead of `## Decision`, 0022 says `## Rejected` rather than `## What was rejected,
/// and why`, and 0009 has grown `## Where this does not reach` — three records that deviate on
/// purpose, in the direction a record is supposed to be able to deviate. The sections are a norm
/// `new-adr` emits, not a rule here.
fn decision_records_are_numbered(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let dir = root.join("docs/adr");
    if !dir.is_dir() {
        return Ok(());
    }
    let records = adr::records(root)?;

    // The sequence, as one assertion about the set: contiguous from 0001, and no number twice.
    *checked += 1;
    let mut by_number: BTreeMap<u16, Vec<&adr::Record>> = BTreeMap::new();
    for record in &records {
        by_number.entry(record.number).or_default().push(record);
    }

    for (number, group) in &by_number {
        if group.len() > 1 {
            let files: Vec<String> = group
                .iter()
                .map(|record| format!("docs/adr/{}", record.file_name))
                .collect();
            problems.push(format!(
                "{} are both numbered {} — every citation to a decision record cites the number, \
                 so this number now names {} files and a reader cannot tell which one a \
                 reference means. Two branches took the same next free number; renumber the one \
                 nobody has cited yet, while that is still cheap.",
                files.join(" and "),
                adr::spelled(*number),
                group.len()
            ));
        }
    }

    // Contiguity. The template's records run contiguously from 0001. A product (no
    // `templates/` — the template-only skeletons `new` strips) inherits the known kept set
    // (0001-0005, 0009, 0011-0023, 0027-0029, 0031) and starts its own records at 0100, so its
    // sequence is checked against that shape instead: a template-only record reaching a product,
    // or a product record below 0100, is the defect.
    let is_product = repo::is_product(root);
    let highest = by_number.keys().next_back().copied();
    if is_product {
        product_adr_sequence(&by_number, highest, problems);
    } else {
        for number in 1..highest.unwrap_or(0) {
            if !by_number.contains_key(&number) {
                let next = by_number
                    .keys()
                    .find(|existing| **existing > number)
                    .map_or(String::new(), |existing| {
                        format!(
                            " and the record after the hole is docs/adr/{}",
                            by_number[existing][0].file_name
                        )
                    });
                problems.push(format!(
                    "docs/adr has no record numbered {}{next} — the numbers run contiguously from \
                     0001, and a hole is a number something used to mean: a citation to {} now \
                     resolves to nothing.",
                    adr::spelled(number),
                    adr::spelled(number)
                ));
            }
        }
    }

    // Each record against its own first line.
    for record in &records {
        *checked += 1;
        let number = adr::spelled(record.number);
        match record.heading_number {
            // The other half of the same-looking defect, and the one that is not a renumber:
            // reported separately so "the heading disagrees" and "there is no heading" do not
            // arrive as one message about a file that has neither.
            None => problems.push(format!(
                "docs/adr/{} has no `# NNNN.` heading on its first line — {} is its number only \
                 in its filename. A record that does not state its own number in its own text is \
                 not a renumber that stopped halfway; it is a record whose heading was never \
                 written.",
                record.file_name, number
            )),
            Some(heading) if heading != record.number => problems.push(format!(
                "docs/adr/{} is {} by filename and {} by its own `# {}.` heading — one of the \
                 two is a renumber that did not reach the other. Every citation in this \
                 repository points at the filename, so the filename is the one to trust and the \
                 heading is the one to fix.",
                record.file_name,
                number,
                adr::spelled(heading),
                adr::spelled(heading)
            )),
            Some(_) => {}
        }
    }

    // And the third place the number is written: the label of the row in the index.
    //
    // A missing index is the neighbour's report to make — it names the file. Here the label half
    // simply has nothing to read, and saying the same absence twice would attribute it to the
    // wrong check.
    let index_path = dir.join("README.md");
    if index_path.is_file() {
        let index = std::fs::read_to_string(&index_path).context("reading docs/adr/README.md")?;
        for row in adr::index_rows(&index) {
            *checked += 1;
            // `Option` equality covers all four shapes at once: two numbers agreeing, two
            // absent numbers agreeing (a row labelled `[Guide]` over an unnumbered file is not
            // this table's business), and one side naming a number the other does not — a row
            // labelled `[Decision]` over a numbered file, which is the same defect wearing a
            // different hat, since the index labels rows by number because that is the spelling
            // every citation uses.
            let agrees = row.label_number == row.target_number;
            if agrees {
                continue;
            }
            problems.push(format!(
                "docs/adr/README.md links {} under the label [{}] — the row's text and its target \
                 disagree about the number. `decision_records_are_indexed` cannot see this: it \
                 only asks whether the target resolves, and a renumber that fixed the filename and \
                 left the label is exactly a target that resolves to the wrong row's text.",
                row.target, row.label
            ));
        }
    }
    Ok(())
}

/// A product must not carry the template's own machinery — the instantiation files, the
/// archetype skeletons, the CLI crate (a product runs the CLI from its pinned input), or the
/// template-only recipes and `cargo cli` alias. `cli new` strips all of it; this is the boundary
/// that fails when a piece reaches a product anyway. The template itself is exempt — it is
/// where that machinery lives — so the check only runs where the origin input marks a product.
fn template_only_content_is_absent(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    if !repo::is_product(root) {
        return Ok(());
    }

    for rel in [".template.toml", "templates", "tools/cli"] {
        *checked += 1;
        if root.join(rel).exists() {
            problems.push(format!(
                "{rel}: template-only content reached a product — `cli new` must remove it"
            ));
        }
    }

    // Both copies seed a product's dependency builds. A stale snapshot must not keep an
    // alias for a crate that the live product no longer contains.
    for rel in [".cargo/config.toml", "nix/deps-base/.cargo/config.toml"] {
        let path = root.join(rel);
        if !path.is_file() {
            continue;
        }
        *checked += 1;
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {rel}"))?;
        let config: toml::Value =
            toml::from_str(&text).with_context(|| format!("parsing {rel}"))?;
        if config
            .get("alias")
            .and_then(|aliases| aliases.get("cli"))
            .is_some()
        {
            problems.push(format!(
                "{rel}: the template-only `cargo cli` alias reached a product"
            ));
        }
    }

    let justfile = root.join("Justfile");
    if justfile.is_file() {
        *checked += 1;
        let text = std::fs::read_to_string(&justfile).context("reading Justfile")?;
        for marker in [
            "cargo-generate",
            "check-template",
            "check-cli-round-trip",
            "check-platform-templates",
            "check-archetype-templates",
        ] {
            if text.contains(marker) {
                problems.push(format!(
                    "Justfile: the template-only recipe or marker `{marker}` reached a product"
                ));
            }
        }
    }
    Ok(())
}

/// A product's citations to decision records that are not present locally must be absolute
/// links, never a bare `ADR NNNN`. `cli new` drops the template-only records (0006-0008, 0010,
/// 0024-0025) and rewrites the citations the kept records make to them as links to the forge;
/// a bare reference that slips through names a record the product does not carry, and a reader
/// who follows it finds nothing. The template is exempt — there every record 0001-0025 is
/// present, so a bare reference resolves.
fn adr_references_do_not_dangle(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    if !repo::is_product(root) {
        return Ok(());
    }
    let local: BTreeSet<u16> = adr::records(root)?.iter().map(|r| r.number).collect();

    let mut files = Vec::new();
    rename::walk(root, &mut files);
    for path in files {
        // Only the prose files are the citation surface; a lockfile or generated file is not
        // a place that cites a decision record, and a non-UTF-8 file cannot contain `ADR NNNN`.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = repo::rel(root, &path);
        for (number, linked) in adr_citations(&text) {
            *checked += 1;
            if !local.contains(&number) && !linked {
                problems.push(format!(
                    "{rel}: cites {} as a bare reference — that record is not in a product; \
                     cite it as an absolute link",
                    adr::spelled(number)
                ));
            }
        }
    }
    Ok(())
}

/// Every `ADR NNNN` spelling in a text, with whether it is immediately followed by `]` — the
/// close of a link label (`[ADR NNNN](https://…)`), which is the form a citation to an absent
/// record must take. Four digits only, matching [`adr::spelled`]: a heading `# 0024.` or an
/// index row `| [0024](…) |` names the record by number alone, not by `ADR NNNN`.
fn adr_citations(text: &str) -> Vec<(u16, bool)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"ADR " {
            let start = i + 4;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end - start == 4
                && let Ok(number) = text[start..end].parse::<u16>()
            {
                let linked = end < bytes.len() && bytes[end] == b']';
                out.push((number, linked));
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// Every file under `nix/` is reached from somewhere.
///
/// The environment is a set of modules wired by one list in `devenv.nix`, and a module
/// missing from that list is not an error anywhere: Nix never reads it, devenv comes up
/// without its packages, and the failure arrives much later as a tool that is not on PATH.
/// `nix/versions.nix` is reached differently — it is a parsed `versions.toml` rather than a
/// module, so its siblings `import` it — which is why this asks only that something names
/// the file, not that `imports` does.
fn nix_modules_are_reachable(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let dir = root.join("nix");
    if !dir.is_dir() {
        return Ok(());
    }

    // Values files read by the Justfile (not imported by devenv or a sibling module), so
    // they are not modules to wire into devenv and nothing in nix/ references them.
    let values_files = ["template-pin.nix"];

    // The direct modules (nix/*.nix) — the ones this check holds accountable. The pipeline
    // library lives in nix/pipeline/ and is reached through nix/ci.nix; its own files are not
    // modules to wire into devenv directly.
    let mut modules: Vec<String> = std::fs::read_dir(&dir)
        .context("reading nix")?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(ToOwned::to_owned))
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "nix"))
        .filter(|name| !values_files.contains(&name.as_str()))
        .collect();
    modules.sort();

    // Every .nix file under nix/, recursively, is a potential importer. A direct module may
    // be referenced by a sibling (`./<name>`) or by the pipeline library one level down
    // (`../<name>`), so both spellings are matched and the walk is recursive.
    let mut importers: Vec<PathBuf> = Vec::new();
    collect_nix_files(&dir, &mut importers);

    let devenv = std::fs::read_to_string(root.join("devenv.nix")).context("reading devenv.nix")?;
    let texts: Vec<(String, String)> = importers
        .iter()
        .map(|path| {
            let rel = path
                .strip_prefix(&dir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", repo::rel(root, path)))?;
            Ok((rel, text))
        })
        .collect::<Result<_>>()?;

    for module in &modules {
        *checked += 1;
        let from_devenv = devenv.contains(&format!("./nix/{module}"));
        // A file does not vouch for itself: a module naming its own filename in a comment
        // would otherwise look reachable.
        let from_other = texts.iter().any(|(rel, text)| {
            rel != module
                && (text.contains(&format!("./{module}")) || text.contains(&format!("../{module}")))
        });
        if !from_devenv && !from_other {
            problems.push(format!(
                "nix/{module} is reached from nothing — add it to devenv.nix's `imports`"
            ));
        }
    }
    Ok(())
}

/// Collect every `.nix` file under `dir`, recursively.
fn collect_nix_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_nix_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "nix") {
            out.push(path);
        }
    }
}

/// Every `telemetry::init(..)` call binds its guard to a name.
///
/// `init` returns a `#[must_use]` guard whose `Drop` flushes buffered spans and metrics;
/// discarding it loses the last batch — usually the interesting one, the last request's.
/// A *bare* `telemetry::init(..)?;` is already a compile error: `#[must_use]` fires
/// `unused_must_use`, and `just clippy` runs `-D warnings`. So the one shape that reaches
/// runtime broken is `let _ = telemetry::init(..)?;` — binding to the wildcard satisfies
/// the lint (the value *is* used: dropped) yet flushes nothing, and it compiles clean.
/// Only a scan like this catches it, which is why this doc comment exists: the next
/// reader must not "simplify" the check into accepting `let _`.
///
/// Stated limit: a text scan cannot see a helper that binds the guard and then fails to
/// return it — that needs semantic analysis. `#[must_use]` plus this scan cover every
/// shape a *call site* plausibly takes; a guard-swallowing helper is a code-review catch,
/// not a layout-invariant one, and pretending otherwise would be worse than saying so.
fn telemetry_guards_are_bound(root: &Path, checked: &mut usize, problems: &mut Vec<String>) {
    let mut files = Vec::new();
    // services/ and templates/, not only main.rs: a helper that calls `init` must be
    // checked at its own call site too. `walk` has already skipped build output.
    for base in ["services", "templates"] {
        let dir = root.join(base);
        if dir.is_dir() {
            rename::walk(&dir, &mut files);
        }
    }

    for file in files {
        if file.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let relative = repo::rel(root, &file);

        for (start, _) in text.match_indices("telemetry::init") {
            // A call, not prose: the name is followed by `(`. The observability tests and
            // the worker's run.rs mention `telemetry::init` in comments; none is a call.
            let after = &text[start + "telemetry::init".len()..];
            if !after.trim_start().starts_with('(') {
                continue;
            }
            // Defence in depth for a comment that *does* write `telemetry::init(`: skip
            // any occurrence sitting on a `//` line, so an example in prose is not read
            // as a call site.
            if mentioned_in_prose(&text, start) {
                continue;
            }

            *checked += 1;

            // The binding is the `let <pattern> =` that precedes the call. Bound the
            // window at the previous `;` so an earlier statement's `let` is not mistaken
            // for this one's; a `;` inside an intervening comment is harmless because
            // `rfind("let ")` still lands on the binding nearest the call.
            let before = &text[..start];
            let stmt_start = before.rfind(';').map_or(0, |i| i + 1);
            let prefix = before[stmt_start..].trim();

            let binding = prefix
                .strip_suffix('=')
                .and_then(|before_eq| {
                    before_eq
                        .rfind("let ")
                        .map(|i| before_eq[i + "let ".len()..].trim())
                })
                // The first token is the name; a type annotation (`let g: T =`) or the
                // wildcard's own `:` must not be read as part of it.
                .and_then(|pattern| {
                    pattern
                        .split(|c: char| c.is_whitespace() || c == ':')
                        .next()
                        .filter(|name| !name.is_empty())
                });

            // `_` (or no `let .. =` at all — a bare call, already a compile error but
            // reported here so the fix is one message) drops the guard immediately.
            if !matches!(binding, Some(name) if name != "_") {
                problems.push(format!(
                    "{relative}: `telemetry::init(..)` must be bound to a name — \
                     `let _ = ..` drops the guard at once and flushes nothing, losing the \
                     last batch of spans and metrics. Write `let _telemetry = \
                     telemetry::init(..)?;` and hold it for the life of the process."
                ));
            }
        }
    }
}

/// The one workflow that writes back to the forge, and the checkout setting that decides
/// which credential it writes with.
const ADVISORIES_PATH: &str = ".forgejo/workflows/advisories.yaml";

/// The scheduled checkout persists no credentials, because that is the workflow that pushes.
///
/// `actions/checkout` defaults to `persist-credentials: true`, which records the runner's job
/// token in the checkout's git config as `http.<server>/.extraheader = AUTHORIZATION: basic
/// <job token>`, scoped to this repository's server. A header git was handed is sent before any
/// credential helper is consulted, and `cargo xtask advisory-pr` pushes to that same server URL —
/// so a persisted one displaces the bot token ADR 0023 specifies. The push then either succeeds
/// as the job token, which is the credential that record rejects because it can merge and because
/// every run of every workflow has it, or `master`'s protection refuses it. Both readings end the
/// same way: the credential the design is about is never the credential used, and the step still
/// goes green.
///
/// Asserted against the file rather than neutralised in the push, because the config belongs to
/// the checkout and the push cannot name the key the runner wrote — git matches a URL-scoped
/// `http.<url>.` section against the remote URL, so a `-c` override would have to guess the
/// spelling. What the push can control it already does (`credential.helper=` and a `GIT_ASKPASS`
/// script, both in `tools/xtask/src/advisory_pr.rs`); this is the half it cannot.
///
/// Only this workflow is checked, and that is the point rather than an omission: it is the only
/// one that writes. A read-only pipeline has no credential to leak into, and demanding the
/// setting of a workflow that never pushes would be noise that teaches people to ignore the
/// finding when it matters.
/// The publish step maps both registry secrets from the repository's secrets.
///
/// Forgejo Actions does not export secrets into a step's environment unless the workflow maps
/// them, so a step that reads `HARBOR_USERNAME` without an `env:` entry sees an empty string.
/// `publish::push` treats two empty credentials as "not configured" — the right default for
/// this repository, which has no registry — so a product with the secret stored and the
/// mapping missing gets a green pipeline that pushes nothing, forever. The mapping is one line
/// per variable and it is invisible: the YAML is valid either way and the job passes either
/// way, which is what makes it worth a check rather than a sentence in a README.
fn the_publish_step_receives_its_secrets(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) {
    let workflow = root.join(".forgejo/workflows/ci.yaml");
    let Ok(text) = std::fs::read_to_string(&workflow) else {
        return;
    };
    *checked += 1;

    let Some(block) = step_block(&text, "publish") else {
        problems.push(
            ".forgejo/workflows/ci.yaml: no `- name: publish` step — the publishing gate is \
             invoked by the workflow, so its step (and the credential mapping it needs) has to \
             be there"
                .to_owned(),
        );
        return;
    };

    for variable in ["HARBOR_USERNAME", "HARBOR_PASSWORD"] {
        let expected = format!("{variable}: ${{{{ secrets.{variable} }}}}");
        if !block.contains(&expected) {
            problems.push(format!(
                ".forgejo/workflows/ci.yaml: the publish step does not map `{variable}` from \
                 `secrets.{variable}` — Forgejo does not put a secret in a step's environment \
                 without the mapping, so `just publish::push` would see an empty value, take its \
                 \"not configured\" path and report success forever. The step needs:\n\n    \
                 env:\n      {expected}\n"
            ));
        }
    }
}

/// One `- name: <name>` step, from its line up to the next sibling step.
///
/// Indentation, not a YAML parser: a step's keys and its `env:` block are indented further than
/// the `- name:` line, and the next step is not. `xtask` has no YAML dependency, and the one
/// thing this needs to read is a block whose shape the workflow itself fixes.
fn step_block(text: &str, name: &str) -> Option<String> {
    let marker = format!("- name: {name}");
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|line| line.trim() == marker)?;
    let indent = lines[start]
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();

    let mut block = String::new();
    for line in &lines[start + 1..] {
        if line.trim().is_empty() {
            block.push('\n');
            continue;
        }
        let line_indent = line.chars().take_while(|c| c.is_whitespace()).count();
        if line_indent <= indent {
            break;
        }
        block.push_str(line);
        block.push('\n');
    }
    Some(block)
}

fn the_scheduled_checkout_persists_no_credentials(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) {
    let path = root.join(ADVISORIES_PATH);
    let Ok(workflow) = std::fs::read_to_string(&path) else {
        // Absent means the nightly is gone, and with it the only push. Nothing to assert.
        return;
    };

    let lines: Vec<&str> = workflow.lines().collect();
    let Some(step) = lines.iter().position(|line| {
        // The list marker first: every step is a `- ` item, and a line that starts with
        // spaces matches neither `- ` nor `uses:` until the whitespace is gone.
        let trimmed = line.trim().trim_start_matches("- ").trim_start();
        trimmed.starts_with("uses: actions/checkout")
    }) else {
        *checked += 1;
        problems.push(format!(
            "{ADVISORIES_PATH}: no `actions/checkout` step, but the advisory bump branches from \
             the checkout this workflow makes — the step is gone rather than moved"
        ));
        return;
    };

    // The step's own block: everything indented deeper than the `- uses:` line, stopping at the
    // next step. Blank lines and comments are kept, because the reason for the setting is written
    // above it in the file and skipping them would end the block early.
    let indent_of = |line: &str| line.len() - line.trim_start().len();
    let step_indent = indent_of(lines[step]);
    let block: Vec<&str> = lines[step + 1..]
        .iter()
        .copied()
        .take_while(|line| {
            let trimmed = line.trim();
            trimmed.is_empty() || trimmed.starts_with('#') || indent_of(line) > step_indent
        })
        .collect();

    *checked += 1;
    let setting = block.iter().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key.trim() == "persist-credentials").then(|| value.trim())
    });
    if setting != Some("false") {
        problems.push(format!(
            "{ADVISORIES_PATH}: the checkout does not set `persist-credentials: false` (found \
             {setting:?}). The default writes the runner's job token into the checkout's git \
             config as `http.<server>/.extraheader`, which git sends before consulting any \
             credential helper — so `cargo xtask advisory-pr` would push with the job token \
             instead of the scoped one ADR 0023 specifies, and go green doing it"
        ));
    }

    // `with:` is asserted separately because `persist-credentials` on the job rather than the
    // step is an ordinary key nothing reads, and the default still applies — a spelling that
    // looks correct in a diff and protects nothing.
    *checked += 1;
    if !block.iter().any(|line| line.trim() == "with:") {
        problems.push(format!(
            "{ADVISORIES_PATH}: `persist-credentials` belongs to the checkout step's `with:`, \
             not to the job — on a job it is an ordinary key nothing reads, and the default \
             still applies"
        ));
    }
}

/// The scanners behind `translate_with_arguments_match_the_catalog`, against the shapes
/// the real call sites have and the shapes that would silently fool them. The invariant
/// itself walks the tree, so its end-to-end proof is the run over this one — a clean
/// checkout passes with every `Key::…` call counted (the six in the shells and the
/// skeletons, plus the two well-formed fixtures below, which the walker reads as the
/// source text they are), and renaming `count = …` to `contn = …` in any shell turns
/// that into two problems: the dropped argument and the placeholder left unfilled.
#[cfg(test)]
// The fixtures below are input *data* for the scanner: `=> "{:08}"` is the literal shape
// a `translate_with!` format specifier has in source, and the lint reads it as a format
// string that never reaches a formatting macro. It is not one; it is the thing parsed.
#[allow(clippy::literal_string_with_formatting_args)]
mod tests {
    use super::*;

    /// The workspace-shape problems a tree with these `members` and `services/` contents has.
    fn layout_problems(members: &str, services: Option<&str>) -> Vec<String> {
        let fixture = Fixture::new("service-layout");
        fixture
            .file(
                "Cargo.toml",
                &format!("[workspace]\nmembers = [{members}]\n"),
            )
            .file("crates/README.md", "crates")
            .file("tools/README.md", "tools");
        if let Some(path) = services {
            fixture.file(path, "");
        }
        let (mut checked, mut problems) = (0, Vec::new());
        workspace_globs_match_manifest(&fixture.root, &mut checked, &mut problems)
            .expect("the fixture manifest parses");
        category_dirs_have_guards(&fixture.root, &mut checked, &mut problems)
            .expect("the fixture manifest parses");
        problems
    }

    #[test]
    fn a_product_without_services_needs_neither_the_glob_nor_the_directory() {
        let problems = layout_problems(r#""crates/*", "tools/*""#, None);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_services_directory_still_requires_its_glob_and_guard() {
        // The template's own shape: the README is all there is, and the glob must be there.
        let problems = layout_problems(r#""crates/*", "tools/*""#, Some("services/README.md"));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("missing the required glob \"services/*\"")),
            "{problems:?}"
        );
        // A crate dropped into an unglobbed `services/` would silently not be a member.
        let problems = layout_problems(r#""crates/*", "tools/*""#, Some("services/x/Cargo.toml"));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("missing the required glob \"services/*\"")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("services/README.md is missing")),
            "{problems:?}"
        );
        let problems = layout_problems(
            r#""crates/*", "services/*", "tools/*""#,
            Some("services/README.md"),
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_services_glob_without_its_directory_is_reported() {
        let problems = layout_problems(r#""crates/*", "services/*", "tools/*""#, None);
        assert!(
            problems.iter().any(|p| p.contains("services/ is missing")),
            "{problems:?}"
        );
    }

    #[test]
    fn web_shell_requires_a_trunk_workspace_member() {
        for (reference, trunk, valid) in [
            ("web-shell = \"missing\"", true, false),
            ("web-shell = \"browser\"", false, false),
            ("web-shell = 42", true, false),
            ("web-shell = \"browser\"", true, true),
            ("", true, true),
        ] {
            let fixture = Fixture::new("web-shell");
            fixture.file(
                "services/api/Cargo.toml",
                &format!(
                    "[package]\nname = \"api\"\n[package.metadata.app]\nkind = \"service\"\n{reference}\n"
                ),
            );
            fixture.file(
                "crates/browser/Cargo.toml",
                &format!(
                    "[package]\nname = \"browser\"\n[package.metadata.app]\ntrunk = {trunk}\n"
                ),
            );
            // Non-service metadata is not consumed by discovery or rename/drop.
            fixture.file(
                "crates/library/Cargo.toml",
                "[package]\nname = \"library\"\n[package.metadata.app]\nweb-shell = \"missing\"\n",
            );
            fixture.file(
                "tools/tool/Cargo.toml",
                "[package]\nname = \"tool\"\n[package.metadata.app]\nweb-shell = 42\n",
            );
            fixture.file(
                "apps/widget/app/Cargo.toml",
                "[package]\nname = \"widget\"\n[package.metadata.app]\nkind = \"app\"\nweb-shell = \"missing\"\n",
            );
            // A matching manifest outside the workspace cannot satisfy the reference.
            fixture.file(
                "templates/missing/Cargo.toml",
                "[package]\nname = \"missing\"\n[package.metadata.app]\ntrunk = true\n",
            );
            let mut problems = Vec::new();
            app_metadata_is_valid(&fixture.root, &mut 0, &mut problems).expect("check metadata");
            assert_eq!(problems.is_empty(), valid, "{reference}: {problems:#?}");
            if !valid {
                assert_eq!(problems.len(), 1, "{problems:#?}");
                assert!(problems[0].contains("web-shell"), "{problems:#?}");
            }
        }
    }

    /// The breakdown accounts for the whole count, or the count is not attributable.
    ///
    /// This is the assertion that makes `step!` non-optional. A check added to `check()` as a
    /// bare call still increments `checked` and contributes no line, which would leave the
    /// breakdown quietly short — the same shape of defect as a gate that exists and runs
    /// nowhere, one level down. Nothing in the language prevents the bare call; this refuses
    /// the result of one.
    ///
    /// It reads the real repository, like its neighbours here, so it also fails if a check
    /// starts counting conditionally in a way the breakdown cannot see.
    #[test]
    fn the_breakdown_accounts_for_every_check() {
        let report = check().expect("the invariants run against this repository");
        let summed: usize = report.breakdown.iter().map(|(_, count)| count).sum();
        assert_eq!(
            summed, report.checked,
            "the breakdown sums to {summed} and the reported total is {}, so some check is \
             counting outside `step!` — add it to the list in `check()` rather than relaxing \
             this test",
            report.checked
        );
        assert!(
            !report.breakdown.is_empty(),
            "no check reported a contribution, which means the breakdown stopped being built"
        );
    }

    fn core_dependency_problems(fixture: &Fixture) -> Vec<String> {
        let mut checked = 0;
        let mut problems = Vec::new();
        cores_stay_domain_level(&fixture.root, &mut checked, &mut problems)
            .expect("the fixture is readable");
        problems
    }

    #[test]
    fn a_core_may_not_depend_on_the_adapters_or_their_runtime() {
        let fixture = Fixture::new("core-adapters");
        fixture.file(
            "apps/fleet/core/Cargo.toml",
            "[package]\nname = \"fleet-core\"\n\n[dependencies]\n\
             auspex-domain = { path = \"../../../crates/auspex-domain\" }\n\
             auspex-effects = { path = \"../../../crates/auspex-effects\" }\n\n\
             [dev-dependencies]\ntokio = \"1\"\n",
        );
        let problems = core_dependency_problems(&fixture);
        assert_eq!(problems.len(), 2, "{problems:#?}");
        assert!(problems[0].contains("`auspex-effects`"), "{problems:#?}");
        assert!(problems[1].contains("`tokio`"), "{problems:#?}");
    }

    #[test]
    fn a_renamed_forbidden_dependency_is_still_refused() {
        let fixture = Fixture::new("core-renamed");
        fixture.file(
            "apps/fleet/core/Cargo.toml",
            "[package]\nname = \"fleet-core\"\n\n[dependencies]\n\
             fx = { package = \"auspex-effects\", path = \"../../../crates/auspex-effects\" }\n",
        );
        let problems = core_dependency_problems(&fixture);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("`auspex-effects`"), "{problems:#?}");
    }

    #[test]
    fn a_core_on_the_domain_alone_passes() {
        let fixture = Fixture::new("core-domain-only");
        fixture.file(
            "apps/fleet/core/Cargo.toml",
            "[package]\nname = \"fleet-core\"\n\n[dependencies]\n\
             auspex-domain = { path = \"../../../crates/auspex-domain\" }\ncrux_core = \"0.20\"\n",
        );
        assert_eq!(core_dependency_problems(&fixture), Vec::<String>::new());
    }

    fn template_only_problems(fixture: &Fixture) -> Vec<String> {
        let mut checked = 0;
        let mut problems = Vec::new();
        template_only_content_is_absent(&fixture.root, &mut checked, &mut problems)
            .expect("the fixture is readable");
        problems
    }

    #[test]
    fn a_product_cannot_keep_the_cli_alias_in_either_cargo_config() {
        for rel in [".cargo/config.toml", "nix/deps-base/.cargo/config.toml"] {
            let fixture = Fixture::new("product-cli-alias");
            fixture
                .file(
                    "flake.nix",
                    "template.url = \"git+https://example.com/template\";",
                )
                .file(rel, "[alias]\ncli = [\"run\", \"-p\", \"cli\", \"--\"]\n");
            let problems = template_only_problems(&fixture);
            assert_eq!(problems.len(), 1, "{problems:#?}");
            assert!(problems[0].contains(rel), "{problems:#?}");
            assert!(problems[0].contains("`cargo cli` alias"), "{problems:#?}");
        }
    }

    #[test]
    fn a_product_may_keep_other_cargo_aliases_and_comments() {
        let fixture = Fixture::new("product-other-aliases");
        fixture.file(
            "flake.nix",
            "template.url = \"git+https://example.com/template\";",
        );
        for rel in [".cargo/config.toml", "nix/deps-base/.cargo/config.toml"] {
            fixture.file(
                rel,
                "[alias]\n# cli = \"removed\"\nxtask = \"run -p xtask --\"\n",
            );
        }
        assert_eq!(template_only_problems(&fixture), Vec::<String>::new());
    }

    #[test]
    fn the_template_may_keep_the_cli_alias() {
        let fixture = Fixture::new("template-cli-alias");
        fixture.file(".cargo/config.toml", "[alias]\ncli = \"run -p cli --\"\n");
        assert_eq!(template_only_problems(&fixture), Vec::<String>::new());
    }

    #[test]
    fn obsolete_generator_filenames_are_not_reserved_in_a_product() {
        let fixture = Fixture::new("product-owned-generator-files");
        fixture
            .file(
                "flake.nix",
                "template.url = \"git+https://example.com/template\";",
            )
            .file("cargo-generate.toml", "[template]\n")
            .file("check-template.toml", "# Product-owned configuration\n");
        assert_eq!(template_only_problems(&fixture), Vec::<String>::new());
    }

    #[test]
    fn current_template_only_paths_and_recipes_still_fail_in_a_product() {
        let fixture = Fixture::new("product-template-content");
        fixture
            .file(
                "flake.nix",
                "template.url = \"git+https://example.com/template\";",
            )
            .file(".template.toml", "")
            .file("templates/example/file", "")
            .file("tools/cli/Cargo.toml", "")
            .file("Justfile", "check-template:\n    echo template\n");
        let problems = template_only_problems(&fixture);
        assert_eq!(problems.len(), 4, "{problems:#?}");
        for expected in [".template.toml", "templates:", "tools/cli", "Justfile"] {
            assert!(
                problems.iter().any(|problem| problem.contains(expected)),
                "{problems:#?}"
            );
        }
    }

    fn product_adr_fixture() -> Fixture {
        let fixture = Fixture::new("product-adrs");
        fixture.file(
            "flake.nix",
            "template.url = \"git+https://example.com/template\";",
        );
        for &number in INHERITED_ADRS {
            let number = adr::spelled(number);
            fixture.file(
                &format!("docs/adr/{number}-inherited.md"),
                &adr_record(&number, "Inherited"),
            );
        }
        fixture
    }

    #[test]
    fn a_product_keeps_inherited_adrs_and_starts_its_own_at_0100() {
        let fixture = product_adr_fixture();
        fixture
            .file("docs/adr/0100-product.md", &adr_record("0100", "Product"))
            .file("docs/adr/0101-next.md", &adr_record("0101", "Next"));
        let (_, problems) = numbering(&fixture);
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn a_product_still_requires_every_inherited_adr() {
        let fixture = product_adr_fixture();
        std::fs::remove_file(fixture.root.join("docs/adr/0001-inherited.md"))
            .expect("removing an inherited record");
        let (_, problems) = numbering(&fixture);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("no record numbered 0001"),
            "{problems:#?}"
        );
    }

    #[test]
    fn a_product_still_rejects_dropped_adrs() {
        let fixture = product_adr_fixture();
        fixture.file("docs/adr/0006-dropped.md", &adr_record("0006", "Dropped"));
        let (_, problems) = numbering(&fixture);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("0006 is template-only"),
            "{problems:#?}"
        );
    }

    #[test]
    fn a_product_still_requires_contiguous_own_adrs_from_0100() {
        let fixture = product_adr_fixture();
        fixture.file("docs/adr/0101-product.md", &adr_record("0101", "Product"));
        let (_, problems) = numbering(&fixture);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("no record numbered 0100"),
            "{problems:#?}"
        );
    }

    /// The rule, live: this repository's records are contiguous from 0001, none shares a number,
    /// and each one says its own number in its own heading and in its own index row.
    ///
    /// The ticket's other half is a fixture proving the check fires; this one is the proof that
    /// it does not fire here, which is what "green the day it lands" means and what makes
    /// `just check-template` pass in a product that carries all of these files.
    #[test]
    fn the_real_records_are_numbered() {
        let root = repo::root();
        if !root.join("templates").is_dir() {
            return;
        }
        let mut checked = 0;
        let mut problems = Vec::new();
        decision_records_are_numbered(&root, &mut checked, &mut problems)
            .expect("docs/adr is readable in this repository");
        assert!(problems.is_empty(), "{problems:#?}");

        let records = adr::records(&root).expect("docs/adr");
        let numbers: Vec<usize> = records
            .iter()
            .map(|record| usize::from(record.number))
            .collect();
        let contiguous: Vec<usize> = (1..=records.len()).collect();
        assert_eq!(numbers, contiguous, "{numbers:?}");

        let index = std::fs::read_to_string(root.join("docs/adr/README.md")).expect("the index");
        assert_eq!(
            checked,
            records.len() + adr::index_rows(&index).len() + 1,
            "one per record for its heading, one per index row for its label, one for the \
             sequence — measured here, not predicted"
        );
    }

    /// A record file's text, with a heading carrying a number chosen independently of the one in
    /// its filename — which is the whole point: the two are separate facts and the fixtures have
    /// to be able to make them disagree.
    fn adr_record(number: &str, title: &str) -> String {
        format!(
            "# {number}. {title}\n\n- **Status:** Accepted\n- **Date:** 2026-01-01\n\n## Context\n"
        )
    }

    /// A `docs/adr/` under a throwaway root, with the index the real one has.
    ///
    /// `files` are `(file_name, contents)` pairs, so a fixture can name a file and its heading
    /// differently rather than being limited to well-formed records.
    fn adr_fixture(name: &str, files: &[(&str, &str)], rows: &[&str]) -> Fixture {
        let fixture = Fixture::new(name);
        for (file, contents) in files {
            fixture.file(&format!("docs/adr/{file}"), contents);
        }
        fixture.file(
            "docs/adr/README.md",
            &format!(
                "# Architecture decision records\n\n| # | Decision | Status |\n|---|---|---|\n{}\n\n\
                 ## Writing one\n\nProse.\n",
                rows.join("\n")
            ),
        );
        fixture
    }

    /// Run the numbering check over a fixture, and hand back what it counted and what it found.
    fn numbering(fixture: &Fixture) -> (usize, Vec<String>) {
        let mut checked = 0;
        let mut problems = Vec::new();
        decision_records_are_numbered(&fixture.root, &mut checked, &mut problems)
            .expect("the fixture is readable");
        (checked, problems)
    }

    /// The collision the ticket is about: two branches, one next free number, both written.
    #[test]
    fn a_duplicate_adr_number_is_reported() {
        let fixture = adr_fixture(
            "adr-duplicate",
            &[
                ("0001-a.md", &adr_record("0001", "A")),
                ("0001-b.md", &adr_record("0001", "B")),
                ("0002-c.md", &adr_record("0002", "C")),
            ],
            &[
                "| [0001](0001-a.md) | A | Accepted |",
                "| [0001](0001-b.md) | B | Accepted |",
                "| [0002](0002-c.md) | C | Accepted |",
            ],
        );

        let (_, problems) = numbering(&fixture);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        let problem = &problems[0];
        for expected in ["0001-a.md", "0001-b.md", "0001"] {
            assert!(problem.contains(expected), "{problem}");
        }
    }

    /// A hole in the sequence, which is what a renumber that deleted its own row leaves behind.
    #[test]
    fn a_gap_in_the_adr_sequence_is_reported() {
        let fixture = adr_fixture(
            "adr-gap",
            &[
                ("0001-a.md", &adr_record("0001", "A")),
                ("0002-b.md", &adr_record("0002", "B")),
                ("0004-d.md", &adr_record("0004", "D")),
            ],
            &[
                "| [0001](0001-a.md) | A | Accepted |",
                "| [0002](0002-b.md) | B | Accepted |",
                "| [0004](0004-d.md) | D | Accepted |",
            ],
        );

        let (_, problems) = numbering(&fixture);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        let problem = &problems[0];
        for expected in ["0003", "0004-d.md"] {
            assert!(problem.contains(expected), "{problem}");
        }
    }

    /// A file whose two spellings of its own number disagree — the renumber that stopped halfway.
    #[test]
    fn an_adr_heading_that_disagrees_with_its_filename_is_reported() {
        let fixture = adr_fixture(
            "adr-heading",
            &[
                ("0001-a.md", &adr_record("0001", "A")),
                ("0002-b.md", &adr_record("0007", "B")),
            ],
            &[
                "| [0001](0001-a.md) | A | Accepted |",
                "| [0002](0002-b.md) | B | Accepted |",
            ],
        );

        let (_, problems) = numbering(&fixture);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        let problem = &problems[0];
        for expected in ["0002-b.md", "0002", "0007"] {
            assert!(problem.contains(expected), "{problem}");
        }
    }

    /// The other half of the same-looking defect, reported as itself.
    ///
    /// "The heading disagrees" and "there is no heading" read alike from the outside and only one
    /// of them is a renumber with citations to chase, so the messages have to separate them —
    /// which is why `Record::heading_number` is an `Option` and not a comparison.
    #[test]
    fn an_adr_with_no_numbered_heading_is_reported_as_that_and_not_as_a_disagreement() {
        let fixture = adr_fixture(
            "adr-no-heading",
            &[
                ("0001-a.md", &adr_record("0001", "A")),
                (
                    "0002-b.md",
                    "## Context\n\nBecause the heading was never written.\n",
                ),
            ],
            &[
                "| [0001](0001-a.md) | A | Accepted |",
                "| [0002](0002-b.md) | B | Accepted |",
            ],
        );

        let (_, problems) = numbering(&fixture);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        let problem = &problems[0];
        for expected in ["0002-b.md", "0002", "no `# NNNN.` heading"] {
            assert!(problem.contains(expected), "{problem}");
        }
        // Not the disagreement's message: no second number is being named, because the file has
        // no heading to name one.
        assert!(!problem.contains("by its own"), "{problem}");
    }

    /// The one the ticket did not ask for, and the one the existing index check cannot see.
    ///
    /// A renumber that fixed the filename and left the label produces a row whose target resolves
    /// perfectly — which is the only question `decision_records_are_indexed` asks.
    #[test]
    fn an_index_row_whose_label_disagrees_with_its_target_is_reported() {
        let fixture = adr_fixture(
            "adr-label",
            &[
                ("0001-a.md", &adr_record("0001", "A")),
                ("0002-b.md", &adr_record("0002", "B")),
            ],
            &[
                "| [0001](0001-a.md) | A | Accepted |",
                "| [0001](0002-b.md) | B | Accepted |",
            ],
        );

        let (_, problems) = numbering(&fixture);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        let problem = &problems[0];
        for expected in ["0002-b.md", "[0001]"] {
            assert!(problem.contains(expected), "{problem}");
        }
    }

    /// A well-formed fixture is silent, so the five above are findings and not this check's
    /// disposition towards every tree it is pointed at.
    #[test]
    fn a_numbered_fixture_is_silent() {
        let fixture = adr_fixture(
            "adr-clean",
            &[
                ("0001-a.md", &adr_record("0001", "A")),
                ("0002-b.md", &adr_record("0002", "B")),
            ],
            &[
                "| [0001](0001-a.md) | A | Accepted |",
                "| [0002](0002-b.md) | B | Accepted |",
            ],
        );

        let (checked, problems) = numbering(&fixture);
        assert!(problems.is_empty(), "{problems:#?}");
        assert_eq!(checked, 5, "two headings, two labels, one sequence");
    }

    #[test]
    fn finds_placeholder_names() {
        assert_eq!(placeholder_names("Count is: %{count}"), ["count"]);
        assert_eq!(
            placeholder_names("%{a} and %{a} again"),
            ["a", "a"],
            "once per occurrence — the caller collects into a set"
        );
        assert_eq!(placeholder_names("no placeholders"), [] as [String; 0]);
        // rust-i18n's specifier syntax, which the generated macro does not consume but
        // the catalog may still carry.
        assert_eq!(placeholder_names("%{sn:.2}"), ["sn"]);
        // Prose that merely looks like a placeholder.
        assert_eq!(placeholder_names("100%{ of the time"), [] as [String; 0]);
        assert_eq!(placeholder_names("%{not closed"), [] as [String; 0]);
        assert_eq!(placeholder_names("%{a-b}"), [] as [String; 0]);
    }

    #[test]
    fn parses_the_shape_every_call_site_has() {
        let (key, args) = parse_translate_with(
            "Key::CountIs, self.locale, count = i18n::number(view.count, self.locale)",
        )
        .expect("a generated key");
        assert_eq!(key, "CountIs");
        assert_eq!(
            args.iter().map(String::as_str).collect::<Vec<_>>(),
            ["count"]
        );

        // The key may arrive fully qualified, and the argument list may be empty or
        // carry a trailing comma and a format specifier.
        let (key, args) =
            parse_translate_with("i18n::Key::Working, locale.get(),").expect("a generated key");
        assert_eq!(key, "Working");
        assert!(args.is_empty());

        let (_, args) = parse_translate_with("Key::Lure, loc, name = \"Ana\", sn = 7 => \"{:03}\"")
            .expect("a generated key");
        assert_eq!(
            args.iter().map(String::as_str).collect::<Vec<_>>(),
            ["name", "sn"]
        );
    }

    /// A literal key is the crate's own tests and the generator's doctests: there is no
    /// catalog entry to compare against, so the call is skipped rather than checked.
    #[test]
    fn skips_keys_that_are_not_generated_variants() {
        assert!(parse_translate_with("\"%{count} fish\", Locale::FALLBACK, count = 3").is_none());
        assert!(parse_translate_with("key_var, locale, count = 3").is_none());
    }

    #[test]
    fn finds_the_end_of_a_call() {
        assert_eq!(closing_paren("a, b)rest"), Some(4));
        assert_eq!(closing_paren("f(g(x)), y)rest"), Some(10));
        // A closer inside a literal is a value, not the end of the call.
        assert_eq!(closing_paren("a, \")\", b)rest"), Some(9));
        assert_eq!(closing_paren("a, ')', b)rest"), Some(9));
        // Braces in a format specifier do not outbalance the call's own parentheses.
        assert_eq!(closing_paren("sn = 7 => \"{:08}\")"), Some(17));
    }

    #[test]
    fn extracts_call_bodies_from_source() {
        let text = "\
let a = 1;
let s = translate_with!(Key::CountIs, loc, count = n);
// translate_with!(Key::Commented, loc) is prose
let t = translate_with!(
    Key::Working,
    loc,
);
let u = translate_with!(plural_key(Key::Apples, n, loc), loc, count = number(n, loc));
";
        let calls = translate_with_calls(text);
        // The commented call is *found* — skipping it is the line-start guard's job at
        // the call site, which knows the file; the scanner only knows strings.
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].1, "Key::CountIs, loc, count = n");
        assert_eq!(calls[1].1, "Key::Commented, loc");
        assert_eq!(calls[2].1.trim(), "Key::Working,\n    loc,");
        assert!(
            parse_translate_with(&calls[2].1).is_some_and(|(k, a)| k == "Working" && a.is_empty())
        );

        // The plural shape, and the fixture that keeps the invariant above honest about
        // it: this file is walked too, so the line below is checked against the real
        // catalog on every `cargo xtask check`.
        assert_eq!(
            calls[3].1,
            "plural_key(Key::Apples, n, loc), loc, count = number(n, loc)"
        );
        assert!(
            parse_translate_with(&calls[3].1).is_some_and(|(k, a)| k == "Apples"
                && a.iter().map(String::as_str).collect::<Vec<_>>() == ["count"])
        );
    }

    /// A `plural_key(..)` key argument: the call is nested, its commas are inside
    /// parentheses the depth-aware split keeps in one segment, and the variant is still
    /// what follows `Key::`.
    #[test]
    fn parses_a_plural_key_call() {
        let (key, args) = parse_translate_with(
            "i18n::plural_key(Key::Apples, count, locale), locale, count = number(count, locale)",
        )
        .expect("a plural family is a generated key too");
        assert_eq!(key, "Apples");
        assert_eq!(
            args.iter().map(String::as_str).collect::<Vec<_>>(),
            ["count"]
        );

        assert!(is_plural_key(
            "plural_key(Key::Apples, count, locale), locale"
        ));
        assert!(is_plural_key("i18n::plural_key(Key::Apples, n, l), l"));
        // An ordinary call is not a plural one — and neither is a call that merely uses
        // `plural_key` to build an *argument*, which the family check has no business
        // running against.
        assert!(!is_plural_key("Key::CountIs, locale, count = 2"));
        assert!(!is_plural_key(
            "Key::CountIs, locale, other = plural_key(Key::Apples, 2, locale)"
        ));
    }

    /// The catalog half, live: a plural family is one entry, and it carries the
    /// placeholders of every category of it, because the call site cannot know which one
    /// the reader's language will select.
    #[test]
    fn a_plural_family_is_one_entry_of_the_real_catalog() {
        let catalog = catalog_placeholders();

        let (key, placeholders) = catalog
            .get("Apples")
            .expect("`apples` is in the catalog this xtask compiles against");
        assert_eq!(key, "apples", "the family, which is what a call site names");
        assert_eq!(
            placeholders.iter().map(String::as_str).collect::<Vec<_>>(),
            ["count"]
        );

        // Not entries of their own: there is no `Key::ApplesFew` to miss.
        assert!(!catalog.contains_key("ApplesFew"));
        assert!(!catalog.contains_key("ApplesOne"));
    }

    /// The rule, live: every family the compiled catalog declares is named in the example
    /// app's shells and in the skeleton's — and the walk saw something to check.
    #[test]
    fn every_plural_family_is_named_in_the_shells() {
        if !repo::root().join("templates").is_dir() {
            return;
        }
        let mut checked = 0;
        let mut problems = Vec::new();
        plural_families_reach_a_shell(&repo::root(), &mut checked, &mut problems);
        assert!(problems.is_empty(), "{problems:#?}");
        // A pass with nothing walked is indistinguishable from a pass with nothing to
        // render, and this file exists because of the difference.
        assert!(
            checked >= i18n::Key::PLURAL_FAMILIES.len() * PLURAL_SHELL_TREES.len(),
            "{checked} checks ran, so a tree or a family was missing"
        );
    }

    /// The failure this invariant exists for, as a fixture: a family every app is shipped and
    /// no shell calls. It cannot be staged in this repository's own trees, where deleting the
    /// call site breaks the check that would report it — which is the argument for a fixture
    /// and for the live test above, one proving the rule passes here, this one proving it
    /// fails somewhere.
    #[test]
    fn a_plural_family_no_shell_calls_is_reported() {
        let fixture = Fixture::new("unrendered");
        // Translating the count is not enough — `count_is` is a key like any other. The
        // commented call below is the other half of the fixture: a family named in prose
        // renders no more than one never written, and the comment guard this check reuses is
        // what refuses to count it.
        // A Crux app has a core manifest; without one the apps/*/shells tree is not checked (`has_crux_apps`).
        fixture.file("apps/probe/core/Cargo.toml", "");
        fixture.file(
            "apps/probe/shells/tui/src/main.rs",
            "\
let line = translate_with!(Key::CountIs, locale, count = count);
// translate_with!(plural_key(Key::Apples, count, locale), locale, count = count);
",
        );
        fixture.file(
            "templates/crux-app/apps/__name__/shells/tui/src/main.rs",
            "fn main() {}\n",
        );

        let mut checked = 0;
        let mut problems = Vec::new();
        plural_families_reach_a_shell(&fixture.root, &mut checked, &mut problems);

        assert_eq!(problems.len(), 2, "{problems:#?}");
        for problem in &problems {
            assert!(
                problem.contains("`apples.*`"),
                "the report should name the family the catalog declares: {problem}"
            );
        }
    }

    /// The same fixture with the call site restored, including the one that was commented
    /// out: which of the two trees calls the family is not the rule, and a commented call is
    /// not the family being rendered.
    #[test]
    fn a_plural_family_some_shell_calls_is_not() {
        let fixture = Fixture::new("rendered");
        // A Crux app has a core manifest; without one the apps/*/shells tree is not checked (`has_crux_apps`).
        fixture.file("apps/probe/core/Cargo.toml", "");
        fixture.file(
            "apps/probe/shells/tui/src/main.rs",
            "\
let line = translate_with!(
    plural_key(Key::Apples, count, locale),
    locale,
    count = count,
);
",
        );
        fixture.file(
            "templates/crux-app/apps/__name__/shells/tui/src/main.rs",
            "fn main() {}\n",
        );

        let mut checked = 0;
        let mut problems = Vec::new();
        plural_families_reach_a_shell(&fixture.root, &mut checked, &mut problems);

        // Only the skeleton's tree is left holding the gap, and it is a problem of its own.
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("__name__"), "{problems:#?}");
    }

    /// The rule, live: every `%{when}` the compiled catalog declares is filled by a
    /// `datetime` call in this repository — and the walk saw call sites to check.
    #[test]
    fn every_instant_in_a_sentence_is_filled_by_a_datetime_call() {
        let mut checked = 0;
        let mut problems = Vec::new();
        instants_are_formatted_at_the_edge(&repo::root(), &mut checked, &mut problems);
        assert!(problems.is_empty(), "{problems:#?}");
        // The template's example app contributes several of these; a product may have
        // dropped it and kept only the shells it scaffolds, so the template's exact count
        // is not a property of every tree. What must stay true everywhere is that the walk
        // found the compiled catalog's own sentence test — zero is a lost walk, not a tree
        // with fewer shells.
        assert!(
            checked >= 1,
            "{checked} call sites carry a `%{{when}}`, and something was not walked"
        );
    }

    /// The failure this invariant exists for, as a fixture: an instant handed to a
    /// sentence raw, beside the same sentence filled correctly, so the check is shown to
    /// separate the two and not merely to dislike the file.
    ///
    /// The raw call is built with `format!` rather than written out, and that is not
    /// decoration: this check walks every `.rs` file under the root it is given — which
    /// live is this repository — so a literal raw call staged here as an example would
    /// fail `cargo xtask check` exactly the way the failure itself fails it. The plural
    /// fixtures above get away with literals because their check walks only the two shell
    /// trees; this one walks everything, because an instant can reach a sentence anywhere.
    #[test]
    fn an_instant_reaching_a_sentence_unformatted_is_reported() {
        let fixture = Fixture::new("raw-instant");
        // A Crux app has a core manifest; without one the apps/*/shells tree is not checked (`has_crux_apps`).
        fixture.file("apps/probe/core/Cargo.toml", "");
        fixture.file(
            "apps/probe/shells/tui/src/main.rs",
            &format!(
                "let raw = {}!(Key::UpdatedAt, locale, when = view.updated_at_unix_ms);\n\
                 let ok = translate_with!(Key::UpdatedAt, locale, when = datetime(\
                     view.updated_at_unix_ms, offset, locale\
                 ));\n",
                "translate_with",
            ),
        );

        let mut checked = 0;
        let mut problems = Vec::new();
        instants_are_formatted_at_the_edge(&fixture.root, &mut checked, &mut problems);

        assert_eq!(checked, 2, "both call sites fill a `%{{when}}`");
        // One problem, and it names the raw line — line 1, not line 3.
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("apps/probe/shells/tui/src/main.rs:1"),
            "{problems:#?}"
        );
    }

    /// The catalog half, live: the compiled crate's `count_is` must be the check's own
    /// fixture, or the invariant is testing a catalog this repository does not ship.
    #[test]
    fn the_real_catalog_declares_count_is_with_one_placeholder() {
        let catalog = catalog_placeholders();
        let (key, placeholders) = catalog
            .get("CountIs")
            .expect("`count_is` is in the catalog this xtask compiles against");
        assert_eq!(key, "count_is");
        assert_eq!(
            placeholders.iter().map(String::as_str).collect::<Vec<_>>(),
            ["count"]
        );
        // And every other key that exists gets an entry, placeholder-free ones included:
        // an argument passed to a key with no `%{…}` is the silent-drop case.
        assert!(catalog.contains_key("Reset"));
        assert!(catalog.get("Reset").is_some_and(|(_, p)| p.is_empty()));
    }

    /// A throwaway repository root under the system temp directory.
    ///
    /// `plural_families_reach_a_shell` fails on a tree that has a family in its catalog and
    /// no call site, and this repository is not allowed to be that tree — the check that
    /// would report the fixture's defect is the one under test. Same shape as
    /// `i18n-keygen`'s fixtures: the guard is about a *shape* the real tree cannot adopt.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static COUNTER: AtomicUsize = AtomicUsize::new(0);

            // Tests share a process and run in parallel, so the pid alone is not enough to
            // keep two fixtures apart.
            let unique = format!(
                "xtask-invariants-{}-{name}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );

            let root = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&root).expect("creating the fixture root");
            Self { root }
        }

        /// Create `relative` and its parents, holding the fixture so chains read as one tree.
        fn file(&self, relative: &str, contents: &str) -> &Self {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().expect("a file has a parent"))
                .expect("creating the fixture directory");
            std::fs::write(&path, contents).expect("writing the fixture file");
            self
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// A configuration key may not end in `_file`: the suffix is reserved for indirection.
///
/// `crates/settings` reads `APP__<KEY>_FILE` as "the value is this file's contents" and removes
/// the variable before the layering. A configuration *field* whose own name ends in `_file`
/// therefore has no environment spelling that reaches it: setting the variable is read as a file
/// indirection, and the field silently stays at its file-layer value. It is cheap to refuse here
/// — the file layer is where the collision would live — and impossible to notice at runtime.
///
/// Only the workspace's own services are read: a `templates/` skeleton is not valid TOML until it
/// is rendered (`port = __port__`), and the rendered copy is covered by the archetype probes.
fn config_keys_avoid_the_reserved_file_suffix(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) {
    for service in repo::expand(root, "services/*") {
        let config = service.join("config");
        let Ok(entries) = std::fs::read_dir(&config) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        files.sort();
        for file in files {
            *checked += 1;
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            // A file that does not parse is `cldr_data_stays_sliced`'s neighbourhood and the
            // loader's problem, not this check's: reporting it here would say the wrong thing.
            let Ok(value) = toml::from_str::<toml::Value>(&text) else {
                continue;
            };
            let mut keys = Vec::new();
            toml_keys("", &value, &mut keys);
            for (dotted, key) in keys {
                if key.ends_with("_file") {
                    problems.push(format!(
                        "{}: configuration key `{dotted}` ends in `_file`, which is reserved — \
                         `crates/settings` reads `APP__…_FILE` as a file indirection and removes \
                         it before the layering, so no environment variable can set this field. \
                         Rename the key (and the option that writes it).",
                        repo::rel(root, &file)
                    ));
                }
            }
        }
    }
}

/// Every key in a TOML document, as `(dotted path, last segment)`.
fn toml_keys(prefix: &str, value: &toml::Value, out: &mut Vec<(String, String)>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, child) in table {
        let dotted = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        out.push((dotted.clone(), key.clone()));
        toml_keys(&dotted, child, out);
    }
}

#[cfg(test)]
mod reserved_suffix_tests {
    use super::*;

    /// A tree with one service's config directory, as the check reads it.
    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let config = dir.path().join("services/demo/config");
        std::fs::create_dir_all(&config).expect("create the config dir");
        for (name, contents) in files {
            std::fs::write(config.join(name), contents).expect("write a config file");
        }
        dir
    }

    fn problems(dir: &Path) -> Vec<String> {
        let mut checked = 0;
        let mut problems = Vec::new();
        config_keys_avoid_the_reserved_file_suffix(dir, &mut checked, &mut problems);
        problems
    }

    #[test]
    fn a_key_ending_in_file_is_refused() {
        // `APP__DATABASE__PASSWORD_FILE` sets `database.password`, not `database.password_file`:
        // the suffix is consumed before the layering, so this field has no environment spelling.
        let dir = tree(&[(
            "demo.toml",
            "[database]\nmax_connections = 10\npassword_file = \"/run/secret\"\n",
        )]);

        let problems = problems(dir.path());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("database.password_file"),
            "the finding names the dotted key: {}",
            problems[0]
        );
    }

    #[test]
    fn a_nested_key_ending_in_file_is_refused_too() {
        let dir = tree(&[(
            "demo.toml",
            "[telemetry]\n[telemetry.cache]\nwarm_file = 3\n",
        )]);

        let problems = problems(dir.path());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("telemetry.cache.warm_file"),
            "{}",
            problems[0]
        );
    }

    #[test]
    fn ordinary_keys_and_a_file_that_does_not_parse_are_silent() {
        let clean = tree(&[(
            "demo.toml",
            "[database]\nmax_connections = 10\n[telemetry]\nfilter = \"info\"\n",
        )]);
        assert_eq!(problems(clean.path()), Vec::<String>::new());

        // A `templates/` skeleton is not valid TOML until it is rendered, and this check must not
        // turn that into a finding about a reserved suffix.
        let unrenderable = tree(&[("demo.toml", "[application]\nport = __port__\n")]);
        assert_eq!(problems(unrenderable.path()), Vec::<String>::new());
    }
}
