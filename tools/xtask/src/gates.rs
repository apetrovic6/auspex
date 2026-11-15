//! The gate manifest (`gates.toml`) and the recipe graph that makes its claims checkable.
//!
//! A recipe named `check`, `build` or `test` is a gate. Whether one actually runs is close
//! to invisible from the files that define it, because the edges are bash loops — `_each`
//! walks `$(just modules)`, `apps/<app>/check` walks `rust_modules` — rather than `just`
//! dependencies, and a recipe nothing reaches reads exactly like a recipe everything
//! reaches. Five gates were running nowhere when this was written up (ADR 0022 lists them,
//! and `gates.toml` says where each one runs now).
//!
//! So the repository keeps a manifest: every gate-shaped recipe, where it runs, and a
//! reason where it does not run in both places. This module builds the graph and fails when
//! manifest and graph disagree in either direction — a gate no entry claims, an entry that
//! matches nothing, a job that does not run what its entry says it runs, a CI invocation
//! nobody has thought about yet.
//!
//! The completeness assertion runs twice, from both ends. `require_gates_claimed` asks what
//! the *Justfiles* contain and demands an entry for each gate-shaped recipe, by name.
//! `nix_ci_reach` asks what *CI* runs and demands an entry for each recipe a workflow invokes,
//! whatever it is called. They overlap and neither covers the other: a recipe added to a
//! Justfile is caught by the first alone, a step added to a workflow by the second alone, and
//! only both together make "every gate runs where the manifest says" mean what it says about a
//! pipeline that runs things no Justfile gate is named after.
//!
//! ## How CI is read
//!
//! Out of the two workflow files, and in two kinds. A `devenv shell -- just <recipe>` step is
//! an invocation like any other and goes through the same lexer. A `devenv build
//! outputs.ci-<stage>` step invokes nothing at all — the stage runs cargo, or buf, or trunk —
//! so `DERIVATION_GATES` is what connects it to the recipes it stands in for, and a stage with
//! no entry there is a gate nobody runs. ADR 0022 records the manifest; [ADR 0024](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0024-the-pipeline-is-derivations-and-a-shell.md) records why
//! one kind of step is a derivation.
//!
//! ## Why the graph is parsed rather than asked for
//!
//! `just --dump --dump-format json` was rejected: the interesting edges are shell text
//! inside `[script]` recipes, which the dump does not interpret either, so it buys a 50 KB
//! document and a `serde_json` dependency and still leaves every real edge to be parsed.
//! Reading headers and bodies directly is what `just_module_paths_resolve` already does,
//! keeps `cargo xtask check` free of subprocesses, and lets the shapes the resolver
//! understands be listed and audited — which is the point of the exercise.
//!
//! ## The edges the resolver understands
//!
//! Every shape a `just` invocation in this repository actually has:
//!
//! - `check: rust-check (_each "check")` — a recipe dependency, with its arguments, which
//!   become the callee's parameters. That is how `_each "check"` and `_each "build"` are
//!   one recipe with two behaviours.
//! - `just proto::check` — a module recipe, via the calling file's `mod` table.
//! - `just platforms/android/build` — a recipe in another directory, relative to the file.
//! - `just "$m/check"` where `$m` is bound by `for m in <source>` — one edge per expansion
//!   of `<source>`, which is an assignment (`rust_modules := "core client …"`), a glob loop
//!   in another script recipe (`$(just modules)`, `$(just _platforms)`) or literal words.
//! - `just "apps/$app/platforms/android/check"` — the same, with a variable the resolver
//!   cannot resolve (it comes from `basename`), which becomes a `*` segment.
//! - `just --justfile '{{ source_file() }}' _generate-to …` — a module calling into itself.
//!
//! An invocation of a shape that is not on that list either resolves to nothing and is
//! reported, or is seen and misread — so the shapes are listed here, and adding one is a
//! deliberate change in one place rather than a coverage hole.
//!
//! Statements beginning with `echo`, `printf` or `fail` are instructions, not calls: a gate
//! that fails tells you how to fix it, and the fix is usually another recipe
//! printed as text rather than run, which is what `fail` and most `echo` lines are.
//!
//! ## The one edge deliberately not followed
//!
//! Nothing past `platforms/apple`. Xcode is macOS only, so neither `just ci` on Linux nor
//! any CI container can run what lies beyond it, and following those edges would make the
//! answer depend on the machine that happened to run the check. `apps/<app>/Justfile`'s
//! `_platforms` is where that skip is declared, and `platform_skip_still_declared` asserts
//! it still says so, which is the only honest way to hold a constant like this one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::repo;

/// The recipe names that are gates.
const GATE_NAMES: [&str; 3] = ["check", "build", "test"];

/// The root recipe that means "the whole local gate", for a developer and for CI alike.
const LOCAL_SEED: &str = "ci";

/// The manifest, at the repository root with the other single-source-of-truth files.
const MANIFEST_PATH: &str = "gates.toml";

/// The CI workflows. Every `just` line and every derivation stage in them is a claim about a
/// gate, and [`nix_ci_reach`] is what makes that sentence checkable.
///
/// Both files are read, and a third would have to be added here: a workflow nobody parses is a
/// hole in the manifest that reads exactly like coverage, which is the failure this whole module
/// exists to make impossible.
const CI_WORKFLOWS: &[&str] = &[
    ".forgejo/workflows/ci.yaml",
    ".forgejo/workflows/advisories.yaml",
];

/// Which derivation stage covers which recipes.
///
/// A stage runs cargo (or buf, or trunk, or datagen) directly and invokes no `just` recipe, so
/// nothing in the graph connects it to the gates it stands in for. This table is that
/// connection, and it is the reason a derivation counts as CI coverage at all: without an entry
/// here, the gate it replaces reads as running nowhere and the manifest fails.
///
/// The recipes named are the leaves the stage reproduces, not the aggregates it replaces.
/// `ci-clippy` covers `rust::clippy`, which is what `just check` dispatches to per crate; it
/// does not claim to run `check` itself, and the manifest says so rather than pretending the
/// shapes match.
///
/// An empty list means the stage corresponds to no recipe — `ci-xtask-check` runs
/// `cargo xtask check`, which no `just` recipe wraps.
const DERIVATION_GATES: &[(&str, &[&str])] = &[
    // The aggregate. `ci-pure` is one derivation the workflow builds with one step; it
    // depends on every pure stage, so it stands in for all of their recipes at once.
    // (`ci-trunk-build` and `ci-xtask-check` were empty entries before the aggregate, so
    // they contribute nothing here.)
    (
        "ci-pure",
        &[
            "rust::fmt-check",
            "rust::clippy",
            "rust::build",
            "rust::test",
            "proto::check",
            "i18n::check-data",
            "check-archetypes",
            "typos",
        ],
    ),
    // Empty because the coverage claim comes from the workflow's loop instead, which runs
    // `just "apps/$app/generate-android"` for every app — the derivation's tree where it built
    // one, the recipe where it did not. `apps/*/generate-android` is therefore claimed by a
    // step that really does reach every app, rather than by a stage that reaches one.
    ("ci-android-generated", &[]),
];

/// [`DERIVATION_GATES`], minus the template-only stages a product's pipeline does not have.
///
/// `check-archetypes` is a template-only stage (stripped from a product's `nix/ci.nix` along
/// with its recipe and its gate entry), so a product's `ci-pure` must not claim it: the mapping
/// is checked against the recipe graph, and in a product that recipe is not there.
fn derivation_gates(is_product: bool) -> Vec<(&'static str, Vec<&'static str>)> {
    DERIVATION_GATES
        .iter()
        .map(|(stage, gates)| {
            let gates = gates
                .iter()
                .copied()
                .filter(|gate| !(is_product && *stage == "ci-pure" && *gate == "check-archetypes"))
                .collect();
            (*stage, gates)
        })
        .collect()
}

/// Statements whose first word is one of these are instructions rather than calls.
const PROSE: [&str; 3] = ["echo", "printf", "fail"];

/// A segment pair no edge is followed through, on any host. See the module documentation.
const MAC_ONLY: [&str; 2] = ["platforms", "apple"];

/// How many times one invocation target may be expanded before giving up on it.
const EXPANSION_ROUNDS: usize = 8;

/// A `just` dependency: `(_each "check")` is `_each` with one argument.
///
/// Public in this module's existing style (a private module, `pub` items — see
/// `check`) since RT-18: `crate::skeletons` consumes the parse output to compare each
/// example Justfile against its skeleton, and it compares exactly these parts.
#[derive(PartialEq, Eq)]
pub struct Dep {
    pub name: String,
    pub args: Vec<String>,
}

pub struct Recipe {
    pub name: String,
    pub params: Vec<String>,
    pub deps: Vec<Dep>,
    pub body: Vec<String>,
    /// `[unix]`, `[no-cd]`, `[script('bash')]` — the attribute lines above the header.
    ///
    /// Only the operating-system ones matter to the graph: `just` allows one recipe name to
    /// be defined several times, each gated to a host, and `just/lib.just` does exactly that
    /// for the Android Studio launcher. Without an OS attribute to tell them apart, two
    /// recipes with the same name are a genuine collision, and every coverage claim about
    /// that id would be ambiguous.
    pub attrs: Vec<String>,
}

/// Attributes `just` uses to gate a recipe to a host, and therefore to allow a second
/// recipe of the same name beside it.
const HOST_ATTRIBUTES: [&str; 4] = ["unix", "windows", "macos", "linux"];

impl Recipe {
    fn host_gated(&self) -> bool {
        self.attrs
            .iter()
            .any(|attr| HOST_ATTRIBUTES.contains(&attr.as_str()))
    }
}

pub struct JustFile {
    /// Where the file sits relative to the repository root; empty for the root `Justfile`.
    dir: String,
    /// Its path from the root, for diagnostics and for matching `mod` declarations.
    pub display: String,
    /// `mod` declarations: name to the module file's path from the repository root.
    pub mods: Vec<(String, String)>,
    /// `name := "value"`, which some loop sources are made of.
    pub assignments: BTreeMap<String, String>,
    pub recipes: Vec<Recipe>,
}

impl JustFile {
    fn recipe(&self, name: &str) -> Option<&Recipe> {
        self.recipes.iter().find(|recipe| recipe.name == name)
    }
}

struct Node {
    file: usize,
    name: String,
    is_gate: bool,
}

/// One `[[gate]]` entry of `gates.toml`.
struct Entry {
    /// Recipe id patterns, e.g. `apps/*/core/check` or `proto::check`.
    path: Vec<String>,
    /// Claimed to run under `just ci`.
    local: bool,
    /// Claimed to run in these CI jobs, by `pub async fn` name.
    ci: Vec<String>,
    /// Required unless the gate runs in both places.
    reason: Option<String>,
    /// Do not follow edges out of what this entry matches, because the work happens in
    /// another workspace: `just ci` inside `check-template` is the generated product's gate.
    terminal: bool,
}

/// Every recipe of every `Justfile` and `just/*.just` module, addressed the way it is
/// typed at a prompt: `check`, `apps/<app>/core/check`, `proto::check`.
struct Graph {
    root: PathBuf,
    files: Vec<JustFile>,
    nodes: Vec<Node>,
    /// Recipe id of `nodes[index]`, in the same order.
    ids: Vec<String>,
    /// Id of each recipe, for lookup.
    index: BTreeMap<String, usize>,
    /// Index of each file by its path, for resolving `mod` declarations.
    by_path: BTreeMap<String, usize>,
}

impl Graph {
    fn id(&self, index: usize) -> &str {
        &self.ids[index]
    }

    /// The indices a pattern matches. A pattern segment matches exactly, or, when it is
    /// `*`, against any single segment.
    fn matching(&self, pattern: &str) -> Vec<usize> {
        if !pattern.contains('*') {
            return self.index.get(pattern).copied().into_iter().collect();
        }
        let wanted: Vec<&str> = pattern.split('/').collect();
        self.ids
            .iter()
            .enumerate()
            .filter(|(_, id)| {
                let parts: Vec<&str> = id.split('/').collect();
                parts.len() == wanted.len()
                    && parts
                        .iter()
                        .zip(wanted.iter())
                        .all(|pair| *pair.1 == "*" || pair.0 == pair.1)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Resolve one invocation target — already substituted, quotes already gone — to
    /// recipes.
    ///
    /// A target that resolves to nothing is reported unless it contains a wildcard. A
    /// wildcard may legitimately match nothing: `apps/*/platforms/android/check` matches no
    /// recipe in a product whose apps have no Android project, and the app Justfile's
    /// `generate-android` guards that case with a `test -f` that says how to add one.
    /// A literal that resolves to nothing is never legitimate, and a
    /// smaller graph than the truth always is.
    fn resolve(
        &self,
        from: usize,
        target: &str,
        justfile: Option<&str>,
        problems: &mut Vec<String>,
    ) -> Vec<usize> {
        let target = target.trim().trim_matches(['\'', '"']).to_owned();
        if target.is_empty() {
            return Vec::new();
        }
        let where_from = || format!("{}: `just {target}`", self.files[from].display);

        // `--justfile <path>` says which file the recipe belongs to, which is how a module
        // reaches its own private recipe. `{{ source_file() }}` says the file it was invoked
        // from, which is this one.
        if let Some(path) = justfile {
            let host = if path.contains("source_file") {
                from
            } else {
                let wanted = normalize(&join_dir(&self.files[from].dir, path));
                let Some(host) = self.by_path.get(&wanted).copied() else {
                    problems.push(format!(
                        "{} — `--justfile {path}` names no just file the graph knows about",
                        where_from(),
                    ));
                    return Vec::new();
                };
                host
            };
            let id = format!("{}{target}", self.prefix_of(host));
            return self.lookup(&id);
        }

        if let Some((module, name)) = target.split_once("::") {
            return self.resolve_module(from, module, name, problems);
        }

        // A path, relative to the calling file's directory.
        let parts: Vec<&str> = target.split('/').collect();
        let Some((name, dirs)) = parts.split_last() else {
            return Vec::new();
        };
        let mut dir = self.files[from].dir.clone();
        for part in dirs {
            dir = join_dir(&dir, part);
        }
        let id = if dir.is_empty() {
            (*name).to_string()
        } else {
            format!("{dir}/{name}")
        };
        self.lookup(&id)
    }

    /// A `module::recipe` target resolves through the module the calling file declares. When
    /// it is not declared, the file that is named after the module is used as well, since
    /// that is the convention, and the missing declaration is reported separately by
    /// `just_module_paths_resolve` rather than guessed at differently here.
    fn resolve_module(
        &self,
        from: usize,
        module: &str,
        name: &str,
        problems: &mut Vec<String>,
    ) -> Vec<usize> {
        let declared = self.files[from]
            .mods
            .iter()
            .find(|(declared, _)| declared == module)
            .map(|(_, path)| path.clone());
        let found = declared
            .as_ref()
            .and_then(|path| self.by_path.get(path))
            .copied();
        let Some(module_file) = found else {
            problems.push(format!(
                "{}: no module `{module}` is declared here, so `{module}::{name}` cannot be \
                 resolved from this file",
                self.files[from].display,
            ));
            // The convention still says which file is meant, so follow it, and let the missing
            // declaration be the thing that is reported.
            return self
                .files
                .iter()
                .enumerate()
                .find(|(_, file)| {
                    Path::new(&file.display)
                        .file_stem()
                        .is_some_and(|stem| stem == module)
                })
                .map_or_else(Vec::new, |(index, _)| {
                    self.lookup(&format!("{}{name}", self.prefix_of(index)))
                });
        };
        self.lookup(&format!("{}{name}", self.prefix_of(module_file)))
    }

    /// Platform projects are added on demand — `just add platform-<platform> <app>` — so an
    /// app's `android` and `apple` recipes may name a project that is not there, and that is the
    /// shape the app Justfile the templates ship has: `just platforms/apple/build` is written
    /// into every product, whether or not that product will ever have an Apple project. A product
    /// with the Android project and not the Apple one therefore contains an invocation pointing
    /// at nothing, which is the template's shape rather than a gate that lost its runner: there
    /// is no Apple project there, so there are no Apple gates to claim. Completeness is still
    /// asserted over every platform project that *is* present, and a platform directory added
    /// under a name nobody has an entry for is still a failure.
    ///
    /// Settled in RT-13 rather than left inherited. That ticket decided the recipes stay typeable
    /// and fail with a message naming `just add platform-<platform> <app>` — over the
    /// alternative of the recipe disappearing with its project, which teaches nothing and stops
    /// `just --list` advertising the platform you are about to add. The invocation into the absent
    /// project therefore survives, guarded at run time in `generate-<platform>` rather than at
    /// parse time here, and this exemption survives with it. Both directions of the case are now
    /// asserted: `just check-template` gives one scaffolded app Android only and another Apple
    /// only, fails the absent platform's recipe, and runs `cargo xtask check` in that copy — this
    /// function included — with an Apple project no Android one.
    ///
    /// Recognised by walking the invocation's own path and asking whether the first segment that
    /// does not exist sits directly under a `platforms` directory.
    fn reaches_absent_platform_project(&self, file: usize, target: &str) -> bool {
        let segments: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
        let mut path = self.root.join(&self.files[file].dir);
        for (index, segment) in segments.iter().enumerate() {
            path = path.join(segment);
            if !path.exists() {
                return index > 0 && segments[index - 1] == "platforms";
            }
        }
        false
    }

    /// Lookup without judgement: whether a miss is a problem, and what to say about it, is
    /// the caller's, since a dependency, a shell call and a CI job fail in different ways.
    fn lookup(&self, id: &str) -> Vec<usize> {
        self.matching(id)
    }

    /// Empty for the root `Justfile`, the file's own directory with a trailing slash for any
    /// other, and the module name with `::` for a `just/*.just` module.
    fn prefix_of(&self, file: usize) -> String {
        let file = &self.files[file];
        let path = Path::new(&file.display);
        if path.extension().is_some_and(|ext| ext == "just") {
            let stem = path
                .file_stem()
                .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
            format!("{stem}::")
        } else if file.dir.is_empty() {
            String::new()
        } else {
            format!("{}/", file.dir)
        }
    }

    /// Every edge out of a recipe, with the parameters bound for its callee.
    ///
    /// Body invocations get an empty environment: this repository passes parameters to
    /// recipes through dependencies (`(_each "check")`), never through a shell call whose
    /// arguments would have to be matched positionally against them.
    fn edges_of(
        &self,
        index: usize,
        env: &BTreeMap<String, String>,
        problems: &mut Vec<String>,
    ) -> Vec<(usize, BTreeMap<String, String>)> {
        let file = self.nodes[index].file;
        let Some(recipe) = self.files[file].recipe(&self.nodes[index].name) else {
            return Vec::new();
        };
        let mut out: Vec<(usize, BTreeMap<String, String>)> = Vec::new();
        for dep in &recipe.deps {
            let targets = self.resolve(file, &dep.name, None, problems);
            if targets.is_empty() {
                problems.push(format!(
                    "{}: `{}` depends on `{}`, which is not a recipe, so `just` itself refuses \
                     to run it",
                    self.files[file].display, recipe.name, dep.name,
                ));
            }
            for target in targets {
                let mut bound = BTreeMap::new();
                let params = self.files[self.nodes[target].file]
                    .recipe(&self.nodes[target].name)
                    .map(|callee| callee.params.clone())
                    .unwrap_or_default();
                for (param, arg) in params.iter().zip(&dep.args) {
                    bound.insert(param.clone(), arg.clone());
                }
                out.push((target, bound));
            }
        }
        out.extend(
            self.body_edges(file, recipe, env, problems)
                .into_iter()
                .map(|target| (target, BTreeMap::new())),
        );
        let crossed = crosses_mac_only(self.id(index));
        out.retain(|(target, _)| crossed || !crosses_mac_only(self.id(*target)));
        out.sort_by_key(|(target, bound)| {
            (
                *target,
                bound
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<Vec<_>>(),
            )
        });
        out.dedup();
        out
    }

    fn body_edges(
        &self,
        file: usize,
        recipe: &Recipe,
        env: &BTreeMap<String, String>,
        problems: &mut Vec<String>,
    ) -> Vec<usize> {
        let mut out = Vec::new();
        // A loop variable lives from its `for` to its `done`, which is all the scoping these
        // bodies need: every loop in the repository is a few lines long.
        let mut loops: Vec<(String, Vec<String>)> = Vec::new();

        for line in &recipe.body {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            for statement in statements(trimmed) {
                let words = tokenize(&statement);
                let Some(first) = words.first() else {
                    continue;
                };
                if first == "for" {
                    if let Some(rest) = statement.strip_prefix("for").map(str::trim) {
                        bind_loop(file, self, rest, &mut loops, problems);
                    }
                    continue;
                }
                if first == "done" {
                    loops.pop();
                    continue;
                }
                if PROSE.contains(&first.as_str()) {
                    continue;
                }
                for call in invocations(&words) {
                    for candidate in substitute(&call.recipe, &loops, env) {
                        let found =
                            self.resolve(file, &candidate, call.justfile.as_deref(), problems);
                        if found.is_empty()
                            && !candidate.contains('*')
                            && !self.reaches_absent_platform_project(file, &candidate)
                        {
                            problems.push(format!(
                                "{}: the `{}` recipe runs `just {candidate}`, which resolves to \
                                 no recipe, so the gate graph cannot say what it covers",
                                self.files[file].display, recipe.name,
                            ));
                        }
                        out.extend(found);
                    }
                }
            }
        }
        out
    }
}

fn crosses_mac_only(id: &str) -> bool {
    let parts: Vec<&str> = id.split('/').collect();
    parts
        .windows(2)
        .any(|pair| pair[0] == MAC_ONLY[0] && pair[1] == MAC_ONLY[1])
}

fn join_dir(dir: &str, part: &str) -> String {
    if part == ".." {
        let parent = Path::new(dir)
            .parent()
            .map_or(String::new(), |parent| parent.display().to_string());
        return if parent == "." { String::new() } else { parent };
    }
    if part == "." || part.is_empty() {
        return dir.to_string();
    }
    if dir.is_empty() {
        (*part).to_string()
    } else {
        format!("{dir}/{part}")
    }
}

// --------------------------------------------------------------------------------------
// Reading `just` files
// --------------------------------------------------------------------------------------

/// Read one `Justfile` into the shape the graph is built from.
///
/// Public since RT-18, and the second consumer is `crate::skeletons`: a parser with
/// one caller is a parser nobody has tested against a second shape, and comparing an
/// example against its skeleton is that second shape — same files, different question.
/// Note what `body` keeps: every indented line of a recipe, *including its comment lines*
/// and the blank lines between statements. The graph ignores bodies, so that never
/// mattered there; `skeletons` strips comments before comparing, because the measurement
/// the RT-18 exception list is built on excludes them.
pub fn parse(root: &Path, path: &Path) -> Result<JustFile> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", repo::rel(root, path)))?;
    let parent = path.parent().unwrap_or(root);
    let dir = if parent == root {
        String::new()
    } else {
        repo::rel(root, parent)
    };

    let mut file = JustFile {
        dir,
        display: repo::rel(root, path),
        mods: Vec::new(),
        assignments: BTreeMap::new(),
        recipes: Vec::new(),
    };
    let mut pending: Vec<String> = Vec::new();

    let lines: Vec<&str> = source.lines().collect();
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        at += 1;
        // Only column 0 is structure: a recipe body is indented, by construction, and an
        // attribute line or a comment carries nothing the graph needs.
        if line.is_empty() || line.starts_with(char::is_whitespace) {
            continue;
        }
        if let Some(attr) = line
            .strip_prefix('[')
            .and_then(|rest| rest.split(['(', ']']).next())
        {
            pending.push(attr.trim().to_string());
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("mod ") {
            if let Some((name, target)) = parse_mod(&file.dir, rest) {
                file.mods.push((name, target));
            }
            continue;
        }
        if line.starts_with("export ") || line.starts_with("set ") {
            continue;
        }
        if let Some((name, value)) = parse_assignment(line) {
            file.assignments.insert(name, value);
            continue;
        }
        let Some((name, params, deps)) = parse_header(line) else {
            continue;
        };
        let mut body: Vec<String> = Vec::new();
        while at < lines.len()
            && (lines[at].is_empty() || lines[at].starts_with(char::is_whitespace))
        {
            body.push(lines[at].to_string());
            at += 1;
        }
        file.recipes.push(Recipe {
            name,
            params,
            deps,
            body,
            attrs: std::mem::take(&mut pending),
        });
    }
    Ok(file)
}

fn parse_mod(dir: &str, rest: &str) -> Option<(String, String)> {
    let name = rest.split_whitespace().next()?.to_string();
    let start = rest.find('\'')?;
    let end = rest[start + 1..].find('\'')?;
    let target = normalize(&join_dir(dir, &rest[start + 1..start + 1 + end]));
    Some((name, target))
}

fn parse_assignment(line: &str) -> Option<(String, String)> {
    for marker in ["?=", "+=", ":="] {
        if let Some((name, value)) = line.split_once(marker) {
            let name = name.trim().to_string();
            if !is_recipe_name(&name) {
                continue;
            }
            let value = value.trim();
            let value = value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .or_else(|| value.strip_prefix('"').and_then(|v| v.strip_suffix('"')))
                .unwrap_or(value)
                .to_string();
            return Some((name, value));
        }
    }
    None
}

/// `check: rust-check (_each "check")`, `breaking base="master":`, `test pkg *args:`.
fn parse_header(line: &str) -> Option<(String, Vec<String>, Vec<Dep>)> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (at, c) in line.char_indices() {
        match quote {
            Some(open) => {
                if c == open {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' | '{' => depth += 1,
                ')' | '}' => depth = depth.saturating_sub(1),
                ':' if depth == 0 && !line[at + 1..].starts_with(['=', ':']) => {
                    let (name, params) = parse_head(&line[..at])?;
                    return Some((name, params, parse_deps(&line[at + 1..])));
                }
                ':' => return None,
                _ => {}
            },
        }
    }
    None
}

fn parse_head(head: &str) -> Option<(String, Vec<String>)> {
    let mut words = head.split_whitespace();
    let name = words.next()?;
    if !is_recipe_name(name) {
        return None;
    }
    let params = words
        .map(|word| {
            word.trim_start_matches(['*', '+'])
                .split(['=', '?'])
                .next()
                .unwrap_or(word)
                .to_string()
        })
        .collect();
    Some((name.to_string(), params))
}

fn is_recipe_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c == '-' || c.is_ascii_alphanumeric())
}

fn parse_deps(rest: &str) -> Vec<Dep> {
    tokenize(rest)
        .into_iter()
        .filter_map(|token| {
            let inner = token
                .strip_prefix('(')
                .and_then(|inner| inner.strip_suffix(')'))
                .unwrap_or(&token);
            let mut words = tokenize(inner).into_iter();
            let name = words.next()?.trim_matches(['\'', '"']).to_string();
            if !is_recipe_name(&name) {
                return None;
            }
            Some(Dep {
                name,
                args: words
                    .map(|arg| arg.trim_matches(['\'', '"']).to_string())
                    .collect(),
            })
        })
        .collect()
}

/// Resolve `.` and `..` lexically, so a `mod` path out of a template still reads correctly.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

// --------------------------------------------------------------------------------------
// Shell-ish lexing
// --------------------------------------------------------------------------------------

/// Split at `;` outside quotes, interpolation and `$( … )`.
///
/// A CI job's whole shell script arrives as one Rust string spanning a dozen source lines,
/// so a statement-shaped rule ("skip a statement that begins with `echo`") has to know what
/// a statement is when there are no newlines in it either.
fn statements(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(open) = quote {
            current.push(c);
            if c == open {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                current.push(c);
            }
            '$' if chars.peek() == Some(&'(') => {
                current.push('$');
                current.push(chars.next().unwrap_or('('));
                depth += 1;
            }
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ';' if depth == 0 => {
                if !current.trim().is_empty() {
                    out.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// Split into tokens, keeping quoted runs, `$( … )` and `{{ … }}` whole.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                let open = c;
                current.push(c);
                for next in chars.by_ref() {
                    current.push(next);
                    if next == open {
                        break;
                    }
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                current.push('$');
                current.push(chars.next().unwrap_or('('));
                let mut depth = 1usize;
                for next in chars.by_ref() {
                    current.push(next);
                    match next {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            '{' if chars.peek() == Some(&'{') => {
                current.push('{');
                current.push(chars.next().unwrap_or('{'));
                // Interpolation ends at `}}`. The closing pair is recognised by remembering the
                // previous character rather than peeking at the next, so the cursor is not
                // borrowed twice in one loop.
                let mut previous = '\0';
                for next in chars.by_ref() {
                    current.push(next);
                    if next == '}' && previous == '}' {
                        break;
                    }
                    previous = next;
                }
            }
            // A parenthesised group is one token. Without this, `check: rust-check
            // (_each "check")` lexes as `rust-check`, `(_each`, `"check")`, the dependency
            // is dropped for not looking like a recipe name, and every recipe `_each` is the
            // only route to — which is most of the repository.
            '(' => {
                current.push('(');
                let mut depth = 1usize;
                for next in chars.by_ref() {
                    current.push(next);
                    match next {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// One `just …` invocation, with the `--justfile` path when the caller forced one.
///
/// The flag matters because it is how a module runs a recipe of its own that it does not
/// want to be reachable as `i18n::generate-to`: `just/lib.just` and `just/i18n.just` do it.
/// Without the path, that edge resolves against the module's namespace and reports a recipe
/// that does not exist.
struct Call {
    justfile: Option<String>,
    recipe: String,
}

/// The calls to `just` in one statement, in order.
fn invocations(words: &[String]) -> Vec<Call> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < words.len() {
        let word = words[at].as_str();
        let called = matches!(word, "just" | "@just") || word.ends_with("/just");
        if called {
            let mut next = at + 1;
            let mut justfile = None;
            while next < words.len() {
                let candidate = words[next].trim_matches(['\'', '"']);
                if let Some(flag) = candidate.strip_prefix("--") {
                    // `--justfile <path>` takes a value, and neither is the recipe.
                    if flag == "justfile" {
                        justfile = words
                            .get(next + 1)
                            .map(|path| path.trim_matches(['\'', '"']).to_string());
                        next += 1;
                    }
                    next += 1;
                    continue;
                }
                out.push(Call {
                    justfile,
                    recipe: candidate.to_string(),
                });
                break;
            }
            at = next;
        }
        at += 1;
    }
    out
}

/// Expand `{{ interpolation }}` and `$variables` in an invocation target.
///
/// A parameter the caller supplied (`(_each "check")`) becomes its argument; a variable
/// bound by an enclosing `for` becomes one candidate per expansion; anything else becomes
/// `*`. That is imprecise in the one direction that is safe: a wildcard can over-reach, and
/// it can never quietly drop coverage, because every recipe the graph reaches is still
/// required to be claimed.
fn substitute(
    target: &str,
    loops: &[(String, Vec<String>)],
    env: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut current = vec![interpolate(target, env)];
    // A variable the resolver cannot resolve is still an edge, just an imprecise one.
    let wildcard = ["*".to_string()];
    for _ in 0..EXPANSION_ROUNDS {
        let mut next: Vec<String> = Vec::new();
        let mut expanded = false;
        for candidate in &current {
            let Some(at) = candidate.find('$') else {
                next.push(candidate.clone());
                continue;
            };
            expanded = true;
            let (before, rest) = candidate.split_at(at);
            let (name, consumed) = variable(rest);
            let tail = &rest[consumed..];
            let expansions = loops
                .iter()
                .rev()
                .find(|(bound, _)| *bound == name)
                .map_or(wildcard.as_slice(), |(_, values)| values.as_slice());
            for value in expansions {
                next.push(format!("{before}{value}{tail}"));
            }
        }
        current = next;
        if !expanded {
            break;
        }
    }
    current.retain(|candidate| !candidate.is_empty());
    current
}

/// The variable name at the head of a shell fragment, and how many bytes of the fragment it
/// occupies (`$m`, `${m}`, `$m/check` → `("m", 2)`).
fn variable(text: &str) -> (String, usize) {
    let body = text.strip_prefix('$').unwrap_or(text);
    if let Some(rest) = body.strip_prefix('{') {
        let name: String = rest.chars().take_while(|c| *c != '}').collect();
        let consumed = 1 + 1 + name.len() + 1;
        return (name, consumed);
    }
    let name: String = body
        .chars()
        .take_while(|c| *c == '_' || c.is_ascii_alphanumeric())
        .collect();
    let consumed = 1 + name.len();
    (name, consumed)
}

/// Replace `{{ name }}` with what the caller passed for it, and any other interpolation with
/// `*` — an interpolated assignment or builtin cannot be known without running `just`.
fn interpolate(target: &str, env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = target;
    while let Some(at) = rest.find("{{") {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 2..];
        let Some(end) = tail.find("}}") else {
            out.push_str(tail);
            return out;
        };
        let key = tail[..end].trim();
        out.push_str(env.get(key).map_or("*", String::as_str));
        rest = &tail[end + 2..];
    }
    out.push_str(rest);
    out
}

/// What a script recipe prints, which is how `$(just modules)` and `$(just _platforms)`
/// become edges rather than a subprocess.
///
/// Both are one glob loop: `modules` walks `apps/*/Justfile services/*/Justfile`, `_platforms`
/// walks `platforms/*/Justfile`. Reading the glob out of the recipe is chosen over running
/// it on purpose: running it makes the check depend on the host — `_platforms` answers
/// differently on a Mac — and running `just` from inside `cargo xtask check` for any reason
/// at all is the slippery slope the module documentation refuses to start down.
fn recipe_output(graph: &Graph, file: usize, name: &str) -> Vec<String> {
    let Some(recipe) = graph.files[file].recipe(name) else {
        return Vec::new();
    };
    let dir = &graph.files[file].dir;
    let mut out = Vec::new();
    let dirname = recipe
        .body
        .iter()
        .any(|line| line.contains("dirname \"") || line.contains("dirname '"));
    for line in &recipe.body {
        for statement in statements(line.trim()) {
            for token in tokenize(&statement) {
                let token = token.trim_matches(['\'', '"']).to_string();
                if !token.contains('*') {
                    continue;
                }
                let pattern = if dir.is_empty() {
                    token
                } else {
                    format!("{dir}/{token}")
                };
                for found in repo::expand(&graph.root, &pattern) {
                    let mut path = found.as_path();
                    if dirname {
                        path = path.parent().unwrap_or(path);
                    }
                    let relative = repo::rel(&graph.root, path);
                    let relative = match dir {
                        empty if empty.is_empty() => relative,
                        dir => relative
                            .strip_prefix(&format!("{dir}/"))
                            .unwrap_or(&relative)
                            .to_string(),
                    };
                    out.push(relative);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Bind one `for VAR in <sources>` loop, having resolved each source.
fn bind_loop(
    file: usize,
    graph: &Graph,
    rest: &str,
    loops: &mut Vec<(String, Vec<String>)>,
    problems: &mut Vec<String>,
) {
    let words = tokenize(rest);
    let Some(var) = words.first().map(String::to_owned) else {
        return;
    };
    let values = words
        .iter()
        .position(|word| word == "in")
        .map_or_else(Vec::new, |at| words[at + 1..].to_vec());
    let mut expansions: Vec<String> = Vec::new();
    for source in &values {
        let source = source.trim_matches(['\'', '"']);
        if source.is_empty() || source == "do" || source.starts_with(';') {
            continue;
        }
        if let Some(inner) = source
            .strip_prefix("$(")
            .and_then(|inner| inner.strip_suffix(')'))
        {
            let printed = inner
                .split_whitespace()
                .skip_while(|word| *word != "just")
                .nth(1)
                .unwrap_or_default();
            let found = recipe_output(graph, file, printed);
            if found.is_empty() {
                problems.push(format!(
                    "{}: a loop `for {var} in {source}` whose contents xtask cannot see. The \
                     gate graph stops here, so nothing further is known to run. Teach \
                     `gates.rs` the shape, or record what it covers in {MANIFEST_PATH}.",
                    graph.files[file].display
                ));
            }
            expansions.extend(found);
            continue;
        }
        if let Some(key) = source
            .strip_prefix("{{")
            .and_then(|inner| inner.strip_suffix("}}"))
        {
            match graph.files[file].assignments.get(key.trim()) {
                Some(list) => expansions.extend(list.split_whitespace().map(String::from)),
                None => expansions.push("*".to_string()),
            }
            continue;
        }
        expansions.push(source.to_string());
    }
    expansions.sort();
    expansions.dedup();
    loops.push((var, expansions));
}

// --------------------------------------------------------------------------------------
// The manifest
// --------------------------------------------------------------------------------------

fn load_manifest(root: &Path) -> Result<Vec<Entry>> {
    let text = std::fs::read_to_string(root.join(MANIFEST_PATH))
        .with_context(|| format!("reading {MANIFEST_PATH}"))?;
    let parsed: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {MANIFEST_PATH}"))?;
    let Some(gates) = parsed.get("gate").and_then(toml::Value::as_array) else {
        bail!("{MANIFEST_PATH} has no [[gate]] entries");
    };
    let mut out = Vec::new();
    for gate in gates {
        let list = |key: &str| {
            gate.get(key)
                .and_then(toml::Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(String::from))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default()
        };
        let path = list("path");
        if path.is_empty() {
            bail!("{MANIFEST_PATH}: a [[gate]] entry needs a non-empty `path` list");
        }
        let reason = gate
            .get("reason")
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(String::from);
        out.push(Entry {
            path,
            local: gate
                .get("local")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
            ci: list("ci"),
            reason,
            terminal: gate
                .get("terminal")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
        });
    }
    Ok(out)
}

// --------------------------------------------------------------------------------------
// Building and walking the graph
// --------------------------------------------------------------------------------------

fn build(root: &Path) -> Result<Graph> {
    let mut paths: Vec<PathBuf> = Vec::new();
    repo::find_files(root, "Justfile", &mut paths);
    if let Ok(entries) = std::fs::read_dir(root.join("just")) {
        paths.extend(entries.flatten().filter_map(|entry| {
            let path = entry.path();
            path.extension()
                .is_some_and(|ext| ext == "just")
                .then_some(path)
        }));
    }
    paths.sort();
    // A template's `Justfile` is the prototype of one, and it is checked in the generated
    // product's own copy, where it has real app names and a real module list. Checking the
    // skeleton here would reason about `apps/__name__`, which is a module nowhere, and would
    // be a third assertion of the same thing after `check-template`.
    //
    // Nothing under a dot-directory either. A nested copy of this repository — a git worktree
    // under `.claude/worktrees`, a checkout under `.devenv` — is a second complete `Justfile`
    // tree whose recipe ids collide one for one with this one's, and the collision check below
    // reports that as an error rather than seeing what it is. `repo::find_files` skips the
    // directories other checks have hit before; a gate walk has no such excuse.
    paths.retain(|path| {
        let relative = repo::rel(root, path);
        !relative.starts_with("templates/")
            && !relative.split('/').any(|segment| segment.starts_with('.'))
    });

    let files: Vec<JustFile> = paths
        .iter()
        .map(|path| parse(root, path))
        .collect::<Result<Vec<JustFile>>>()?;

    let mut graph = Graph {
        root: root.to_path_buf(),
        files,
        nodes: Vec::new(),
        ids: Vec::new(),
        index: BTreeMap::new(),
        by_path: BTreeMap::new(),
    };
    for (file, just) in graph.files.iter().enumerate() {
        graph.by_path.insert(just.display.clone(), file);
    }
    let mut clashes = Vec::new();
    for (file, just) in graph.files.iter().enumerate() {
        for recipe in &just.recipes {
            let id = format!("{}{}", graph.prefix_of(file), recipe.name);
            if let Some(previous) = graph.index.get(&id) {
                let first = &graph.files[graph.nodes[*previous].file].recipes;
                let gated = first
                    .iter()
                    .any(|other| other.name == recipe.name && other.host_gated())
                    && recipe.host_gated();
                if gated {
                    // `just` picks one of them by host. The graph keeps the first and stays
                    // host-independent, which is the only answer that does not change when
                    // someone runs the check on a Mac.
                    continue;
                }
                clashes.push(format!(
                    "{id} ({} and {})",
                    graph.files[graph.nodes[*previous].file].display, just.display
                ));
            }
            graph.index.insert(id.clone(), graph.nodes.len());
            graph.ids.push(id);
            graph.nodes.push(Node {
                file,
                name: recipe.name.clone(),
                is_gate: GATE_NAMES.contains(&recipe.name.as_str()),
            });
        }
    }
    if !clashes.is_empty() {
        // Two files whose recipe ids collide make every coverage claim ambiguous.
        bail!(
            "two just files share a recipe id: {}; the id is its path, so this means a \
             module is declared under two names",
            clashes.join(", ")
        );
    }
    Ok(graph)
}

/// Follow the graph from some recipes to everything they reach.
///
/// Traversal state carries the parameters bound by the edge that arrived, since `_each
/// recipe:` is one recipe with as many behaviours as it has callers. Coverage is recorded
/// per recipe, regardless of the arguments it was reached with.
fn closure(
    graph: &Graph,
    seeds: &[usize],
    terminal: &BTreeSet<usize>,
    problems: &mut Vec<String>,
) -> BTreeSet<usize> {
    let mut reached: BTreeSet<usize> = BTreeSet::new();
    let mut visited: BTreeSet<(usize, Vec<(String, String)>)> = BTreeSet::new();
    let mut queue: Vec<(usize, BTreeMap<String, String>)> = seeds
        .iter()
        .map(|index| (*index, BTreeMap::new()))
        .collect();

    while let Some((index, env)) = queue.pop() {
        reached.insert(index);
        let key = (
            index,
            env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        );
        if !visited.insert(key) || terminal.contains(&index) {
            continue;
        }
        queue.extend(graph.edges_of(index, &env, problems));
    }
    reached
}

/// `_platforms` is where the macOS-only platform skip is declared, and `MAC_ONLY` is an
/// assertion about it. If the declaration ever stops saying it, the constant and the
/// repository disagree, and silently.
fn platform_skip_still_declared(
    root: &Path,
    graph: &Graph,
    checked: &mut usize,
    problems: &mut Vec<String>,
) {
    let mut found = false;
    for file in &graph.files {
        let Some(recipe) = file.recipe("_platforms") else {
            continue;
        };
        found = true;
        *checked += 1;
        let declares = |needle: &str| recipe.body.iter().any(|line| line.contains(needle));
        if !declares(MAC_ONLY[1]) || !declares("macos") {
            problems.push(format!(
                "{}: `_platforms` no longer names both `{}` and `macos`, so the skip that \
                 `gates.rs` assumes about Apple projects is no longer declared there. Either \
                 the rule changed, in which case update `MAC_ONLY`, or the recipe lost a \
                 guard it still needs.",
                file.display, MAC_ONLY[1],
            ));
        }
    }
    // A Crux app's Justfile declares `_platforms`, so a product with no Crux app (`new --drop-example`)
    // has none to declare. Keyed on `has_crux_apps`: with a Crux app present, a missing declaration is
    // the defect it always was.
    if !found && crate::repo::has_crux_apps(root) {
        problems.push(
            "no `_platforms` recipe anywhere: `gates.rs` assumes platform projects are \
             discovered by one. Say otherwise there, or restore it."
                .to_string(),
        );
    }
}

// --------------------------------------------------------------------------------------
// The Nix workflow, read from YAML
// --------------------------------------------------------------------------------------

/// The `just` recipe invocations and derivation stages in the Nix workflow.
///
/// `just_recipes`: every `devenv shell -- just <recipe>` line, keyed by the job name.
/// `derivations`: every `outputs.ci-<stage>` name from `devenv build` lines, under the job
/// that builds it. Per job rather than per file because the manifest's `ci` list names jobs,
/// and a stage moved from one job to another is a change in where a gate runs.
struct NixWorkflow {
    just_recipes: BTreeMap<String, Vec<String>>,
    derivations: BTreeMap<String, BTreeSet<String>>,
}

impl NixWorkflow {
    const fn empty() -> Self {
        Self {
            just_recipes: BTreeMap::new(),
            derivations: BTreeMap::new(),
        }
    }

    /// Every job this workflow runs, whether it has recipe lines, derivation stages, or both.
    fn jobs(&self) -> BTreeSet<&str> {
        self.just_recipes
            .keys()
            .chain(self.derivations.keys())
            .map(String::as_str)
            .collect()
    }

    /// Every derivation stage named anywhere in the file.
    fn all_derivations(&self) -> BTreeSet<&str> {
        self.derivations
            .values()
            .flatten()
            .map(String::as_str)
            .collect()
    }
}

/// One job header of a Nix workflow: `  ci-pure:` → `ci-pure`.
///
/// Job headers are two-space-indented bare keys; a job's own attributes sit at the same depth,
/// so `steps` is excluded by name and anything with a value (`runs-on: nix`) is excluded by
/// shape. Reading the indentation rather than a list of job names is what lets a workflow gain
/// or rename a job without an edit here.
fn job_header(line: &str) -> Option<String> {
    let name = line.strip_prefix("  ")?.trim_end_matches(':');
    if name.is_empty() || name.contains(char::is_whitespace) || name == "steps" {
        return None;
    }
    is_recipe_name(name).then(|| name.to_string())
}

/// Parse one Nix workflow into the claims its steps make.
///
/// A step is one of two shapes, and both count as CI coverage:
///
///   `run: devenv build outputs.ci-<stage>`  — a derivation stage
///   `run: devenv shell -- just <recipe>`    — a direct `just` invocation
///
/// The recipe extraction goes through the same [`tokenize`] and [`invocations`] the Justfile
/// bodies are read with, rather than a split on `"just "`: the nightly runs `just deny --fix`
/// and `just check-template yes`, and a split would keep the argument as part of the recipe
/// name, which then resolves to nothing and reports a renamed recipe that was never renamed.
/// Taking the first word after `just` is what `invocations` already does, and it is also what
/// makes `just deny` and `just deny --fix` the same edge — one entry in the manifest, not two.
fn nix_workflow(root: &Path, workflow_path: &str, problems: &mut Vec<String>) -> NixWorkflow {
    let path = root.join(workflow_path);
    let Ok(source) = std::fs::read_to_string(&path) else {
        problems.push(format!(
            "{workflow_path}: unreadable, so the Nix pipeline cannot be checked against \
             {MANIFEST_PATH}"
        ));
        return NixWorkflow::empty();
    };

    let mut just_recipes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut derivations: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut current_job: Option<String> = None;

    let lines: Vec<&str> = source.lines().collect();
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        at += 1;
        let trimmed = line.trim();

        // Track which job the steps below belong to.
        if let Some(job) = job_header(line) {
            current_job = Some(job);
        }

        // Only a step's command is a claim. A comment in this file's own voice — the nightly's
        // header says "a whole `just ci` run inside a second workspace" — is prose about a recipe,
        // not an invocation of one, and reading it as one would claim coverage that does not exist.
        if trimmed.starts_with('#') {
            continue;
        }

        let Some(value) = trimmed.strip_prefix("run:").map(str::trim) else {
            continue;
        };
        let Some(ref job) = current_job else {
            continue;
        };

        // The step's whole script. Either it is on the `run:` line, or the line ends in a YAML
        // block indicator and the script is the run of more-indented lines below it:
        //
        //   run: devenv build outputs.ci-fmt
        //   run: |
        //     set -euo pipefail
        //     GEN="$(devenv build outputs.ci-x | grep …)"
        //
        // Both shapes are in use, and reading only the first silently claims that the second
        // step runs nothing — which is what the derivation-stage check caught when the Android
        // step grew a block scalar to extract a store path.
        let indent = line.len() - line.trim_start().len();
        let mut script = String::new();
        if value.is_empty() || value.starts_with('|') || value.starts_with('>') {
            while at < lines.len() {
                let next = lines[at];
                // A blank line is part of the block; anything at or before the `run:` line's
                // own indentation ends it.
                if next.trim().is_empty() {
                    at += 1;
                    continue;
                }
                if next.len() - next.trim_start().len() <= indent {
                    break;
                }
                at += 1;
                // A `#` inside the block is a shell comment, not a command.
                if next.trim().starts_with('#') {
                    continue;
                }
                script.push_str(next.trim());
                script.push('\n');
            }
        } else {
            script.push_str(value);
        }

        for script_line in script.lines() {
            // `devenv build outputs.ci-<stage>` — strip `outputs.` only,
            // keeping `ci-<stage>` to match DERIVATION_GATES keys.
            for part in script_line.split_whitespace() {
                if let Some(stage) = part.strip_prefix("outputs.") {
                    derivations
                        .entry(job.clone())
                        .or_default()
                        .insert(stage.trim_matches(['\'', '"']).to_string());
                }
            }

            for statement in statements(script_line) {
                for call in invocations(&tokenize(&statement)) {
                    just_recipes
                        .entry(job.clone())
                        .or_default()
                        .push(call.recipe.trim_matches(['\'', '"']).to_string());
                }
            }
        }
    }

    for recipes in just_recipes.values_mut() {
        recipes.sort();
        recipes.dedup();
    }

    NixWorkflow {
        just_recipes,
        derivations,
    }
}

/// What each CI job reaches, read out of the workflows, and the claims that reading makes
/// checkable.
///
/// The workflows are the only CI entry point. A job's coverage is the closure of two kinds of
/// seed:
///
/// - every `devenv shell -- just <recipe>` line it runs, which is a recipe invocation in the
///   same sense a Justfile body's is, and
/// - the recipes each derivation stage it builds stands in for, from [`DERIVATION_GATES`].
///
/// The second kind is what makes a derivation a claim rather than an opaque build. `ci-clippy`
/// runs cargo directly and invokes no recipe, so without the mapping nothing would connect it to
/// `rust::clippy` and the gate that recipe anchors would read as running nowhere.
///
/// Three findings come out of the same walk, and all three are the manifest's promise — a gate
/// nobody runs must fail the check:
///
/// - a derivation stage no step builds, which is a gate that lost its runner;
/// - a `just` line naming a recipe that does not exist, which is a step that has been broken
///   since the rename;
/// - a recipe CI invokes that no entry claims, which is a gate running untracked.
///
/// Deleting any single step fails, and for the reason it was deleted: a derivation step is
/// caught by the first finding, a host step by the closure mismatch in [`assert_claim`] (the
/// entry's `ci` list still names the job, and the job no longer reaches the recipe).
///
/// The argument list is the closure's inputs plus the two reporting accumulators; `is_product`
/// only changes which derivation stages exist (see [`derivation_gates`]).
#[allow(clippy::too_many_arguments)]
fn nix_ci_reach(
    graph: &Graph,
    workflows: &[(&str, NixWorkflow)],
    root_file: usize,
    terminal: &BTreeSet<usize>,
    claimed: &BTreeMap<usize, Vec<usize>>,
    is_product: bool,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> BTreeMap<String, BTreeSet<usize>> {
    let built: BTreeSet<&str> = workflows
        .iter()
        .flat_map(|(_, wf)| wf.all_derivations())
        .collect();

    // Every stage the mapping knows about must be built by some step, somewhere.
    for (stage, gates) in derivation_gates(is_product) {
        *checked += 1;
        if !built.contains(stage) {
            problems.push(format!(
                "no workflow builds the derivation stage `outputs.{stage}`, which covers {}. \
                 A stage nobody builds is a gate nobody runs — add a `devenv build \
                 outputs.{stage}` step, or drop the stage from DERIVATION_GATES if it is gone \
                 for good.",
                if gates.is_empty() {
                    "no `just` recipe (it runs cargo directly)".to_string()
                } else {
                    format!("`just {}`", gates.join(", `just "))
                },
            ));
        }
    }

    // Every gate a stage claims to cover must still be a recipe, and must be claimed by an
    // entry — so a rename shows up as a stale mapping rather than as a silently smaller
    // closure, and a deleted entry shows up as a gate CI runs that nobody has thought about.
    //
    // The second half is the only thing demanding an entry for these. `require_gates_claimed`
    // asks for entries by *recipe name*, and a stage's recipes are mostly not named
    // `check`/`build`/`test` — `typos`, `rust::clippy`, `proto::check`, `generate-android` —
    // so without this a manifest entry for any of them could be deleted and nothing would
    // notice, which is the exact hole the fifth direction of ADR 0022 was added to close.
    // Measured: deleting the `typos` entry passed the check before this was restored.
    for (stage, gates) in derivation_gates(is_product) {
        for gate in &gates {
            *checked += 1;
            let found = graph.matching(gate);
            if found.is_empty() {
                problems.push(format!(
                    "DERIVATION_GATES says `outputs.{stage}` covers `just {gate}`, which is not \
                     a recipe — it has been renamed, or the mapping is stale, and either way the \
                     stage now claims coverage of nothing"
                ));
                continue;
            }
            for index in &found {
                // A gate-shaped one is `require_gates_claimed`'s, which demands an entry for
                // it by name and words the finding better: one defect, one message. Skipped
                // before the count as well as before the report, so the number this direction
                // contributes is the number of claims only it makes.
                if graph.nodes[*index].is_gate {
                    continue;
                }
                *checked += 1;
                if !claimed.contains_key(index) {
                    problems.push(format!(
                        "`outputs.{stage}` covers `just {gate}`, which no entry in {MANIFEST_PATH} \
                         claims. A derivation stage is a CI invocation like any other: add the \
                         entry — `path`, `local`, and `ci` naming whichever job builds the stage \
                         — and the `reason` unless it runs locally too."
                    ));
                }
            }
        }
    }

    let mut job_reach: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (path, workflow) in workflows {
        for job in workflow.jobs() {
            let mut seeds: Vec<usize> = Vec::new();

            for recipe in workflow.just_recipes.get(job).into_iter().flatten() {
                // Through `substitute`, which is what turns a shell variable the resolver
                // cannot bind into a `*` segment. The Android steps loop over discovered apps
                // and invoke `just "apps/$app/platforms/android/check"`, and `$app` comes from
                // a `basename`-style expansion no parser can follow — so it becomes
                // `apps/*/platforms/android/check`, which is the same wildcard the manifest
                // spells its patterns in, and matches every app rather than none. Resolving
                // the raw string instead finds no recipe at all, which reads as a rename.
                for candidate in substitute(recipe, &[], &BTreeMap::new()) {
                    *checked += 1;
                    let found = graph.resolve(root_file, &candidate, None, problems);
                    if found.is_empty() && !candidate.contains('*') {
                        problems.push(format!(
                            "{path}: the {job} job runs `just {candidate}`, which is not a recipe \
                             — that step fails, and has done since the recipe was renamed"
                        ));
                        continue;
                    }
                    for index in &found {
                        if !claimed.contains_key(index) && !graph.nodes[*index].is_gate {
                            problems.push(format!(
                                "{path}: the {job} job runs `just {candidate}`, which no entry in \
                                 {MANIFEST_PATH} claims. Every invocation in a CI workflow is a claim \
                                 about a gate: add the entry — `path`, `local`, `ci` including \
                                 `{job}` — and the `reason` unless it runs in both places."
                            ));
                        }
                    }
                    seeds.extend(found);
                }
            }

            // The stages this job builds, through the recipes they stand in for.
            for stage in workflow.derivations.get(job).into_iter().flatten() {
                for (_, gates) in derivation_gates(is_product)
                    .iter()
                    .filter(|(name, _)| *name == stage)
                {
                    for gate in gates {
                        seeds.extend(graph.matching(gate));
                    }
                }
            }

            seeds.sort_unstable();
            seeds.dedup();
            job_reach.insert(job.to_string(), closure(graph, &seeds, terminal, problems));
        }
    }
    job_reach
}

// --------------------------------------------------------------------------------------
// The invariant
// --------------------------------------------------------------------------------------

/// The invariant: `gates.toml` says where every gate runs, and what it says is true.
///
/// Five directions, all of them needed:
///
/// - a gate-shaped recipe no entry claims, which is the original failure — a check that
///   exists and runs nowhere;
/// - an entry that matches no recipe, which is a claim about something that is not there;
/// - a claim about `just ci` or about a CI job that the graph disagrees with, in either
///   direction, which is how a manifest rots into a comment;
/// - a gate that runs in only one place — locally, or in CI, or neither — without a `reason`,
///   since an unexplained exclusion is the same hole wearing a tie;
/// - a recipe a CI job invokes directly that no entry claims, which is the completeness of
///   the first direction seen from the other end: not "every gate-shaped recipe has been
///   thought about" but "everything CI runs has been thought about".
pub fn check(root: &Path, checked: &mut usize, problems: &mut Vec<String>) -> Result<()> {
    let graph = build(root)?;
    let entries = load_manifest(root)?;
    platform_skip_still_declared(root, &graph, checked, problems);

    let per_entry = match_entries(&graph, &entries, checked, problems);
    let claimed = owners(&per_entry);
    require_gates_claimed(&graph, &claimed, checked, problems);

    let Some(root_file) = graph
        .files
        .iter()
        .position(|file| file.display == "Justfile")
    else {
        problems.push("no root `Justfile` to read the module list from".to_string());
        return Ok(());
    };
    let Some(local_root) = graph.index.get(LOCAL_SEED).copied() else {
        problems.push(format!(
            "the root `Justfile` has no {LOCAL_SEED} recipe, so there is no local gate for \
             {MANIFEST_PATH} to be checked against"
        ));
        return Ok(());
    };
    let mut terminal: BTreeSet<usize> = BTreeSet::new();
    for (entry, found) in entries.iter().zip(&per_entry) {
        if entry.terminal {
            terminal.extend(found.iter().copied());
        }
    }

    // The workflows are the only CI entry point. Each job's coverage is the closure of the
    // recipes it invokes plus the recipes its derivation stages stand in for; the seeds are
    // resolved inside `nix_ci_reach`, which reports an unclaimed invocation and an unbuilt
    // stage as findings of their own rather than folding them silently into the closure.
    let parsed: Vec<(&str, NixWorkflow)> = CI_WORKFLOWS
        .iter()
        .map(|path| (*path, nix_workflow(root, path, problems)))
        .collect();
    let job_reach = nix_ci_reach(
        &graph,
        &parsed,
        root_file,
        &terminal,
        &claimed,
        repo::is_product(root),
        checked,
        problems,
    );
    let local = closure(&graph, &[local_root], &terminal, problems);

    // And now the claims themselves.
    for (entry, found) in entries.iter().zip(&per_entry) {
        if found.is_empty() {
            continue;
        }
        *checked += 1;
        for index in found {
            assert_claim(&graph, entry, *index, &local, &job_reach, problems);
        }
        let runs_everywhere = entry.local && !entry.ci.is_empty();
        if entry.reason.is_none() && !runs_everywhere {
            let where_it_runs = match (entry.local, entry.ci.is_empty()) {
                (false, false) => "in CI only",
                (true, true) => "locally only",
                _ => "nowhere",
            };
            problems.push(format!(
                "{MANIFEST_PATH}: {} run {where_it_runs} and the entry carries no `reason`. An \
                 exclusion nobody explained is a gate that went missing politely.",
                list(
                    &found
                        .iter()
                        .map(|index| graph.id(*index).to_owned())
                        .collect::<BTreeSet<String>>()
                ),
            ));
        }
    }
    Ok(())
}

/// Which recipes each entry matches, reporting the patterns that match nothing and the
/// recipes more than one entry claims.
fn match_entries(
    graph: &Graph,
    entries: &[Entry],
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Vec<Vec<usize>> {
    let mut per_entry: Vec<Vec<usize>> = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut found = Vec::new();
        for pattern in &entry.path {
            let matched = graph.matching(pattern);
            *checked += 1;
            if matched.is_empty() {
                problems.push(format!(
                    "{MANIFEST_PATH}: an entry lists {pattern:?}, which matches no recipe. It \
                     is stale, or the gate has been renamed — either way the entry now \
                     asserts nothing."
                ));
                continue;
            }
            found.extend(matched);
        }
        per_entry.push(found);
    }
    per_entry
}

/// Recipe index -> the entries that claim it.
fn owners(per_entry: &[Vec<usize>]) -> BTreeMap<usize, Vec<usize>> {
    let mut claimed: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (entry, found) in per_entry.iter().enumerate() {
        for index in found {
            claimed.entry(*index).or_default().push(entry);
        }
    }
    claimed
}

fn require_gates_claimed(
    graph: &Graph,
    claimed: &BTreeMap<usize, Vec<usize>>,
    checked: &mut usize,
    problems: &mut Vec<String>,
) {
    for (index, owners) in claimed {
        if owners.len() > 1 {
            problems.push(format!(
                "{}: claimed by {} entries of {MANIFEST_PATH}, which may disagree about where \
                 it runs. One entry claims one recipe.",
                graph.id(*index),
                owners.len()
            ));
        }
    }
    for index in 0..graph.nodes.len() {
        if !graph.nodes[index].is_gate {
            continue;
        }
        *checked += 1;
        if claimed.contains_key(&index) {
            continue;
        }
        problems.push(format!(
            "{} is a gate — named {} in {} — that no entry in {MANIFEST_PATH} claims, so \
             nothing we can see runs it. Reach it from `just {LOCAL_SEED}` or a CI job, or \
             record in {MANIFEST_PATH} where it runs and why.",
            graph.id(index),
            graph.nodes[index].name,
            graph.files[graph.nodes[index].file].display,
        ));
    }
}

/// One entry, one recipe it claims, against what the two closures actually reach.
fn assert_claim(
    graph: &Graph,
    entry: &Entry,
    index: usize,
    local: &BTreeSet<usize>,
    job_reach: &BTreeMap<String, BTreeSet<usize>>,
    problems: &mut Vec<String>,
) {
    let id = graph.id(index);
    if entry.local != local.contains(&index) {
        problems.push(format!(
            "{MANIFEST_PATH}: the entry for {id} says `just {LOCAL_SEED}` {} it, and the graph \
             says {}.",
            if entry.local { "runs" } else { "does not run" },
            if local.contains(&index) {
                "it does — cut the edge, or record the gate as running"
            } else {
                "it does not — reach it from there, or set `local = false` and say where it \
                 runs instead"
            },
        ));
    }
    let wants: BTreeSet<&str> = entry.ci.iter().map(String::as_str).collect();
    let actual: BTreeSet<&str> = job_reach
        .iter()
        .filter(|(_, reached)| reached.contains(&index))
        .map(|(job, _)| job.as_str())
        .collect();
    if actual != wants {
        // Bound and named rather than inlined, because the two `difference` calls read alike
        // at a call site and were the wrong way round from the day this was written: a job
        // that ran an unclaimed recipe was reported as "claimed and not running", which sends
        // the reader to look for a job that stopped running something when none has. The
        // message fires only when manifest and pipeline already disagree and nobody yet knows
        // which side is wrong, so it is the last place to be approximate (RT-15).
        //
        // `actual` is what the graph says runs the recipe; `wants` is what the entry claims.
        let running_unclaimed: BTreeSet<&str> = actual.difference(&wants).copied().collect();
        let claimed_not_running: BTreeSet<&str> = wants.difference(&actual).copied().collect();
        // Only the half that applies. One of the two is empty in every mismatch but the rare
        // double one, and advice attached to "nothing" is noise in the line a reader is
        // trying to act on.
        let mut detail = Vec::new();
        if !running_unclaimed.is_empty() {
            detail.push(format!(
                " Running and unclaimed: {} — add to the entry's `ci`.",
                list(&running_unclaimed)
            ));
        }
        if !claimed_not_running.is_empty() {
            detail.push(format!(
                " Claimed and not running: {} — drop it from `ci`, or reach the recipe from \
                 that job.",
                list(&claimed_not_running)
            ));
        }
        problems.push(format!(
            "{MANIFEST_PATH}: the entry for {id} claims {} and the graph says {}.{}",
            list(&wants),
            list(&actual),
            detail.concat(),
        ));
    }
}

fn list<T: std::fmt::Display>(items: &BTreeSet<T>) -> String {
    if items.is_empty() {
        return "nothing".to_string();
    }
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
