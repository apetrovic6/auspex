//! `cargo xtask deny` — the supply-chain gate, and the line a failing advisories run is missing.
//!
//! `just deny` runs this, the `ci-impure` job runs `just deny` on every push, and the nightly
//! runs `just deny --fix`, so the triage reaches a laptop and a notification from one
//! implementation and nothing carries a second copy of it.
//!
//! ## What the failure is missing
//!
//! An advisories failure has two answers and the failure itself does not say which one it wants.
//! On this repository the two arrived two days apart, and telling them apart meant running the
//! gate again on a laptop and reading advisory prose first: three `deny.toml` `ignore` entries
//! (6c91474, RUSTSEC-2026-0370/0371/0372, the zbus trio — `ashpd` pins `zbus = "^5.13"` and
//! nothing in the tree can take zbus 6), and seven changed lines of `Cargo.lock` from one
//! `cargo update` (6bf65dc, RUSTSEC-2026-0381/0382, webpki-roots and rustls-webpki).
//!
//! RT-37 made the finding reach the notification at all. This is the other half: what the finding
//! is, in the two terms that decide what to do about it. It reports the categories cargo-deny
//! already distinguishes — `fields.code` is `vulnerability`, `unmaintained`, or `yanked` — and for
//! each finding whether cargo-deny's own solution note names something to move to. It stops there.
//! The one exception is a yanked crate: it carries no solution note to stop at, so the report names
//! the `cargo update` this module derives from the inclusion graph and says the bump will not run
//! it.
//! Which kind gets an `ignore` entry is `deny.toml`'s policy, written above that file's `ignore`
//! list, and a triage step that decided it would be a second parse of one rule — the drift RT-18
//! exists to stop, and the reason RT-25's shared `SENTINELS` are one list instead of two.
//!
//! ## Why two cargo-deny runs rather than one JSON run re-rendered
//!
//! The obvious shape is one `--format json` run and a rendering of it that replaces cargo-deny's
//! own output. That was rejected: cargo-deny's human diagnostics carry the `Cargo.lock` line span
//! of the offending lock entry (`Cargo.lock:845:1`, and the inclusion graph under it), which is
//! where a fix actually gets applied, and its JSON carries the same span only as a `labels`
//! object nobody is going to read as text. Re-rendering means writing that renderer, and the
//! result is a gate that says less than the one it replaced. So pass one is cargo-deny's own
//! output and its own exit status, streamed unchanged, and the JSON pass runs only on failure
//! and only adds a verdict underneath.
//!
//! ## What the JSON actually is
//!
//! Measured against cargo-deny 0.20.2 on this tree, not taken from its documentation.
//!
//! - Newline-delimited JSON on **stderr**; stdout stays empty. One object per diagnostic, then a
//!   trailing `{"type":"summary"}` with per-check counts.
//! - Honoured `ignore` entries are **not** diagnostics, and cargo-deny's human output does not
//!   print them either: master today prints `advisories ok` and nothing else, while the summary
//!   reports `notes: 22` behind 11 `ignore` entries. A note is therefore not an ignore entry, and
//!   neither number is assumed here — per-finding detail comes from the diagnostics, totals from
//!   the summary.
//! - A diagnostic with no `fields.advisory` is not an advisory, but neither is it about the gate's
//!   own config. Run against a `deny.toml` holding a key cargo-deny does not recognise, the JSON is
//!   one `unexpected-keys` diagnostic, a `log` line, and no summary: the advisories check never
//!   ran. Those are counted and named as what they are rather than triaged, and the report about
//!   them does not point at `deny.toml`'s ignore rule, because that question was never reached. A
//!   `yanked` diagnostic names no advisory either — a withdrawal from crates.io is not a RUSTSEC
//!   entry, so there is nothing to carry — which is why the missing field cannot be the test, and
//!   why `triage` splits on `fields.code` instead.
//! - `fields.code` is the kind, and **every advisory finding** carries a `Solution:` note — the
//!   three unmaintained ones as much as the two vulnerabilities. Whether a fix is reachable is the
//!   *content* of that note, so kind and reachability are two independent axes, and a stuck
//!   vulnerability is a case that has to render as itself rather than collapse into either
//!   category. A yanked diagnostic carries no `notes` field at all, measured on the fixture, so it
//!   is the one finding with no note to read and `Fix::NoNote` is its shape rather than an
//!   unexpected one.
//! - There is no structured patched-version field. `fields.advisory` has id, package, title, url
//!   and withdrawn, and no `patched`; the advisory database's `[versions] patched = [">= 1.0.10"]`
//!   never reaches the JSON at all. The upgrade target exists only as prose behind that prefix.
//! - The prefix is cargo-deny's own string and not RUSTSEC's, which is the only thing that makes
//!   "driven by JSON rather than by matching human output" honest here: the advisory file behind
//!   RUSTSEC-2026-0370 has no `solution` key at all, and "Solution: ", "Upgrade to " and "No safe
//!   upgrade is available!" are format strings compiled into the 0.20.2 binary this shell puts on
//!   PATH. It is a weaker handle than a field, and saying so is part of why it is keyed on the
//!   prefix rather than on a substring of the advisory's description.
//!
//! ## The flag
//!
//! `--offline`, not the `--disable-fetch` the ticket named: 0.20.2 has no such flag. It is also a
//! `cargo-deny` option rather than a `check` option, so it has to precede the subcommand — after
//! `check` it is rejected outright with "unexpected argument '--offline' found". It keeps the
//! second pass off the network and makes it read the advisory database the first pass just
//! fetched, which is what stops the two runs disagreeing; measured to report identical counts on
//! both a green tree and a red one.
//!
//! ## `--fix`, and why the bump lives here rather than in the pipeline
//!
//! When the verdict above is the same every time — every finding a vulnerability, every one of
//! them naming something to move to — the answer is a `cargo update` per crate, and RT-41 asked
//! for the gate to produce it rather than describe it. The question that shapes it is *where*: the
//! fold is one line over `Verdict`, and three of the four ways to get it to a caller are worse
//! than the fourth.
//!
//! - **A distinguished exit code** (3 for "bumpable") was rejected: RT-37 is this repository
//!   already paying for a status code carrying meaning nobody could see, and `supply_chain`'s own
//!   doc comment leans on `deny` exits 1 / `typos` exits 2 as a thing that had to be *measured*.
//! - **Parsing the verdict line back out of the job's log** is the defect this module exists to
//!   avoid, one level up: RT-41's own criterion is "driven by cargo-deny's JSON rather than by
//!   matching its human output", and regexing *this* module's prose is the same mistake.
//! - **Giving `xtask` a lib target** so a pipeline driver could `use xtask::deny::triage` is
//!   typed and parse-free, and it makes the thing that runs the gate depend on repository tooling
//!   as a library, for nothing the next option does not give. (There is no driver any more —
//!   [ADR 0024](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0024-the-pipeline-is-derivations-and-a-shell.md) replaced it with derivations and `devenv shell -- just` — and the rejection stands
//!   anyway: the workflows invoke this subcommand, which is the option below.)
//! - **So the subcommand does the whole job.** `cargo xtask deny --fix` runs the gate, folds the
//!   verdict, runs the updates, re-runs the gate and leaves the result on disk. The predicate
//!   stays adjacent to the `triage` that defines it — one place, which is RT-18's lesson — and the
//!   whole decision is testable against the committed fixtures instead of only inside a container.
//!
//! Default off, and silent on a green tree exactly as before: `--fix` cannot act on a run that
//! never reached the second pass.
//!
//! ### What `--fix` reports, and the omission it would otherwise lie by
//!
//! `cargo update -p <crate>` moves more than the crate it names. Measured on the tokio bump
//! (fca5131): cargo re-resolves `windows-sys` whenever the lock is touched, and the committed lock
//! was in a state cargo will not reproduce, so seven edge changes appeared that were not tokio's
//! doing. They are Windows-only and so outside `deny.toml`'s `[graph] targets`, which is why the
//! gate stayed green — but a report that says "moved tokio 1.53.1 → 1.53.3" while the diff moves
//! seven other things is lying by omission to the person who has to review it. So the report is
//! the shape of the whole lockfile diff, with the advisories' own crates named inside it.
//!
//! The upgrade command is derived from the inclusion graph's structured fields —
//! `Finding::package` and `Finding::version`, as `name@version` — never from `Fix::Named`'s prose.
//! The prose is cargo-deny's own format string with a version range in it and no contract behind
//! it; `--precise` fed from it was tried on the tokio bump and produced an identical-size diff, so
//! it buys nothing for the parse it would cost. The version is in the spec rather than the bare
//! name being enough because the bare name does not run against this lockfile at all: two majors
//! of one crate is the normal case here, and `specs` records what cargo says about it.
//!
//! ### The same trap one level down: an advisory whose crate an earlier update already moved
//!
//! The list of updates is derived from the findings, and the findings are independent of each other
//! in a way the lockfile is not. This repository's own four zstd advisories sit on one chain —
//! `zstd` -> `zstd-safe` -> `zstd-sys` — and running the list one `cargo update` per spec means the
//! first of them, `-p zstd@0.13.3`, re-resolves `zstd-safe` to 7.4.0 on its way through. The next
//! spec on the list names a locked version the lock no longer holds, and cargo answers:
//!
//! ```text
//! error: package ID specification `zstd-safe@7.3.0` did not match any packages
//! help: there are similar package ID specifications: zstd-safe@7.4.0
//! ```
//!
//! so the run reverted a lockfile it had already improved, on the one tree it exists for. RT-46
//! filed it after finding it by fixing the nightly by hand (PR #59), and its option 1 is what
//! `bump` does: **one `cargo update` carrying every `-p` spec**, so all of them are resolved
//! against the lockfile as the verdict described it, before anything moves. Measured here, on the
//! real four: that one call moves exactly the three named crates and nothing else, six lines.
//!
//! Rejected, with the reasoning kept: re-read the lock between updates and skip a spec whose version
//! has already moved (RT-46's option 2, and the shape this file had for an hour). It works — it is
//! what made the four zstd advisories green — but it is more states for the reader and more code,
//! and it decides "already handled" by absence rather than letting cargo resolve a chain the way it
//! resolves chains. Bare names (option 3) were rejected by `specs` above.
//!
//! What a spec that cannot move does: measured with `zstd-sys = "=2.1.0"` added to a crate, the same
//! single call exits 0 having moved the other two and left the pinned one where it was. The re-run
//! of the gate is what decides, as it always was — the exit status of the update never was.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::repo;

/// cargo-deny's own prefix on the one note that answers "what fixes this?".
///
/// cargo-deny's string, not the advisory's prose — see the module documentation. Keying on it is
/// the weakest link in this module and the only handle there is.
const SOLUTION_PREFIX: &str = "Solution: ";

/// cargo-deny's own sentence for "the advisory names nothing to move to".
const NO_SAFE_UPGRADE: &str = "No safe upgrade is available!";

/// Where a green `--fix` leaves what it did, for the thing that turns it into a pull request.
///
/// Under `target/` because that is where build output goes and what `.gitignore` already covers:
/// a bump nobody has to remember to unstage is part of the point.
///
/// Read back by [`crate::advisory_pr`], and the default of its `--artefact` argument — one
/// spelling of the location rather than two that could drift. `pub` and not `pub(crate)`:
/// xtask is a binary, so a private module's `pub` is already crate-wide, and clippy's
/// `redundant_pub_crate` says so.
pub const ARTEFACT: &str = "target/advisory-bump.json";

/// The second pass's whole argv, spelled out because its order is load-bearing.
///
/// `deny` first, then cargo-deny's own options, then `check` and its argument: `--offline` is a
/// cargo-deny option rather than a `check` option, so after `check` it is rejected outright. And
/// the subcommand is not optional — without it this is `cargo --offline --format json …`, which
/// cargo itself rejects with "unexpected argument '--format' found" and its own usage block,
/// which is a confusing way to describe a missing subcommand. It was written that way once.
const JSON_ARGS: [&str; 6] = [
    "deny",
    "--offline",
    "--format",
    "json",
    "check",
    "advisories",
];

/// Run the gate. On success this prints nothing at all: the gate is green far more often than it
/// is red, and a gate that comments on its own success is noise that gets tuned out. `--fix` does
/// not change that — a green tree never reaches anything below the early return.
pub fn run(fix: bool) -> Result<()> {
    let root = repo::root();

    // Before anything else, so that what is on disk afterwards describes this run and not the
    // last one. See `clear_artefact`.
    if fix {
        clear_artefact(&root)?;
    }

    // Pass one: cargo-deny's own output, inherited rather than captured, and its own exit status.
    let status = gate(&root, Stdio::inherit)?;
    if status.success() {
        return Ok(());
    }
    let code = status.code().unwrap_or(1);

    // Pass two, only now, and only for advisories: the same database, offline, in JSON.
    let (ndjson, json_code) = json_pass(&root)?;
    let verdict = triage(&ndjson);

    if fix && let Some(findings) = verdict.bumpable() {
        match bump(&root, &findings)? {
            Bump::Green {
                report: bump_report,
                lockfile_changed,
            } => {
                println!("{bump_report}");
                // The verdict line is the one the gate would have printed had it stayed red, and
                // the pull request wants both halves: what was wrong, and what was done about it.
                let verdict_report = report(&verdict)
                    .context("a bump was made from a verdict with nothing in it to report")?;
                write_artefact(
                    &root,
                    &artefact(&findings, &verdict_report, &bump_report, lockfile_changed),
                )?;
                return Ok(());
            }
            // The lockfile is back to what it was, so the verdict computed before the update
            // still describes the tree. Say the attempt happened — a `--fix` that silently
            // reverted and printed the untouched report would read as a `--fix` that never
            // ran — and then print that report unchanged.
            Bump::Reverted(tried) => println!(
                "advisory bump: `{tried}` ran, and `cargo deny check` is still red — Cargo.lock restored to what it was",
            ),
        }
    }

    match report(&verdict) {
        Some(verdict) => println!("{verdict}"),
        // Nothing usable came back from the second pass. Say that, and let the failure above
        // stand on its own rather than inventing a verdict for it.
        None => println!("{}", unavailable(&ndjson, json_code)),
    }

    bail!("`cargo deny check` exited {code}");
}

/// The gate itself — cargo-deny's own four checks, its own output, its own status.
///
/// `io` is how its two streams are wired, and the two callers want opposite things: pass one
/// inherits, because those diagnostics carry the `Cargo.lock` line spans that are the whole reason
/// this module does not re-render them; the re-run after a bump discards them, because a second
/// wall of diagnostics about a lockfile that is about to be restored describes a tree that will
/// not exist by the time anybody reads it.
fn gate(root: &Path, io: fn() -> Stdio) -> Result<ExitStatus> {
    Command::new("cargo")
        .args(["deny", "check"])
        .current_dir(root)
        .stdout(io())
        .stderr(io())
        .status()
        .context("`cargo deny check` could not be run — is cargo-deny in the dev shell?")
}

/// cargo-deny's advisories check in JSON. Returns its stderr and its exit status.
///
/// The status is not the interesting part: this pass exits 1 whenever advisories have findings,
/// which is the expected outcome of a run that got here at all. What matters is whether it
/// produced a summary, and `triage` answers that.
fn json_pass(root: &Path) -> Result<(String, Option<i32>)> {
    let outcome = Command::new("cargo")
        .args(JSON_ARGS)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("`cargo deny --offline --format json check advisories` could not be run")?;
    Ok((
        String::from_utf8_lossy(&outcome.stderr).into_owned(),
        outcome.status.code(),
    ))
}

/// What the second pass said, split into the three things it actually distinguishes.
#[derive(Debug)]
pub struct Verdict {
    /// Per-finding detail, in cargo-deny's own emission order.
    findings: Vec<Finding>,
    /// Diagnostics that parsed, named no advisory, and are not a yanked crate — cargo-deny
    /// complaining about its own config, which is a gate failure the advisories check never got to
    /// take part in. Counted rather than triaged: they are not advisories, and dropping one
    /// silently turns a real failure into a report that says nothing about it.
    ///
    /// The bucket used to be defined by the missing field alone, and that is RT-47. A yanked
    /// diagnostic names no advisory either — a withdrawal from crates.io is not a RUSTSEC entry —
    /// and it arrived here labelled "most likely about its config", which sent the reader of the
    /// 2026-11-02 `async-executor` failure to `deny.toml` for something that was one `cargo update`
    /// away. `fields.code` decides the bucket now, and a real config complaint keeps its count and
    /// its wording unchanged.
    not_advisories: usize,
    /// The trailing summary's advisories counts. Absent means the pass never produced one, which
    /// is a different failure from "no findings" and must never be reported as it.
    summary: Option<Counts>,
    /// Lines that were not JSON, or JSON of a shape this does not read. Counted and reported
    /// rather than dropped: a triage that quietly loses a finding is worse than one that admits
    /// it lost one.
    unread: usize,
}

/// cargo-deny's `fields.code` for the one kind a bump may act on.
///
/// `unmaintained` is not a `cargo update` away from anything: `deny.toml`'s policy is written above
/// its `ignore` list and says so, and re-deciding it here would be the second parse of one rule
/// that RT-18 exists to stop.
///
/// `yanked` was, and this is the correction RT-47 asked for. This doc used to name `yanked`
/// alongside `unmaintained`, and `async-executor 1.14.0` disproved it: PR #61 was one `cargo
/// update` and two lines of `Cargo.lock`. The bump still does not act on a yanked crate, for a
/// reason that is not the one this doc used to give — cargo-deny names no fix target for a yanked
/// crate, no `Solution:` note and no patched-version field, so the only thing a bump could act on
/// is cargo's own resolution. Making that the predicate is a change to RT-41's, and a change of
/// that kind is decided rather than slipped in: it is RT-47's option 2, and it is a ticket of its
/// own. What lands here is the finding being seen and named, with the command the person can run —
/// `report` prints `cargo update -p <crate>@<version>` from the inclusion graph, the same spec
/// [`specs`] would have used — and the step staying theirs, with the re-run of the gate as the
/// authority on whether it worked.
const VULNERABILITY: &str = "vulnerability";

/// cargo-deny's `fields.code` for a crate crates.io has withdrawn.
///
/// A finding like any other: it names a crate and a locked version in the inclusion graph, and it
/// is the reason "carries no `advisory` field" cannot be the test for "is about `deny.toml`" — a
/// withdrawal is a crates.io fact rather than a RUSTSEC entry, so there is no advisory to carry.
/// `triage` splits on this string, and [`ADJECTIVAL_KINDS`] already spells it.
const YANKED: &str = "yanked";

impl Verdict {
    /// The findings a bump is allowed to act on, or `None` when it is not allowed to act.
    ///
    /// Seven conditions, and RT-41 named three of them: findings non-empty, every `kind` is
    /// `vulnerability`, every `fix` is `Fix::Named`. Three more are the reason this is a method
    /// on `Verdict` and not a fold over `findings` alone.
    ///
    /// The seventh is that every finding has an advisory id. The ids are what the artefact
    /// carries and what [`crate::advisory_pr`] deduplicates open pull requests on, and a bump
    /// with an id missing would write a list that names less than it bumped — with every id
    /// missing, an empty list, which `covers` reads as covered by whatever pull request happens
    /// to be open, so the nightly bumps and then opens nothing. cargo-deny names an id on every
    /// vulnerability it has emitted here; this is the guard for the run where it does not.
    ///
    /// `unread > 0` means this module **admitted it lost a line** of cargo-deny's output — that is
    /// the only thing that field is for — so the findings are a subset of the run's findings and
    /// nobody knows of what. `not_advisories > 0` means cargo-deny was complaining about its own
    /// config and the advisories check never ran, so there is no fold to be complete. `summary ==
    /// None` means the pass never got far enough to count anything, which is a different failure
    /// from "no findings" and must never be read as it.
    ///
    /// A yanked finding fails the kind test, and since RT-47 that is the whole of why a tree
    /// holding one is not bumpable — `not_advisories` no longer counts it. The outcome is the same
    /// as it was; the reason is not, and the better one is visible: the report prints the finding
    /// and says the bump acts only on vulnerabilities that name a fix, where the old refusal came
    /// with no reason in the report at all whenever a fixable vulnerability shared the run.
    ///
    /// Acting on an incomplete fold is precisely the failure this module was written to prevent:
    /// the whole point of RT-40 was a verdict that says what it does not know. A `--fix` that
    /// reads past that and runs `cargo update` anyway would spend the one credit this module has.
    /// Not `pub`: `Finding` is private, and a `pub fn` handing one out is a type more private than
    /// the item that returns it. It stays crate-visible, which inside a binary is what `pub` on
    /// `triage` already means in practice.
    #[must_use]
    fn bumpable(&self) -> Option<Vec<&Finding>> {
        if self.findings.is_empty() || self.not_advisories > 0 || self.unread > 0 {
            return None;
        }
        self.summary?;
        let unambiguous = self.findings.iter().all(|finding| {
            finding.kind == VULNERABILITY
                && matches!(finding.fix, Fix::Named(_))
                && finding.id.is_some()
        });
        unambiguous.then(|| self.findings.iter().collect())
    }
}

#[derive(Debug)]
struct Finding {
    /// `fields.code`, verbatim: `vulnerability`, `unmaintained`, `yanked`, whatever cargo-deny
    /// calls this lint. Not translated into a category of this module's own.
    kind: String,
    /// `fields.advisory.id`, when the diagnostic has an advisory to name at all.
    ///
    /// `Option` rather than a placeholder string. The placeholder this field used to fall back
    /// to — "an unnamed advisory" — prints in the report's id column *and* lands in
    /// [`artefact`]'s `advisories` list, which is the list a driver deduplicates open pull
    /// requests against. A finding with no advisory has to say that, not hand out an id nobody
    /// can look up. The finding that has none is a yanked crate: crates.io withdrew it, and there
    /// is no RUSTSEC entry to name.
    id: Option<String>,
    package: String,
    version: Option<String>,
    fix: Fix,
    /// The immediate dependents in cargo-deny's inclusion graph — who pulls it in, one hop.
    ///
    /// One hop is what the graph's first level is, and it is not always the interesting name:
    /// `zvariant` is pulled in by `zbus`, and the crate that actually pins it out of reach is
    /// `ashpd`, one level further up. The full chain is cargo-deny's own inclusion graph, printed
    /// above this line, and re-deriving it here would be re-printing it. Printed for `Fix::Stuck`
    /// only, because that is the half the ignore-entry answer needs — every `ignore` entry in
    /// `deny.toml` names a puller by hand today. A yanked diagnostic has no note to read and no
    /// ignore entry to name a puller for, so its graph goes unread here.
    pulled_in_by: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Fix {
    /// The note names something to move to, kept verbatim after the prefix —
    /// `Upgrade to >=1.0.10 (try `cargo update -p webpki-roots`)`.
    Named(String),
    /// The note is cargo-deny's `No safe upgrade is available!`. A vulnerability can be stuck,
    /// and that is the case the ticket's "a vulnerability carries a Solution line" assumption
    /// hid: the line is on all of them, and its content is the answer.
    Stuck,
    /// No solution note at all. The shape this is known for is a yanked crate, whose diagnostic
    /// carries no `notes` field at all; the arm stays a report of what is missing rather than a
    /// guess at which of the two above it was, because a cargo-deny lint this module has not seen
    /// can land here too.
    NoNote,
}

/// The summary's per-check counts, as cargo-deny spells them.
#[derive(Debug, Clone, Copy, Deserialize)]
struct Counts {
    errors: usize,
    warnings: usize,
    notes: usize,
    helps: usize,
}

impl Counts {
    /// The counts cargo-deny reports without calling them findings: its notes — where the
    /// `deny.toml` `ignore` entries it honoured land, since those never arrive as diagnostics —
    /// plus any `help` it declined to escalate.
    ///
    /// `None` when there are neither, so a report never announces a zero it found.
    fn non_findings(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.notes > 0 {
            parts.push(format!("{} notes", self.notes));
        }
        if self.helps > 0 {
            parts.push(format!("{} helps", self.helps));
        }
        (!parts.is_empty()).then(|| parts.join(" and "))
    }
}

#[derive(Deserialize)]
struct Diagnostic {
    fields: DiagnosticFields,
}

#[derive(Deserialize)]
struct DiagnosticFields {
    code: Option<String>,
    severity: Option<String>,
    advisory: Option<AdvisoryInfo>,
    notes: Option<Vec<String>>,
    graphs: Option<Vec<Graph>>,
}

#[derive(Deserialize)]
struct AdvisoryInfo {
    id: Option<String>,
    package: Option<String>,
}

/// One crate node and its dependents, as cargo-deny serialises the inclusion graph.
#[derive(Deserialize)]
struct Graph {
    /// cargo-deny's own type name, serialised verbatim. Not a typo to fix up.
    #[serde(rename = "Krate")]
    krate: Option<Node>,
    #[serde(default)]
    parents: Vec<Parent>,
}

#[derive(Deserialize)]
struct Parent {
    #[serde(rename = "Krate")]
    krate: Option<Node>,
}

#[derive(Deserialize)]
struct Node {
    name: String,
    version: Option<String>,
}

#[derive(Deserialize)]
struct Summary {
    fields: SummaryFields,
}

#[derive(Deserialize)]
struct SummaryFields {
    advisories: Option<Counts>,
}

/// The whole parse, and the whole decision about what the second pass means. Pure: nothing here
/// spawns a process, so every branch below is testable against a committed capture.
#[must_use]
pub fn triage(ndjson: &str) -> Verdict {
    let mut findings = Vec::new();
    let mut not_advisories = 0;
    let mut summary = None;
    let mut unread = 0;

    for line in ndjson
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            unread += 1;
            continue;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("diagnostic") => match serde_json::from_value::<Diagnostic>(value) {
                Ok(diagnostic) => {
                    // Two different failures used to share one shape, and the shape was the test.
                    //
                    // A diagnostic that names no advisory is cargo-deny complaining about
                    // something other than an advisory — with this pass running only
                    // `check advisories`, that means its own config, and the advisories check
                    // never ran. But a **yanked crate names no advisory either**, and it is a
                    // finding about a crate sitting in the lockfile: on 2026-11-02, when
                    // crates.io withdrew `async-executor 1.14.0`, this arm put it in the config
                    // bucket and the triage under a correct `error[yanked]` told every reader of
                    // the run to go and look at `deny.toml`.
                    //
                    // So the split is on what the diagnostic *is* — `fields.code` — and not on
                    // which field it lacks. The narrow rule, deliberately: `unexpected-keys` is
                    // the only other no-advisory diagnostic with a capture here, and "no advisory
                    // but a graph naming a crate" would guess at every cargo-deny lint this
                    // repository has not seen yet.
                    let yanked = diagnostic.fields.code.as_deref() == Some(YANKED);
                    if diagnostic.fields.advisory.is_none() && !yanked {
                        not_advisories += 1;
                    } else {
                        findings.extend(finding(diagnostic.fields));
                    }
                }
                Err(_) => unread += 1,
            },
            Some("summary") => match serde_json::from_value::<Summary>(value) {
                Ok(rest) => summary = rest.fields.advisories,
                Err(_) => unread += 1,
            },
            _ => unread += 1,
        }
    }

    Verdict {
        findings,
        not_advisories,
        summary,
        unread,
    }
}

/// One diagnostic object into one finding, or into nothing when cargo-deny did not report it as a
/// finding in the first place.
///
/// A `note` or a `help` is cargo-deny declining to treat its own diagnostic as a finding. The
/// honoured `ignore` entries never reach the JSON as diagnostics at all — they are counted only
/// in the summary — so this arm is about a lint configured down to note, and not about ignores.
fn finding(fields: DiagnosticFields) -> Option<Finding> {
    if matches!(fields.severity.as_deref(), Some("note" | "help")) {
        return None;
    }

    let graph = fields.graphs.as_deref().and_then(<[Graph]>::first);
    // The affected crate and its locked version come from the graph, which is the only place the
    // version appears; `advisory.package` is the fallback and names the crate but never the pin.
    let package = graph
        .and_then(|graph| graph.krate.as_ref())
        .map(|node| node.name.clone())
        .or_else(|| {
            fields
                .advisory
                .as_ref()
                .and_then(|advisory| advisory.package.clone())
        })
        .unwrap_or_else(|| "an unnamed crate".to_owned());
    let version = graph
        .and_then(|graph| graph.krate.as_ref())
        .and_then(|node| node.version.clone());

    let solution = fields
        .notes
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|note| note.strip_prefix(SOLUTION_PREFIX))
        .map(str::trim)
        .find(|body| !body.is_empty());
    let fix = match solution {
        Some(body) if body == NO_SAFE_UPGRADE => Fix::Stuck,
        Some(body) => Fix::Named(body.to_owned()),
        None => Fix::NoNote,
    };

    let pulled_in_by = match &fix {
        Fix::Stuck => graph.map_or_else(Vec::new, |graph| {
            let mut parents: Vec<String> = graph
                .parents
                .iter()
                .filter_map(|parent| parent.krate.as_ref())
                .map(describe)
                .collect();
            parents.sort();
            parents.dedup();
            parents
        }),
        _ => Vec::new(),
    };

    Some(Finding {
        kind: fields.code.unwrap_or_else(|| "unlabelled".to_owned()),
        id: fields
            .advisory
            .as_ref()
            .and_then(|advisory| advisory.id.clone()),
        package,
        version,
        fix,
        pulled_in_by,
    })
}

fn describe(node: &Node) -> String {
    pinned(&node.name, node.version.as_deref())
}

/// What the id column says for a finding that has no advisory to name.
///
/// The column has to print something, and `None` is not it: it reads as a parse that lost an id
/// rather than as a finding that never had one. `Finding::id` says which of the two it is.
const NO_ADVISORY: &str = "no advisory";

fn id_label(finding: &Finding) -> &str {
    finding.id.as_deref().unwrap_or(NO_ADVISORY)
}

/// `zvariant 5.15.0`, or the bare name where the inclusion graph gave no version.
fn pinned(name: &str, version: Option<&str>) -> String {
    version.map_or_else(|| name.to_owned(), |version| format!("{name} {version}"))
}

/// cargo-deny's lint names that are adjectives in English.
///
/// The tally pluralises a kind label, because "2 vulnerability" is not something a reader of a
/// failed pipeline should have to decode, and these two — the ones this gate actually emits — take
/// no -s: "3 unmaintaineds" is not a word. The label stays cargo-deny's own otherwise, matching the
/// `error[unmaintained]` header above it.
const ADJECTIVAL_KINDS: [&str; 2] = ["unmaintained", YANKED];

fn plural(kind: &str, count: usize) -> String {
    if count == 1 || ADJECTIVAL_KINDS.contains(&kind) {
        return kind.to_owned();
    }
    // The noun labels end in a consonant and `y` — `vulnerability`, `policy` — and take -ies.
    match kind.strip_suffix('y') {
        Some(stem) if !stem.ends_with(['a', 'e', 'i', 'o', 'u']) => format!("{stem}ies"),
        _ => format!("{kind}s"),
    }
}

/// The verdict as text, or `None` when the second pass produced nothing to report.
///
/// Never an empty block: every branch below says something, and the one case with nothing to say
/// returns `None` so the caller prints the triage-unavailable line instead of a header over an
/// empty list.
#[must_use]
fn report(verdict: &Verdict) -> Option<String> {
    if verdict.summary.is_none() && verdict.findings.is_empty() && verdict.not_advisories == 0 {
        return None;
    }

    let mut lines: Vec<String> = Vec::new();
    if verdict.findings.is_empty() {
        // Nothing to triage, and the reason needs saying: the gate failed, so a silence here
        // would read as a gate that failed without saying why.
        let mut clauses: Vec<String> = vec![match verdict.summary {
            Some(counts) if counts.errors + counts.warnings > 0 => format!(
                "cargo-deny's summary counted {} errors and {} warnings in advisories, but emitted no diagnostic this module could read — its output above is the only account of them",
                counts.errors, counts.warnings
            ),
            Some(counts) => format!(
                "advisories are clean ({}) — whatever failed is bans, licences or sources, and cargo-deny's output above names it",
                counts
                    .non_findings()
                    .unwrap_or_else(|| "no notes".to_owned())
            ),
            None => "the advisories check produced no summary object, which means it never ran far enough to count anything".to_owned(),
        }];
        if verdict.not_advisories > 0 {
            clauses.push(format!(
                "{} {} named no advisory — cargo-deny's own complaint, most likely about its config, and not an advisories finding",
                verdict.not_advisories,
                plural("diagnostic", verdict.not_advisories)
            ));
        }
        lines.push(format!("advisory triage: {}", clauses.join("; ")));
    } else {
        // The tally is by kind, in the order cargo-deny emitted them: the kind is what decides the
        // answer, and a run holding more than one kind has to say how many of each there are.
        let mut tally: Vec<(&str, usize)> = Vec::new();
        for finding in &verdict.findings {
            match tally.iter_mut().find(|(kind, _)| *kind == finding.kind) {
                Some(entry) => entry.1 += 1,
                None => tally.push((&finding.kind, 1)),
            }
        }
        let mut clauses: Vec<String> = vec![format!(
            "advisory triage: {}",
            tally
                .iter()
                .map(|(kind, count)| format!("{count} {}", plural(kind, *count)))
                .collect::<Vec<_>>()
                .join(", ")
        )];
        if let Some(non_findings) = verdict.summary.as_ref().and_then(Counts::non_findings) {
            clauses.push(format!(
                "{non_findings}, which is where cargo-deny counts the `deny.toml` ignore entries it honoured rather than reporting them as findings"
            ));
        }
        lines.push(clauses.join("; "));

        for finding in &verdict.findings {
            let package = pinned(&finding.package, finding.version.as_deref());
            let answer = match &finding.fix {
                Fix::Named(body) => format!("names a fix: {body}"),
                Fix::Stuck => "no fix is named".to_owned(),
                // A yanked diagnostic carries no `notes` at all — measured, the fixture is the
                // capture — so it lands in this arm. The answer is not cargo-deny's own prose
                // either, which suggests `cargo update -p async-executor`: the bare name is the
                // spec cargo refuses when a crate is in the lock twice, for the reason `specs`
                // records. It is the same spec `--fix` would have used, derived from the graph.
                Fix::NoNote if finding.kind == YANKED => {
                    format!("no fix is named; try `cargo update -p {}`", spec(finding))
                }
                Fix::NoNote => "no solution note in cargo-deny's output".to_owned(),
            };
            lines.push(format!(
                "  {:<14}  {:<18}  {:<28}  {answer}",
                finding.kind,
                id_label(finding),
                package
            ));
            if !finding.pulled_in_by.is_empty() {
                lines.push(format!(
                    "  pulled in by: {}",
                    finding.pulled_in_by.join(", ")
                ));
            }
        }
    }

    // The caveats apply to either branch. A triage that lost a line says so even when what it
    // kept was a clean summary, and a verdict with no summary behind it says whose numbers these
    // are not.
    if verdict.summary.is_none() {
        lines.push("  (no summary object in cargo-deny's output: these are the diagnostics it emitted, not its totals)".to_owned());
    }
    if verdict.unread > 0 {
        lines.push(format!(
            "  ({} line{} of cargo-deny's JSON this module could not read)",
            verdict.unread,
            if verdict.unread == 1 { "" } else { "s" }
        ));
    }
    if !verdict.findings.is_empty() {
        // The `--fix` half of a yanked finding, said where the reader of a red `--fix` run will
        // hit it: the bump refuses a tree holding one, and before RT-47 the refusal came with no
        // reason in the report at all — the config clause only prints when there are no findings,
        // so a vulnerability and a yanked crate rendered as "1 vulnerability" and moved nothing.
        if verdict
            .findings
            .iter()
            .any(|finding| finding.kind == YANKED)
        {
            lines.push(
                "  (a yanked crate is a crates.io fact rather than an advisory, and `--fix` acts only on vulnerabilities that name a fix: the `cargo update` above is the person's step, and the re-run of the gate is what decides it worked)"
                    .to_owned(),
            );
        }
        lines.push(
            "  which answer a finding needs is `deny.toml`'s rule, written above its `ignore` list, and not this report's"
                .to_owned(),
        );
    }
    Some(lines.join("\n"))
}

/// The line for a second pass that produced nothing.
///
/// Not a verdict: no findings, no "clean", no counts. What failed here is the triage, and the
/// failure that made this run happen is still the one above it.
fn unavailable(ndjson: &str, code: Option<i32>) -> String {
    let mut out = format!(
        "advisory triage: unavailable — `cargo {}` exited {} and produced no summary object, so nothing here says which answer the failure above needs",
        JSON_ARGS.join(" "),
        code.map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
    );
    let noise: Vec<&str> = ndjson
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(5)
        .collect();
    if !noise.is_empty() {
        out.push_str(": ");
        out.push_str(&noise.join(" / "));
    }
    out
}

/// What `--fix` did. Either the gate is green and this is the report, or the tree is back to what
/// it was and the caller prints the verdict it already had.
enum Bump {
    /// The report, and whether `Cargo.lock`'s bytes differ from what they were before the update.
    ///
    /// The second half is not derivable from the first: `cargo update` may re-resolve to the
    /// versions the lock already had, and a green gate would then be a gate that changed nothing.
    /// Whoever reads this is the one that has to decide whether there is anything to commit.
    Green {
        report: String,
        lockfile_changed: bool,
    },
    /// The one `cargo update` that was tried, spelled out, for the line that says it was.
    Reverted(String),
}

/// The crates to update, deduplicated, in the order cargo-deny reported them.
///
/// Two advisories against one crate is one `cargo update`, which is not a saving but a
/// correctness point: the second invocation would run against a lockfile the first already
/// rewrote, so its output describes a different tree from the one the verdict was computed on.
fn packages<'a>(findings: &[&'a Finding]) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for finding in findings {
        if !out.contains(&finding.package.as_str()) {
            out.push(&finding.package);
        }
    }
    out
}

/// The `-p` argument per update, deduplicated, in the order cargo-deny reported them.
///
/// **`name@version`, not the bare name**, and this was measured rather than preferred. `cargo
/// update -p webpki-roots` against this repository's own lockfile fails outright:
///
/// ```text
/// error: specification `webpki-roots` is ambiguous
/// help: re-run this command with one of the following specifications
///   webpki-roots@0.26.11
///   webpki-roots@1.0.9
/// ```
///
/// A crate with two majors in the lock is the normal case, not a corner — `webpki-roots` 1.x and
/// its 0.26 compatibility package are both here, and that pair is exactly what RUSTSEC-2026-0381
/// is about. cargo-deny's own `Solution` note says "try `cargo update -p webpki-roots`", which is
/// a command that does not run, and taking the spec from the inclusion graph's locked version
/// instead is the same decision as taking the crate name from it: structured field, never prose.
///
/// The version is an `Option` because the graph is where it comes from and a diagnostic can arrive
/// without one; the bare name is the fallback, and it is the spec that was ambiguous.
///
/// Deduplication is by the whole spec. Two advisories against one crate at one locked version is
/// still one update, which is the case the ticket names; two against one crate at two *different*
/// locked versions is two crates in the lock wearing one name, and one update would leave one of
/// them where it was.
fn specs(findings: &[&Finding]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for finding in findings {
        let spec = spec(finding);
        if !out.contains(&spec) {
            out.push(spec);
        }
    }
    out
}

/// One finding's `-p` spec, `name@version`, or the bare name where the graph gave no version.
///
/// Spelled once because two callers read it: the update list, and the report's yanked row, which
/// names a command it will not itself run. Two derivations of the same command is RT-18's drift,
/// and here it would be a report telling a person to run something the module knows cargo may
/// refuse.
fn spec(finding: &Finding) -> String {
    finding.version.as_deref().map_or_else(
        || finding.package.clone(),
        |version| format!("{}@{version}", finding.package),
    )
}

/// Run the update, re-run the gate, and either keep the result or put the lockfile back.
///
/// The only impure thing in this module besides `run` and `json_pass`: the fold is `bumpable`, the
/// diff is `lock_diff` and the words are `bumped`, all of which are pure and all of which are
/// tested without a subprocess.
fn bump(root: &Path, findings: &[&Finding]) -> Result<Bump> {
    let lock = root.join("Cargo.lock");
    // Read before the update, and restore by writing this back. Not `git checkout
    // Cargo.lock`: that discards whatever else the worktree had staged or modified in that file,
    // and xtask has no business assuming it is running in a clean tree — `just deny --fix` on a
    // laptop is the case this is for, and that tree is dirty by definition.
    let original = fs::read_to_string(&lock)
        .with_context(|| format!("read {} before updating it", lock.display()))?;

    let specs = specs(findings);
    let tried = command(&specs);

    // **One call, every spec in it**, which is RT-46's option 1 and the half of it that was measured
    // rather than argued. One `cargo update` per spec is what this file used to do, and it fails on
    // the case it exists for: this repository's four zstd advisories are on one chain — `zstd` ->
    // `zstd-safe` -> `zstd-sys` — so the first call re-resolves the second one's crate, the second
    // call then names a locked version that no longer exists, cargo exits 1, and the run reverts a
    // lockfile it had already improved. Handing cargo the whole list at once resolves every spec
    // against the lockfile as the verdict described it, before anything moves: measured on those
    // four, it moves exactly `zstd` 0.13.4, `zstd-safe` 7.4.0 and `zstd-sys` 2.1.2 and nothing else.
    //
    // A spec cargo will not move does not fail the call either — measured with `zstd-sys = "=2.1.0"`
    // added to a crate, the same call exits 0 having moved the other two and left that one alone —
    // which is right, because the authority for whether the bump worked is the re-run below and not
    // the exit status of an update.
    let outcome = Command::new("cargo")
        .args(update_args(&specs))
        .current_dir(root)
        .output()
        .with_context(|| format!("`{tried}` could not be run"))?;
    if !outcome.status.success() {
        fs::write(&lock, &original).with_context(|| format!("restore {}", lock.display()))?;
        bail!(
            "`{tried}` failed, and Cargo.lock was restored: {}",
            String::from_utf8_lossy(&outcome.stderr).trim()
        );
    }

    if !gate(root, Stdio::null)?.success() {
        fs::write(&lock, &original).with_context(|| format!("restore {}", lock.display()))?;
        return Ok(Bump::Reverted(tried));
    }

    let updated = fs::read_to_string(&lock)
        .with_context(|| format!("read {} after updating it", lock.display()))?;
    Ok(Bump::Green {
        report: bumped(findings, &lock_diff(&original, &updated)?),
        lockfile_changed: original != updated,
    })
}

/// The argv of the one `cargo update`, every spec in it.
///
/// Split out and pure so the shape that matters is testable without a subprocess: RT-46's defect is
/// this list reaching cargo one spec at a time, and nothing about a `Vec<String>` can show that on
/// its own.
fn update_args(specs: &[String]) -> Vec<String> {
    let mut args = vec!["update".to_owned()];
    for spec in specs {
        args.push("-p".to_owned());
        args.push(spec.clone());
    }
    args
}

/// The same command as one line, for the two places that have to name it: the failure that says the
/// lockfile was put back, and the revert that says the gate is still red.
fn command(specs: &[String]) -> String {
    format!("cargo {}", update_args(specs).join(" "))
}

/// One crate's movement across a lockfile rewrite.
///
/// Version sets rather than versions, because a lockfile legitimately carries two majors of one
/// crate (`webpki-roots` 1.0.9 and 0.26.11 are both in this tree's lock), and a diff that reports
/// the first of them is a diff that reports the wrong one half the time.
#[derive(Debug, PartialEq, Eq)]
struct LockChange {
    package: String,
    before: Vec<String>,
    after: Vec<String>,
}

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockedPackage>,
}

#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
}

fn lock_versions(lock: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let parsed: Lockfile = toml::from_str(lock).context("parse Cargo.lock")?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for package in parsed.package {
        out.entry(package.name).or_default().push(package.version);
    }
    for versions in out.values_mut() {
        versions.sort();
    }
    Ok(out)
}

/// Every crate whose version set changed, alphabetically.
///
/// Alphabetical and not "the named crates first": the report orders them, and this is the data.
fn lock_diff(before: &str, after: &str) -> Result<Vec<LockChange>> {
    let before = lock_versions(before)?;
    let after = lock_versions(after)?;
    let mut changes = Vec::new();
    for package in before.keys().chain(after.keys()) {
        if changes
            .iter()
            .any(|change: &LockChange| change.package == *package)
        {
            continue;
        }
        let was = before.get(package).cloned().unwrap_or_default();
        let now = after.get(package).cloned().unwrap_or_default();
        if was != now {
            changes.push(LockChange {
                package: package.clone(),
                before: was,
                after: now,
            });
        }
    }
    changes.sort_by(|a, b| a.package.cmp(&b.package));
    Ok(changes)
}

/// `0.103.15 -> 0.103.16`, and the two edge cases spelled out rather than rendered as an empty
/// side of an arrow.
fn movement(change: &LockChange) -> String {
    match (change.before.is_empty(), change.after.is_empty()) {
        (true, _) => format!("new at {}", change.after.join(", ")),
        (_, true) => format!("dropped from {}", change.before.join(", ")),
        _ => format!(
            "{} -> {}",
            change.before.join(", "),
            change.after.join(", ")
        ),
    }
}

/// What the bump did, as text. Pure, so the one thing a reviewer of an automated lockfile change
/// has to read is pinned by a test rather than by a run.
///
/// The named crates first, each with its advisory id — the lockfile diff alone does not say which
/// advisory asked for a line, and that is the one thing the person reading it needs. Then the rest
/// of the diff, counted and listed: `cargo update -p <crate>` re-resolves whatever else cargo
/// feels like re-resolving (seven packages, on the tokio bump this was measured against), and a
/// report naming only the crates it asked for would be describing a smaller change than the one on
/// disk.
fn bumped(findings: &[&Finding], changes: &[LockChange]) -> String {
    let named = packages(findings);
    let updates = specs(findings);
    let mut lines: Vec<String> = vec![format!(
        "advisory bump: {} {}, every one of them naming a fix; one `cargo update` with {} of them, and `cargo deny check` is green after it",
        findings.len(),
        plural(VULNERABILITY, findings.len()),
        updates.len(),
    )];

    for finding in findings {
        let moved = changes
            .iter()
            .find(|change| change.package == finding.package)
            .map_or_else(
                // The gate is green, so something cleared this advisory — but not a line of the
                // lockfile naming this crate. Say that rather than print a version pair that is
                // not in the diff.
                || "no line of Cargo.lock naming it changed".to_owned(),
                movement,
            );
        lines.push(format!(
            "  {:<18}  {:<28}  {moved}",
            id_label(finding),
            finding.package
        ));
    }

    let incidental: Vec<&LockChange> = changes
        .iter()
        .filter(|change| !named.contains(&change.package.as_str()))
        .collect();
    lines.push(format!(
        "  Cargo.lock: {} {} changed — {} named above, {} cargo re-resolved alongside them",
        changes.len(),
        plural("package", changes.len()),
        changes.len() - incidental.len(),
        incidental.len()
    ));
    for change in &incidental {
        lines.push(format!("    {:<28}  {}", change.package, movement(change)));
    }

    lines.push(
        "  the new Cargo.lock is on disk and uncommitted: the advisories gate is the only one that has run against it, and a semver-compatible bump can still break a build or a test"
            .to_owned(),
    );
    lines.join("\n")
}

/// What a green `--fix` did, in the form a program is meant to read.
///
/// Machine-readable because the reader of it is one, and the rule this exists to satisfy is that
/// nothing downstream re-parses the prose below it. The prose is carried *inside* it for the same
/// reason: a pull request has to say both what the gate found and what was done about it, and a
/// driver that rebuilt either sentence from cargo-deny's output would be a second renderer of a
/// report this module already settled.
///
/// `Deserialize` as well as `Serialize`, and `pub` rather than private, because the reader is
/// [`crate::advisory_pr`]: one struct is the artefact's whole definition, so a field added here is
/// a field the reader sees, and the two cannot disagree about a rename.
#[derive(Debug, Deserialize, Serialize)]
pub struct BumpArtefact {
    /// Every advisory the bump cleared, in cargo-deny's emission order.
    ///
    /// This is what a driver deduplicates against. "Is this advisory already open?" is a question
    /// about advisories, and a branch name is a lossy encoding of it — the ids are the thing a
    /// forge can be asked about.
    pub advisories: Vec<String>,
    /// The crates those advisories name, deduplicated, in the same order.
    pub packages: Vec<String>,
    /// The `-p` argument of every spec the one `cargo update` was given, `name@version`, as
    /// [`specs`] derived them.
    pub updates: Vec<String>,
    /// Whether `Cargo.lock` actually differs from what it was before the updates.
    ///
    /// Not derivable from the rest, which is why it is a field: see [`Bump::Green`]. A driver
    /// commits a lockfile only when this says it moved.
    pub lockfile_changed: bool,
    /// The verdict line, computed before the bump: what the gate said about the old lockfile.
    pub verdict: String,
    /// What the bump did, as [`bumped`] renders it.
    pub report: String,
}

/// Delete the artefact before a `--fix` run, so that what is on disk afterwards describes this run.
///
/// **Not tidiness.** In the container this gate runs in, `target/` is a cache volume that survives
/// between runs — `env::toolchain` mounts `/src/target` — so an artefact left behind by last
/// night's bump is still there tonight, and the driver reading it would export a `Cargo.lock` for
/// a bump that no longer happened. Absent is not an error: a tree with no advisories has never
/// written one, and neither has a `--fix` that reverted.
fn clear_artefact(root: &Path) -> Result<()> {
    let path = root.join(ARTEFACT);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

/// The artefact's JSON, rendered from data rather than from the tree.
///
/// Pure, and therefore testable against the same fixtures the rest of this module is: nothing here
/// re-reads `Cargo.lock` or re-runs the gate, so the two things a reviewer of an automated bump
/// needs to trust — the ids, and whether the lock moved — are pinned by a test rather than by a
/// run.
#[must_use]
fn artefact(findings: &[&Finding], verdict: &str, report: &str, lockfile_changed: bool) -> String {
    let mut advisories: Vec<String> = Vec::new();
    for finding in findings {
        // A finding with no advisory contributes nothing to this list, because there is nothing
        // in it to deduplicate against. Skipping it is the point of `Finding::id` being an
        // `Option`: the alternative is a placeholder id in the list a forge keys its open pull
        // requests on.
        let Some(id) = &finding.id else { continue };
        if !advisories.contains(id) {
            advisories.push(id.clone());
        }
    }
    let value = BumpArtefact {
        advisories,
        packages: packages(findings).into_iter().map(str::to_owned).collect(),
        updates: specs(findings),
        lockfile_changed,
        verdict: verdict.to_owned(),
        report: report.to_owned(),
    };
    // Infallible in every field this struct has: they are all strings or lists of strings. A
    // failure here means the struct gained something `serde` cannot write, which is a bug in this
    // file rather than a condition to handle at runtime.
    serde_json::to_string_pretty(&value)
        .expect("every field of BumpArtefact is a string or a list of strings")
}

/// Write it, and say where it went.
///
/// The line is for the person who ran `just deny --fix` on a laptop: the bump report ends by
/// saying the new `Cargo.lock` is on disk and uncommitted, and this is the second half of that —
/// the record of it, in the directory they are already not looking at.
fn write_artefact(root: &Path, text: &str) -> Result<()> {
    let path = root.join(ARTEFACT);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    println!(
        "  recorded in {}: what the step that opens the pull request reads",
        repo::rel(root, &path)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures are cargo-deny 0.20.2's own NDJSON, captured on this machine and cut to the
    /// fields this module reads. The values and the nesting are cargo-deny's; what was cut is what
    /// this module does not read (`labels`, `message`, the advisory's `cvss` and `categories`, the
    /// depth below the first parent level), because a fixture that carries all of it is 21 KB of
    /// inclusion graph nobody can review. Capture with:
    ///
    /// ```sh
    /// cargo deny --format json check advisories 2> advisories.ndjson
    /// ```
    ///
    /// `both-kinds.ndjson` is the whole of one real run — the tree at db8534c, which is master
    /// before PR #49's zbus `ignore` entries and PR #51's lockfile bump, so it holds both kinds at
    /// once. The two single-kind fixtures are that same run filtered to one kind, with the
    /// summary's `errors` rewritten to match the diagnostics left in the file: a fixture whose own
    /// summary contradicts its own diagnostics would test the parser's tolerance for a
    /// contradiction rather than the split it is here to pin.
    ///
    /// `yanked.ndjson` is a different run: a scratch copy of 9630843, the master whose `Cargo.lock`
    /// still held the withdrawn `async-executor 1.14.0`, captured with this module's own
    /// `JSON_ARGS` — `--offline` included, because a fixture has to be what the gate actually reads
    /// and not what an online run happens to print. It is the whole of that run's advisories
    /// output: one diagnostic, and the summary that reports it alongside the 22 notes a tree with
    /// every `ignore` entry honoured still counts.
    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/deny")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    #[test]
    fn a_run_that_found_both_kinds_says_both() {
        let verdict = triage(&fixture("both-kinds.ndjson"));
        let counts = verdict.summary.expect("the run printed a summary");
        assert_eq!(verdict.findings.len(), 5);
        assert_eq!(counts.errors, 5);
        // That tree had 8 `ignore` entries and reported 16 notes, and not one of them arrived as a
        // diagnostic: findings come from the diagnostics, totals from the summary.
        assert_eq!(counts.notes, 16);

        let kinds = verdict
            .findings
            .iter()
            .map(|finding| finding.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                "vulnerability",
                "vulnerability",
                "unmaintained",
                "unmaintained",
                "unmaintained"
            ]
        );

        // The line a failed supply-chain job would carry, in the order cargo-deny emitted the
        // findings: the two kinds counted separately, and the notes named as notes.
        let report = report(&verdict).expect("findings to report");
        assert!(
            report.starts_with(
                "advisory triage: 2 vulnerabilities, 3 unmaintained; 16 notes, which is where cargo-deny counts"
            ),
            "{report}"
        );
        assert_eq!(report.lines().count(), 1 + 5 + 3 + 1, "{report}");
    }

    #[test]
    fn a_vulnerability_with_a_fix_names_the_upgrade_and_the_command_that_reaches_it() {
        let verdict = triage(&fixture("vulnerability-with-fix.ndjson"));
        assert_eq!(verdict.findings.len(), 2);
        for finding in &verdict.findings {
            let Fix::Named(body) = &finding.fix else {
                panic!("expected a named fix, got {:?}", finding.fix);
            };
            assert!(body.starts_with("Upgrade to "), "{body}");
            assert!(body.contains("cargo update -p "), "{body}");
        }
        let report = report(&verdict).expect("findings to report");
        assert!(
            report.starts_with("advisory triage: 2 vulnerabilities"),
            "{report}"
        );
        assert!(report.contains("webpki-roots 1.0.9"), "{report}");
        assert!(
            report.contains("names a fix: Upgrade to >=1.0.10"),
            "{report}"
        );
    }

    #[test]
    fn an_unmaintained_finding_with_no_upgrade_is_stuck_and_names_who_pulls_it_in() {
        let verdict = triage(&fixture("unmaintained-stuck.ndjson"));
        assert_eq!(verdict.findings.len(), 3);
        for finding in &verdict.findings {
            assert_eq!(finding.kind, "unmaintained");
            assert_eq!(finding.fix, Fix::Stuck);
            assert!(
                !finding.pulled_in_by.is_empty(),
                "{} has no parents",
                id_label(finding)
            );
        }
        let zvariant = verdict
            .findings
            .iter()
            .find(|finding| finding.package == "zvariant")
            .expect("zvariant is in the run");
        assert_eq!(zvariant.version.as_deref(), Some("5.15.0"));
        // One hop, as the graph is: `zvariant` is pulled in by `zbus`. The name in `deny.toml`'s
        // ignore reason is `ashpd`, which pins `zbus` and sits a level higher — that chain is in
        // cargo-deny's own output above the verdict, and this line does not pretend to be it.
        assert!(
            zvariant.pulled_in_by.contains(&"zbus 5.19.0".to_owned()),
            "{:?}",
            zvariant.pulled_in_by
        );

        let report = report(&verdict).expect("findings to report");
        assert!(
            report.starts_with("advisory triage: 3 unmaintained"),
            "{report}"
        );
        assert!(report.contains("no fix is named"), "{report}");
        assert!(report.contains("pulled in by: "), "{report}");
    }

    /// The failure RT-47 is. On 2026-11-02 crates.io withdrew `async-executor 1.14.0`, cargo-deny
    /// said exactly the right thing — `error[yanked]: detected yanked crate` — and the triage under
    /// it said the diagnostic "named no advisory — cargo-deny's own complaint, most likely about
    /// its config". It is not about the config: it names a crate and a locked version, and the
    /// answer is a `cargo update`.
    #[test]
    fn a_yanked_crate_is_named_by_crate_and_version_rather_than_blamed_on_the_config() {
        let verdict = triage(&fixture("yanked.ndjson"));
        assert_eq!(
            verdict.not_advisories, 0,
            "the yanked diagnostic went to the config bucket"
        );
        assert_eq!(verdict.findings.len(), 1);
        assert_eq!(
            verdict.summary.expect("the run printed a summary").errors,
            1
        );

        let finding = &verdict.findings[0];
        assert_eq!(finding.kind, YANKED);
        // Crate and locked version from the inclusion graph, exactly as every other kind takes
        // them. Never from `message`, which spells the bare name cargo refuses when a crate is in
        // the lock twice.
        assert_eq!(finding.package, "async-executor");
        assert_eq!(finding.version.as_deref(), Some("1.14.0"));
        // No advisory to name, and the field says so: a withdrawal from crates.io is not a RUSTSEC
        // entry, so there is no id anywhere in the diagnostic.
        assert_eq!(finding.id, None);
        // Measured: a yanked diagnostic carries no `notes` at all, so there is no `Solution:` line
        // to read a fix out of.
        assert_eq!(finding.fix, Fix::NoNote);

        let report = report(&verdict).expect("a finding to report");
        assert!(report.starts_with("advisory triage: 1 yanked"), "{report}");
        assert!(report.contains("async-executor 1.14.0"), "{report}");
        assert!(
            report.contains("cargo update -p async-executor@1.14.0"),
            "{report}"
        );
        // The whole point of the ticket, asserted as an absence: the line must not send the reader
        // to `deny.toml`.
        assert!(!report.contains("config"), "{report}");
        assert!(!report.contains("unnamed"), "{report}");

        // And the `--fix` decision, pinned rather than claimed in prose: the bump still refuses a
        // yanked crate. Why that is still the answer is `VULNERABILITY`'s doc.
        assert!(
            verdict.bumpable().is_none(),
            "`--fix` acted on a yanked crate"
        );
    }

    /// Every finding carries the note, so `Fix::Stuck` on a `vulnerability` is reachable from the
    /// data and not from a category guess. None of the captures above contains one — an advisory
    /// with `patched = []` and `informational = "unmaintained"` is what produced the stuck ones
    /// here — so this one is cargo-deny's shape and cargo-deny's own strings, assembled by hand.
    #[test]
    fn a_stuck_vulnerability_is_a_case_and_renders_as_one() {
        let ndjson = r#"{"fields":{"code":"vulnerability","severity":"error","advisory":{"id":"RUSTSEC-2026-0000","package":"stuck-crate"},"notes":["ID: RUSTSEC-2026-0000","Solution: No safe upgrade is available!"],"graphs":[{"Krate":{"name":"stuck-crate","version":"1.2.3"},"parents":[{"Krate":{"name":"who-pins-it","version":"9.9.9"}}]}]},"type":"diagnostic"}
{"fields":{"advisories":{"errors":1,"helps":0,"notes":0,"warnings":0}},"type":"summary"}"#;
        let verdict = triage(ndjson);
        let finding = &verdict.findings[0];
        assert_eq!(finding.kind, "vulnerability");
        assert_eq!(finding.fix, Fix::Stuck);
        assert_eq!(finding.pulled_in_by, vec!["who-pins-it 9.9.9"]);

        let report = report(&verdict).expect("a finding to report");
        assert!(
            report.starts_with("advisory triage: 1 vulnerability"),
            "{report}"
        );
        assert!(
            report.contains("vulnerability") && report.contains("no fix is named"),
            "{report}"
        );
    }

    #[test]
    fn a_clean_advisories_run_says_the_failure_is_elsewhere() {
        let verdict = triage(&fixture("clean-summary.ndjson"));
        assert!(verdict.findings.is_empty());
        let counts = verdict.summary.expect("the run printed a summary");
        assert_eq!(counts.errors, 0);
        // 22 notes behind 11 `ignore` entries: a note is not an ignore entry, and this fixture is
        // the tree where every one of them is honoured.
        assert_eq!(counts.notes, 22);

        let report = report(&verdict).expect("a verdict to report");
        assert!(report.contains("advisories are clean"), "{report}");
        assert!(report.contains("bans, licences or sources"), "{report}");
    }

    /// A green run never reaches the second pass at all — `run` returns before it — but the shape
    /// that keeps a verdict block from ever printing empty is here, and pinned.
    #[test]
    fn nothing_says_and_nothing_is_printed_when_the_second_pass_said_nothing() {
        let verdict = triage("");
        assert!(verdict.findings.is_empty());
        assert!(verdict.summary.is_none());
        assert!(report(&verdict).is_none());
    }

    #[test]
    fn a_line_that_is_not_json_is_counted_and_said_so() {
        let ndjson = format!(
            "error[vulnerability]: not JSON at all\n{}",
            fixture("clean-summary.ndjson")
        );
        let verdict = triage(&ndjson);
        assert_eq!(verdict.unread, 1);
        // The summary still arrived, so there is a verdict — and it admits the line it lost.
        let report = report(&verdict).expect("a verdict to report");
        assert!(
            report.contains("1 line of cargo-deny's JSON this module could not read"),
            "{report}"
        );
    }

    #[test]
    fn a_note_is_not_a_finding() {
        let ndjson = r#"{"fields":{"code":"unmaintained","severity":"note","advisory":{"id":"RUSTSEC-2026-0001","package":"noted"},"notes":["Solution: No safe upgrade is available!"],"graphs":[{"Krate":{"name":"noted","version":"0.1.0"},"parents":[]}]},"type":"diagnostic"}
{"fields":{"advisories":{"errors":0,"helps":0,"notes":1,"warnings":0}},"type":"summary"}"#;
        let verdict = triage(ndjson);
        assert!(verdict.findings.is_empty(), "a note became a finding");
    }

    #[test]
    fn a_kind_label_is_pluralised_and_an_adjective_is_not() {
        assert_eq!(plural("vulnerability", 2), "vulnerabilities");
        assert_eq!(plural("vulnerability", 1), "vulnerability");
        assert_eq!(plural("unmaintained", 3), "unmaintained");
        assert_eq!(plural("yanked", 2), "yanked");
    }

    /// A malformed `deny.toml` is a gate failure the advisories check never took part in. Captured
    /// from cargo-deny 0.20.2 run against a `deny.toml` carrying a key `[bans]` does not have: one
    /// diagnostic with no `advisory` field at all, a `log` line, and no summary.
    #[test]
    fn a_diagnostic_that_names_no_advisory_is_not_triaged_as_one() {
        let ndjson = r#"{"fields":{"code":"unexpected-keys","labels":[{"column":1,"line":92,"message":"","span":"deny"}],"message":"found 1 unexpected keys","severity":"error"},"type":"diagnostic"}
{"fields":{"level":"ERROR","message":"failed to deserialize config from 'deny.toml'","timestamp":"2026-10-23T12:27:34Z"},"type":"log"}"#;
        let verdict = triage(ndjson);
        assert!(verdict.findings.is_empty(), "{verdict:?}");
        assert_eq!(verdict.not_advisories, 1);
        assert!(verdict.summary.is_none(), "{verdict:?}");

        let report = report(&verdict).expect("a diagnostic to report");
        assert!(
            report.contains("1 diagnostic named no advisory"),
            "{report}"
        );
        // It is not a verdict about advisories, so it must not point at the rule that decides
        // which advisory gets an ignore entry.
        assert!(!report.contains("deny.toml's rule"), "{report}");
    }

    #[test]
    fn the_second_pass_is_a_cargo_deny_invocation_and_not_a_cargo_one() {
        // `deny` has to be in the argv, and before `--offline`: without the subcommand this is
        // `cargo --offline --format json …`, and cargo answers it with "unexpected argument
        // '--format' found" over its own usage block. Pinned because that is what the first
        // version of this line actually ran.
        assert_eq!(
            JSON_ARGS.join(" "),
            "deny --offline --format json check advisories"
        );
    }

    /// A verdict and its summary, assembled from one diagnostic per argument. The `--fix`
    /// predicate is a fold over *every* finding, so most of its cases are "the same findings as
    /// the happy path, plus one that is not", and writing each as a captured file would be nine
    /// near-identical fixtures of cargo-deny's own shape. The four real captures are what pin the
    /// shape; this pins the fold over it.
    fn ndjson(diagnostics: &[&str]) -> String {
        let mut out: Vec<String> = diagnostics.iter().map(|&line| line.to_owned()).collect();
        out.push(format!(
            r#"{{"fields":{{"advisories":{{"errors":{},"helps":0,"notes":0,"warnings":0}}}},"type":"summary"}}"#,
            diagnostics.len()
        ));
        out.join("\n")
    }

    /// One `vulnerability` with a reachable fix, in cargo-deny's own shape.
    fn vulnerability(id: &str, package: &str, version: &str) -> String {
        format!(
            r#"{{"fields":{{"code":"vulnerability","severity":"error","advisory":{{"id":"{id}","package":"{package}"}},"notes":["Solution: Upgrade to >=9.9.9 (try `cargo update -p {package}`)"],"graphs":[{{"Krate":{{"name":"{package}","version":"{version}"}},"parents":[]}}]}},"type":"diagnostic"}}"#
        )
    }

    #[test]
    fn a_run_of_nothing_but_fixable_vulnerabilities_is_bumpable() {
        let verdict = triage(&fixture("vulnerability-with-fix.ndjson"));
        let bumpable = verdict.bumpable().expect("every finding names a fix");
        assert_eq!(bumpable.len(), 2);
        // Two advisories, two distinct crates, so two updates. The plan for this ticket said
        // "one crate" for this fixture and that is wrong about the file: RUSTSEC-2026-0382 is
        // against `rustls-webpki` and RUSTSEC-2026-0381 against `webpki-roots`. The one-update
        // case is the test below, which needs two advisories on the *same* crate.
        assert_eq!(packages(&bumpable), vec!["rustls-webpki", "webpki-roots"]);
        // And the spec carries the locked version from the inclusion graph, because the bare name
        // is the one cargo rejects: `webpki-roots` is in this tree's lockfile twice.
        assert_eq!(
            specs(&bumpable),
            vec!["rustls-webpki@0.103.15", "webpki-roots@1.0.9"]
        );
    }

    /// Measured, not preferred. `cargo update -p webpki-roots` on this repository's own lockfile
    /// exits non-zero with "specification `webpki-roots` is ambiguous" and lists `0.26.11` and
    /// `1.0.9` — which is exactly the pair RUSTSEC-2026-0381 is about, so the ambiguous case is
    /// the advisory's own case and not a corner. cargo-deny's `Solution` note suggests the bare
    /// name; the locked version comes from the graph instead, the same way the crate name does.
    #[test]
    fn the_update_spec_carries_the_locked_version_because_the_bare_name_is_ambiguous() {
        let verdict = triage(&ndjson(&[&vulnerability(
            "RUSTSEC-2026-0100",
            "two-majors",
            "1.0.9",
        )]));
        let findings = verdict.bumpable().expect("one fixable vulnerability");
        assert_eq!(specs(&findings), vec!["two-majors@1.0.9"]);
        assert!(
            matches!(&findings[0].fix, Fix::Named(body) if body.contains("cargo update -p two-majors")),
            "the prose suggests the bare name, which is the thing not used"
        );
    }

    /// A diagnostic with no inclusion graph carries no locked version, and the spec falls back to
    /// the bare name — the one cargo can reject. Better a command that fails loudly and restores
    /// the lockfile than a guessed version that updates the wrong copy of the crate.
    #[test]
    fn a_finding_with_no_locked_version_falls_back_to_the_bare_name() {
        let verdict = triage(&ndjson(&[
            r#"{"fields":{"code":"vulnerability","severity":"error","advisory":{"id":"RUSTSEC-2026-0100","package":"graphless"},"notes":["Solution: Upgrade to >=2.0.0"]},"type":"diagnostic"}"#,
        ]));
        let findings = verdict.bumpable().expect("one fixable vulnerability");
        assert_eq!(findings[0].version, None);
        assert_eq!(specs(&findings), vec!["graphless"]);
    }

    #[test]
    fn two_advisories_against_one_package_are_one_update() {
        let verdict = triage(&ndjson(&[
            &vulnerability("RUSTSEC-2026-0100", "doubly-cursed", "1.0.0"),
            &vulnerability("RUSTSEC-2026-0101", "doubly-cursed", "1.0.0"),
        ]));
        let bumpable = verdict.bumpable().expect("both name a fix");
        assert_eq!(bumpable.len(), 2);
        assert_eq!(packages(&bumpable), vec!["doubly-cursed"]);
        assert_eq!(specs(&bumpable), vec!["doubly-cursed@1.0.0"]);
    }

    /// The three kinds of "not unambiguous" that have a committed capture behind them.
    #[test]
    fn a_run_that_is_not_all_fixable_vulnerabilities_is_not_bumpable() {
        for name in [
            // Two vulnerabilities and three unmaintained: the unmaintained ones are a `deny.toml`
            // decision, so the whole run is off limits rather than partly actionable.
            "both-kinds.ndjson",
            "unmaintained-stuck.ndjson",
            // No findings at all — the gate failed on bans, licences or sources, and there is
            // nothing here to update.
            "clean-summary.ndjson",
        ] {
            let verdict = triage(&fixture(name));
            assert!(verdict.bumpable().is_none(), "{name} was bumpable");
        }
    }

    /// `a_stuck_vulnerability_is_a_case_and_renders_as_one` proves this state is reachable from
    /// cargo-deny's data rather than from a category guess. It is the state a kind-only predicate
    /// would act on, and the update it would run moves nothing.
    #[test]
    fn a_stuck_vulnerability_is_not_bumpable() {
        let verdict = triage(&ndjson(&[
            r#"{"fields":{"code":"vulnerability","severity":"error","advisory":{"id":"RUSTSEC-2026-0000","package":"stuck-crate"},"notes":["Solution: No safe upgrade is available!"],"graphs":[{"Krate":{"name":"stuck-crate","version":"1.2.3"},"parents":[]}]},"type":"diagnostic"}"#,
        ]));
        assert_eq!(verdict.findings[0].fix, Fix::Stuck);
        assert!(verdict.bumpable().is_none());
    }

    /// The condition RT-41 did not ask for, and the reason it is here: a verdict that lost a line
    /// does not know what its own findings are, so "every finding is a fixable vulnerability" is a
    /// statement about a subset of unknown size.
    #[test]
    fn a_verdict_that_lost_a_line_is_not_bumpable_however_perfect_the_rest_is() {
        let clean = ndjson(&[&vulnerability("RUSTSEC-2026-0100", "cursed", "1.0.0")]);
        assert!(
            triage(&clean).bumpable().is_some(),
            "the control case must be bumpable, or this test proves nothing"
        );

        let lossy = format!("error[vulnerability]: not JSON at all\n{clean}");
        let verdict = triage(&lossy);
        assert_eq!(verdict.unread, 1);
        assert_eq!(verdict.findings.len(), 1);
        assert!(verdict.bumpable().is_none());
    }

    /// `not_advisories > 0` is cargo-deny complaining about its own config, which means the
    /// advisories check never ran and there is nothing to be complete about.
    #[test]
    fn a_config_complaint_alongside_a_finding_is_not_bumpable() {
        let verdict = triage(&ndjson(&[
            &vulnerability("RUSTSEC-2026-0100", "cursed", "1.0.0"),
            r#"{"fields":{"code":"unexpected-keys","message":"found 1 unexpected keys","severity":"error"},"type":"diagnostic"}"#,
        ]));
        assert_eq!(verdict.not_advisories, 1);
        assert_eq!(verdict.findings.len(), 1);
        assert!(verdict.bumpable().is_none());
    }

    /// The mixed tree, which is the case whose *reason* changes and whose outcome does not. Before
    /// RT-47 a yanked crate was a `not_advisories` count, `bumpable` refused the run on
    /// `not_advisories > 0`, and the report said nothing about it — the config clause prints only
    /// when there are no findings at all, so a fixable vulnerability plus a yanked crate rendered
    /// as "1 vulnerability" while a `--fix` silently bumped nothing. It is still refused, on the
    /// kind test instead of the count test, and the refusal is now legible from the report.
    #[test]
    fn a_yanked_crate_alongside_a_fixable_vulnerability_is_refused_and_now_says_why() {
        let yanked = fixture("yanked.ndjson")
            .lines()
            .next()
            .expect("the yanked fixture has a diagnostic")
            .to_owned();
        let verdict = triage(&ndjson(&[
            &vulnerability("RUSTSEC-2026-0100", "cursed", "1.0.0"),
            &yanked,
        ]));
        assert_eq!(verdict.findings.len(), 2);
        assert_eq!(
            verdict.not_advisories, 0,
            "a yanked crate is not a config complaint"
        );
        assert!(
            verdict.bumpable().is_none(),
            "the yanked finding let the bump through"
        );

        let report = report(&verdict).expect("two findings to report");
        assert!(
            report.starts_with("advisory triage: 1 vulnerability, 1 yanked"),
            "{report}"
        );
        assert!(report.contains("cursed 1.0.0"), "{report}");
        assert!(report.contains("async-executor 1.14.0"), "{report}");
        assert!(
            report.contains("cargo update -p async-executor@1.14.0"),
            "{report}"
        );
        assert!(
            report.contains("`--fix` acts only on vulnerabilities that name a fix"),
            "{report}"
        );
        assert!(!report.contains("named no advisory"), "{report}");
        assert!(!report.contains("config"), "{report}");
    }

    /// No summary is a different failure from no findings, and the predicate must not read it as
    /// "one perfect finding and nothing else".
    #[test]
    fn a_pass_that_never_summarised_is_not_bumpable() {
        let verdict = triage(&vulnerability("RUSTSEC-2026-0100", "cursed", "1.0.0"));
        assert_eq!(verdict.findings.len(), 1);
        assert!(verdict.summary.is_none());
        assert!(verdict.bumpable().is_none());
    }

    const BEFORE: &str = r#"
[[package]]
name = "rustls-webpki"
version = "0.103.15"

[[package]]
name = "webpki-roots"
version = "1.0.9"

[[package]]
name = "webpki-roots"
version = "0.26.11"

[[package]]
name = "windows-sys"
version = "0.52.0"

[[package]]
name = "untouched"
version = "1.0.0"
"#;

    const AFTER: &str = r#"
[[package]]
name = "rustls-webpki"
version = "0.103.16"

[[package]]
name = "webpki-roots"
version = "1.0.10"

[[package]]
name = "webpki-roots"
version = "0.26.11"

[[package]]
name = "windows-sys"
version = "0.59.0"

[[package]]
name = "untouched"
version = "1.0.0"

[[package]]
name = "arrived-from-nowhere"
version = "0.1.0"
"#;

    #[test]
    fn the_lockfile_diff_is_by_version_set_and_keeps_the_crate_that_has_two_majors() {
        let changes = lock_diff(BEFORE, AFTER).expect("both parse");
        let moved: Vec<&str> = changes
            .iter()
            .map(|change| change.package.as_str())
            .collect();
        assert_eq!(
            moved,
            vec![
                "arrived-from-nowhere",
                "rustls-webpki",
                "webpki-roots",
                "windows-sys"
            ]
        );
        // `webpki-roots` is in the lock twice and only one of the two moved; the change is the
        // whole set, so the 0.26.11 compatibility package is still visible in it.
        let roots = &changes[2];
        assert_eq!(roots.before, vec!["0.26.11", "1.0.9"]);
        assert_eq!(roots.after, vec!["0.26.11", "1.0.10"]);
        assert_eq!(movement(roots), "0.26.11, 1.0.9 -> 0.26.11, 1.0.10");
        assert_eq!(movement(&changes[0]), "new at 0.1.0");
    }

    /// The trap RT-46 is: the findings are independent and the lockfile is not, so a list handed to
    /// cargo one spec at a time asks for versions that the first update already moved. One call with
    /// every spec in it resolves them all against the lock as the verdict described it.
    #[test]
    fn every_spec_goes_into_one_cargo_update_rather_than_one_call_each() {
        let verdict = triage(&fixture("vulnerability-with-fix.ndjson"));
        let findings = verdict.bumpable().expect("the happy path");
        let specs = specs(&findings);

        let args = update_args(&specs);
        assert_eq!(args[0], "update", "cargo's subcommand comes first");
        // `-p` before each spec, and one `-p` per spec: cargo reads a package id per flag, so a
        // space-separated list would be one spec with a stray argument after it.
        assert_eq!(
            args.iter().filter(|word| **word == "-p").count(),
            specs.len(),
            "{args:?}"
        );
        assert_eq!(
            args.iter().skip(2).step_by(2).collect::<Vec<&String>>(),
            vec!["rustls-webpki@0.103.15", "webpki-roots@1.0.9"],
            "{args:?}"
        );
        assert_eq!(args.len(), 1 + 2 * specs.len(), "{args:?}");

        // The same command the failure line names, spelled the way it would be typed.
        assert_eq!(
            command(&specs),
            "cargo update -p rustls-webpki@0.103.15 -p webpki-roots@1.0.9"
        );
    }

    /// The bump report a reviewer reads has to count the one command and still name every advisory,
    /// including the case where one advisory's crate moved because it sits below another on the
    /// chain — which is what a single call is for.
    #[test]
    fn the_bump_report_says_one_command_and_names_every_advisory_it_cleared() {
        let verdict = triage(&fixture("vulnerability-with-fix.ndjson"));
        let findings = verdict.bumpable().expect("the happy path");
        let report = bumped(&findings, &lock_diff(BEFORE, AFTER).expect("both parse"));

        assert!(
            report.starts_with(
                "advisory bump: 2 vulnerabilities, every one of them naming a fix; one `cargo update` with 2 of them,"
            ),
            "{report}"
        );
        assert!(
            report.contains("RUSTSEC-2026-0382") && report.contains("0.103.15 -> 0.103.16"),
            "{report}"
        );
        assert!(
            report.contains(
                "Cargo.lock: 4 packages changed — 2 named above, 2 cargo re-resolved alongside them"
            ),
            "{report}"
        );
        assert!(report.contains("windows-sys"), "{report}");
        assert!(report.contains("arrived-from-nowhere"), "{report}");
        // The advisories gate is the only one that ran against the new lockfile, and the report
        // has to say so: `cargo update` can move a semver-compatible version that changes
        // behaviour, and this gate cannot see that.
        assert!(report.contains("uncommitted"), "{report}");
    }

    /// A named crate the lockfile diff does not mention. Reachable: one `cargo update` can resolve
    /// another advisory's crate too, and the report must not print a version pair it does not have.
    #[test]
    fn a_named_crate_that_did_not_move_is_said_not_to_have_moved() {
        let verdict = triage(&ndjson(&[&vulnerability(
            "RUSTSEC-2026-0100",
            "cursed",
            "1.0.0",
        )]));
        let findings = verdict.bumpable().expect("one fixable vulnerability");
        let report = bumped(&findings, &lock_diff(BEFORE, BEFORE).expect("both parse"));
        assert!(
            report.contains("no line of Cargo.lock naming it changed"),
            "{report}"
        );
        assert!(
            report.contains("Cargo.lock: 0 packages changed — 0 named above, 0 cargo re-resolved"),
            "{report}"
        );
    }

    /// A bump that moved the middle of a chain on its way past the top of it is described by the
    /// diff, not by a list of commands: `zstd-safe` moved because it is named in the advisory list
    /// as well as because `zstd` pulled it, and the report says which version it ended at either way.
    #[test]
    fn a_crate_moved_by_another_crate_update_is_still_named_by_its_own_advisory() {
        let chain = r#"
[[package]]
name = "zstd"
version = "0.13.3"

[[package]]
name = "zstd-safe"
version = "7.3.0"
"#;
        let moved = r#"
[[package]]
name = "zstd"
version = "0.13.4"

[[package]]
name = "zstd-safe"
version = "7.4.0"
"#;
        let verdict = triage(&ndjson(&[
            &vulnerability("RUSTSEC-2026-0391", "zstd", "0.13.3"),
            &vulnerability("RUSTSEC-2026-0398", "zstd-safe", "7.3.0"),
        ]));
        let findings = verdict.bumpable().expect("two fixable vulnerabilities");
        let report = bumped(&findings, &lock_diff(chain, moved).expect("both parse"));

        // One command, two specs, and both crates named with the versions they reached.
        assert!(
            report.contains("one `cargo update` with 2 of them"),
            "{report}"
        );
        assert!(report.contains("zstd-safe"), "{report}");
        assert!(report.contains("7.3.0 -> 7.4.0"), "{report}");
        assert!(
            report.contains("Cargo.lock: 2 packages changed — 2 named above, 0 cargo re-resolved"),
            "{report}"
        );
    }

    #[test]
    fn the_unavailable_line_names_the_command_it_tried() {
        let text = unavailable("error: unexpected argument\n", Some(2));
        assert!(
            text.contains("cargo deny --offline --format json check advisories"),
            "{text}"
        );
        assert!(text.contains("exited 2"), "{text}");
        assert!(text.contains("unexpected argument"), "{text}");
    }

    /// A finding whose advisory names no id. cargo-deny 0.20.2 does not emit this — every
    /// advisory in the database has one — but it is the shape the field's own type allows, and it
    /// is the hole a `yanked` diagnostic walks into: a finding with no advisory to name. The
    /// placeholder this field used to fall back to would have printed "an unnamed advisory" in
    /// the table and appeared in the artefact as an advisory id, which is the thing a driver
    /// deduplicates open pull requests on. Assembled by hand, the way
    /// `a_stuck_vulnerability_is_a_case_and_renders_as_one` is.
    #[test]
    fn a_finding_with_no_advisory_id_says_that_it_has_none() {
        let verdict = triage(&ndjson(&[
            r#"{"fields":{"code":"vulnerability","severity":"error","advisory":{"package":"no-id-crate"},"notes":["Solution: Upgrade to >=2.0.0"],"graphs":[{"Krate":{"name":"no-id-crate","version":"1.0.0"},"parents":[]}]},"type":"diagnostic"}"#,
        ]));
        let finding = &verdict.findings[0];
        assert_eq!(finding.id, None, "a placeholder is still filling the id");
        // Named fix or not, it is not bumpable: its id is missing from the list a forge dedupes
        // open pull requests on, and an empty list is covered by any of them.
        assert!(
            verdict.bumpable().is_none(),
            "`--fix` would bump a finding it cannot deduplicate"
        );

        let report = report(&verdict).expect("a finding to report");
        assert!(report.contains(NO_ADVISORY), "{report}");
        assert!(!report.contains("unnamed"), "{report}");

        // The artefact half, which is the half that reaches a forge.
        let findings = verdict.findings.iter().collect::<Vec<&Finding>>();
        let json: serde_json::Value =
            serde_json::from_str(&artefact(&findings, "verdict", "report", true))
                .expect("the artefact is JSON");
        assert_eq!(json["advisories"], serde_json::json!([]), "{json}");
        assert!(!json.to_string().contains("unnamed"), "{json}");
        // The crate is still named, because the update list comes from the graph and not from
        // the advisory.
        assert_eq!(
            json["packages"],
            serde_json::json!(["no-id-crate"]),
            "{json}"
        );
    }

    /// The artefact is the only thing this module writes, and it is the handle a pull request is
    /// built from: the ids to deduplicate against, and a yes-or-no on whether there is a lockfile
    /// to commit. Both pinned here rather than left to a run.
    #[test]
    fn the_artefact_names_the_advisories_the_updates_and_whether_the_lock_moved() {
        let verdict = triage(&fixture("vulnerability-with-fix.ndjson"));
        let findings = verdict.bumpable().expect("the happy path");
        let changes = lock_diff(BEFORE, AFTER).expect("both parse");
        let verdict_text = report(&verdict).expect("findings to report");
        let bump_text = bumped(&findings, &changes);
        let json: serde_json::Value = serde_json::from_str(&artefact(
            &findings,
            &verdict_text,
            &bump_text,
            // The same computation `bump` makes: the bytes differ or they do not, which is not the
            // same question as whether any version set moved.
            BEFORE != AFTER,
        ))
        .expect("the artefact is JSON");

        // In cargo-deny's emission order, which is the order the report above prints them in: one
        // list a reader can line up against the other.
        assert_eq!(
            json["advisories"],
            serde_json::json!(["RUSTSEC-2026-0382", "RUSTSEC-2026-0381"]),
            "{json}"
        );
        assert_eq!(
            json["packages"],
            serde_json::json!(["rustls-webpki", "webpki-roots"]),
            "{json}"
        );
        assert_eq!(
            json["updates"],
            serde_json::json!(["rustls-webpki@0.103.15", "webpki-roots@1.0.9"]),
            "{json}"
        );
        assert_eq!(json["lockfile_changed"], true, "{json}");
        let verdict_line = json["verdict"].as_str().expect("a string field");
        assert!(
            verdict_line.starts_with("advisory triage: 2 vulnerabilities"),
            "{verdict_line}"
        );
        let report_line = json["report"].as_str().expect("a string field");
        assert!(
            report_line.starts_with("advisory bump: 2 vulnerabilities"),
            "{report_line}"
        );
    }

    /// A green gate whose updates changed nothing in the lock. Reachable — `cargo update` is free to
    /// re-resolve to the versions already there — and the case where whoever reads this must not
    /// commit an unchanged file or open a pull request for a bump that did not happen.
    #[test]
    fn an_artefact_for_a_lock_that_did_not_move_says_that_it_did_not_move() {
        let verdict = triage(&fixture("vulnerability-with-fix.ndjson"));
        let findings = verdict.bumpable().expect("the happy path");
        let json: serde_json::Value =
            serde_json::from_str(&artefact(&findings, "verdict", "report", false))
                .expect("the artefact is JSON");

        assert_eq!(json["lockfile_changed"], false, "{json}");
        // The ids are still there. The record describes these advisories whether or not anything
        // moved, and it is the half a driver reads first that decides there is nothing to push.
        assert_eq!(
            json["advisories"].as_array().expect("a list").len(),
            2,
            "{json}"
        );
    }

    /// Trap 1, pinned where it would do the damage. `advisories` is the list #60's forge step
    /// deduplicates open pull requests against, and a yanked finding has no advisory to put in it.
    /// It cannot reach [`artefact`] through the real path — `bumpable` requires kind
    /// `vulnerability`, asserted here first — so the call below hands the findings over directly,
    /// which is the only way to pin that the list stays clean rather than that the door happens to
    /// be shut today.
    #[test]
    fn a_yanked_finding_never_reaches_the_advisory_ids_a_pull_request_is_deduped_on() {
        let verdict = triage(&fixture("yanked.ndjson"));
        assert!(
            verdict.bumpable().is_none(),
            "the only path to the artefact is open, which is a different bug"
        );
        let findings = verdict.findings.iter().collect::<Vec<&Finding>>();
        let json: serde_json::Value =
            serde_json::from_str(&artefact(&findings, "verdict", "report", true))
                .expect("the artefact is JSON");
        assert_eq!(json["advisories"], serde_json::json!([]), "{json}");
        assert!(!json.to_string().contains("unnamed"), "{json}");
        assert!(!json.to_string().contains(NO_ADVISORY), "{json}");
    }
}
