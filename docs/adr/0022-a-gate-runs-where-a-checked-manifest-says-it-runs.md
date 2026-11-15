# 0022. A gate runs where a checked manifest says it runs

- **Status:** Accepted
- **Date:** 2026-10-07
- **Amended:** 2026-10-08 — the manifest's responsibility was always "every recipe CI runs";
  only four of the five directions that follow were enforced, and the missing one was the
  direction the prose named last. It is enforced now. Amended rather than superseded: the
  decisions above stand, including the rejection of computed reachability, which the new
  direction observes.
- **Amended:** 2026-10-08 — RT-8 and RT-10 watched the two gates this record wired fail their jobs, as the record
  itself demanded rather than as arithmetic; one of the two recipes could not fail for the
  break it was wired against, and does now. See the final amendment.
- **Amended:** 2026-11-04 — a Nix pipeline runs alongside the Dagger one (`.forgejo/workflows/ci-nix.yaml`,
  `nix/ci.nix`). The manifest continues to track the Dagger pipeline through `jobs.rs` until
  retirement; the Nix pipeline runs the same `just` recipes for impure stages and builds the
  same gate coverage as crane derivations for pure ones. See the final amendment.
- **Amended:** 2026-11-04 — the Dagger pipeline is retired ([ADR 0024](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0024-the-pipeline-is-derivations-and-a-shell.md)). The workflows are now the
  only CI entry point, the manifest's `ci` fields name their jobs, and a derivation stage counts
  as coverage through a table that says which recipes it stands in for. The five directions are
  unchanged; what reads them is. See the final amendment.

## Context

Five checks in this repository ran nowhere.

`apps/counter/platforms/android/Justfile:20` is `./gradlew :app:lintDebug`. Gradle was
configured for it, the recipe was written for it, and nothing aggregate reaches it.
`apps/counter/platforms/apple/Justfile:20-22` is `swiftlint lint CounterApp/`, same shape,
same cause, and it was not in the list anyone had written down — it was found while verifying
the other four. `deny` and `typos` are root recipes that `just ci` does not reach and that
appear only in the `supply_chain` job, which is probably right and was certainly not written
down anywhere, so nobody could tell which of the two it was. `i18n::check-data` has run under
`just ci` since ADR 0021 and under no job at all, a gap that ADR named in its own text and
left open.

The property is invisible from the file that defines the recipe, and there are two reasons it
is invisible rather than merely hidden.

**Recipes are reached by two different dispatch mechanisms, and only one of them is
enumerable.** `Justfile:1-4` declares four modules — `i18n`, `lib`, `proto`, `rust` — and
`just --summary` at the root lists the root recipes plus those four and nothing else: no
`apps/*` recipe, no `services/*` recipe. That is not a gap in `just`, it is how the app and
service Justfiles are reached at all — by *path dispatch*, `just apps/counter/platforms/
android/check` finding `apps/counter/platforms/android/Justfile` without any declaration
anywhere pointing at it. So `just --summary`, `just --list` and every other introspection
rooted in the root file see a repository with roughly a third of the recipes it has, and the
two-thirds it misses are exactly the ones with native toolchains in them. Any check built on
that enumeration inherits the blind spot.

**The aggregates are bash loops, not just dependencies.** `check: rust-check (_each "check")`
depends on `_each`, whose body is `for m in $(just modules); do just "$m/{{ recipe }}"; done`.
`apps/counter/Justfile`'s own `check` loops `rust_modules`, an assignment listing `core
client shells/tui shells/web shells/desktop`. Whether a given crate's `check` runs is a fact
about a shell loop and a list, and no dependency graph over just's own `dependencies` edges
contains it. The same loops are how the platform projects get *left out*: `rust_modules` lists
crates, and `_platforms` — which does list the platform projects — is used by `doctor`, `clean`
and `native`, and by none of `check`, `build`, `test`.

The repository had already written down the rule it wanted, in prose, in the wrong place:

> Two passes on purpose. The workspace pass covers crates/, services/ and tools/, which have no
> Justfile of their own; the module pass adds what cargo alone does not reach — wasm32 clippy,
> Gradle lint, swiftlint.

One of those three was true. `apps/counter/shells/web/Justfile:18-20` really does run clippy
against `wasm32-unknown-unknown`. Gradle lint and swiftlint were reached by nothing, and the
comment that named them — placed where a reader would look, in the definition of the aggregate
that does not reach them — is why nobody noticed for as long as the platform Justfiles have
existed. A sentence that describes coverage is not coverage; it is a thing that goes wrong
silently.

RT-11 framed this as the rule and RT-6, RT-8 and RT-10 as its instances, and that framing is
the decision: fixing five gates leaves the sixth to be found the way these five were, by
accident, usually by someone who needed the gate for something else.

## Decision

`gates.toml` at the repository root lists every gate recipe and where it runs.
`tools/xtask/src/gates.rs` builds the recipe graph and asserts five things against it — four
as originally decided, plus the direction added by the 2026-10-08 amendment, listed here as the
fifth so the enumeration below is the whole set.

A **gate** is a recipe named exactly `check`, `build` or `test`. There are 30 of them across
the 10 Justfiles outside `templates/`, three per file.

An **entry** is one `[[gate]]` block: `path` (one or more recipe ids, `*` matching exactly one
path segment), `local` (does `just ci` reach it), `ci` (which jobs reach it, spelled as their
`pub async fn` in `tools/ci/src/jobs.rs`), `reason` (required unless it runs both locally and
in CI), `terminal` (stop traversing here — what runs inside belongs to another workspace, so
following its edges would claim coverage this repository does not have). Entries are patterns,
not instances: `apps/*/core/check` already covers the second app, so scaffolding an app is not
an edit to this file. A new *shape* of gate is an edit.

The invariant then:

1. **Completeness.** Every gate-shaped recipe is claimed by an entry. This is the direction
   that turns "is this recipe wired?" — a question needing a heuristic — into "did anybody
   think about this recipe?", which is a lookup, and it is the whole point of the manifest.
2. **No stale claims.** An entry whose `path` matches no recipe fails, so the file cannot
   become a record of a repository that no longer exists.
3. **Claims corroborated, both ways.** The graph is traversed from `just ci` and from every
   job's own recipe lines; an entry that says a job runs a gate which the job does not reach
   fails, and so does a gate a job reaches which no entry claims.
4. **Every exclusion explained.** An entry running locally-only, CI-only or nowhere without a
   `reason` fails.
5. **CI's invocations are all claimed.** *(Added 2026-10-08; see the amendment.)* Every recipe a
   CI job invokes directly is claimed by some entry, whatever that recipe is called. Direction 3
   checks a claimed gate against the jobs; this checks an invoked recipe against the manifest.
   It is what "every `just` invocation in `jobs.rs` is a claim about a gate" had always meant.

The precedent for a completeness check over a list that carries its own justification is
`every_registered_method_is_classified` in `services/api/tests/api/auth.rs`, which enumerates
the routing table and demands each method appear in `PUBLIC` or `PROTECTED`, and whose default
for a method classified by nobody is a failure rather than "public". That is the same shape:
the list is not documentation, it is a set the check is over, and being missing from it is the
error. A second instance of an existing pattern is cheaper to keep than a new one.

**The graph is built by reading the Justfiles textually, not by `just --dump --dump-format
json`.** The dump was tried on paper first and rejected on two grounds. It adds a `serde_json`
dependency and a subprocess per Justfile to `cargo xtask check`, which is CI's `rust` job and
is supposed to stay sub-second and offline; and it does not deliver the edges that matter,
because — see above — the aggregates are bash loops, so the JSON gives real dependency edges
for the third of the graph that was never at risk and nothing for the part that is. Parsing the
headers, dependencies, `mod` declarations, assignments, `for` loops and `just …` invocations
directly is the whole of `gates.rs` (1,595 lines, of which the parser and lexer are a third),
and it models the loops, which is where the truth lives.
The trade made deliberately is that `gates.rs` owns a small, wrong-in-general `just`
interpreter; what it does not have is permission to be silent about a shape it has not seen.

Bodies are read for invocations of `just`, resolving through module syntax (`just proto::lint`
from any file, `just check` relative to the calling file's directory), through `--justfile
<path>` (which is how a module file's own private recipes are reached, and how
`{{ source_file() }}` means "here"), and through loop variables (`for m in {{ rust_modules }}`
expands to one edge per listed module, `for f in apps/*/Justfile; do dirname "$f"; done`
expands by globbing the filesystem the same way `repo::expand` does for Cargo's `members`).
`$(just modules)` and `$(just _platforms)` are resolved by reading those two recipes' globs
rather than running them: running them would make the graph depend on the host, since
`_platforms` answers differently on a macOS machine, and the graph has to be the same graph on
Linux, where this check runs, and on the Mac, where the Apple gates are.

**CI's side is read out of `tools/ci/src/jobs.rs`'s string literals**, since that is where its
recipe lines are. The jobs file's own doc states the property this relies on: "Each is a short
list of `just` recipes — deliberately, because the recipes are what a developer runs. A
pipeline that reimplements them in another language is a second definition of 'correct' and
will drift from the first." It is true of the pipeline today; the invariant is what keeps it
true. Shell variables inside recipe paths (`stage just "apps/$app/platforms/android/check"`)
become wildcards, which is the same shape the manifest spells its patterns in.

*As written, that last paragraph was a claim about one direction and an aspiration about the
other; see the amendment at the end.*

**`templates/` is excluded from the completeness walk**, and said so in the doc comment rather
than left to be inferred: a skeleton's `check` is not wired to anything until it is
instantiated, and the gate that covers instantiation is `check-platform-templates`, which is
itself an entry in the manifest. Ten Justfiles inside `templates/` are therefore unlisted, and
that is the answer, not an oversight.

**A nested checkout is skipped by `repo::find_files`.** A git worktree — which the multi-agent
workflow under `.claude/worktrees/` makes routinely, and which `git worktree list` still
reports after its branch is merged — is a second complete copy of this repository whose root
is somewhere else. With one checked out, `cargo xtask check` failed 53 invariants, all of them
about the copy, and `gates.rs` reported duplicate recipe ids for a repository that has none.
The general rule (`path.join(".git").exists()`) was chosen over naming `.claude/worktrees`,
because a vendored clone is the same lie.

## The five instances, and where each ended up

| Gate | Was | Now |
| --- | --- | --- |
| `apps/*/platforms/android/check` — `gradlew :app:lintDebug` | ran nowhere | wired into the `android` job; entry records that platform projects are outside `rust_modules` on purpose |
| `apps/*/platforms/apple/{check,build,test}` — swiftlint, xcodebuild | ran nowhere | entry with `local = false`, `ci = []`: Xcode is macOS-only and the pipeline is Linux containers, so no job *can* run it; who runs it on a Mac is RT-4 |
| `i18n::check-data` | `just ci` only, since ADR 0021 | stays local, and now runs in the `proto` job — the one that already has the network excuse |
| `typos`, `deny` | local, and in the `supply_chain` job, invisible to the aggregate story | entry: whole-tree checks whose result depends on the date as much as the commit |
| `proto::breaking`, `check-template`, `check-platform-templates` | CI jobs running gates the Justfiles never mentioned | entries; `check-template` is `terminal`, because past it lies a generated repository whose gates are not this one's |

The Android wiring went into the `android` job rather than into `just check`, which is what
makes that job's existence honest: `apps/counter/Justfile` promises that "the default recipes
cover the Rust side only: they are fast and work on any machine with a Rust toolchain".
Whether the `android` job belongs on every push or on a schedule is RT-8's measurement, and
this change takes neither position — the manifest says the gate runs in that job, not how often
the job runs. `lintDebug` was verified passing (0 errors, 23 warnings) in the ADR 0018 work, so
the stage being added is known to be green rather than hoped to be.

## Consequences

`cargo xtask check` went from **304 to 402** invariants, measured by reverting one file at a
time and re-running rather than by arithmetic. Reverting only `tools/ci/src/jobs.rs` — keeping
the manifest and dropping the two recipe lines it gained — fails exactly two checks and names
both gates and both jobs. That is the demonstration that the CI direction is load-bearing
rather than decorative.

`just ci`'s wall clock is unchanged: the manifest adds 98 assertions to a check that already
runs in well under a second, and none of them touch the network, the toolchain or a subprocess.

Adding a gate is now a two-place change, and the second place is what makes the first visible.
Adding an *app* is not a change to `gates.toml` at all, which is what `*` is for; the invariant
`check-template` runs generates `ledger`, `billing` and `storefront` and reads this same file
there, so a manifest that quietly depended on the example app's name would fail in that job.

`gates.rs` asserts that every `_platforms` recipe still names both `apple` and `macos`, because
the one edge the graph refuses to follow — nothing crosses into `platforms/apple`, since a
Linux build of a Swift project is not a thing — is implemented by filtering edges into that
path. That filter is only correct while platform discovery still works the way it did when the
filter was written; if `_platforms` stops listing the Apple project, the skip becomes a silent
hole of exactly the kind this ADR exists to close, and the guard makes it a failure instead.

What this does **not** cover, stated because the four directions do not cover everything:

- **A gate that was never written as a recipe.** Completeness is over recipe names, so
  `proto::lint` — whose body is inlined into `proto::check` at `just/proto.just:40` rather than
  being a dependency of it — is not in the set and is not missed by anything. Substance inlined
  into another recipe's body remains invisible to a check over recipe edges. The alternative was
  to demand `proto::check` depend on `proto::lint`; that is a Justfile change with its own
  costs and is not taken here.
- **The eleven `lib::check-*` helpers** (`check-tool`, `check-file`, `check-rust`,
  `check-rust-target`, `check-rust-toolchain`, `check-android`, `check-apple`, `check-boltffi`,
  `check-crux-core`, `check-web-trunk`, `ensure-rust-targets`) are not named `check`, so they
  are not gates and need no exemption. That is not luck: it is why the gate set is defined by
  exact name rather than by prefix.
- **`just` semantics beyond the shapes listed.** No conditional recipes, no dotenv, no exports,
  no `[private]` handling, no `[macos]`-conditional edges (a recipe defined twice under
  `[unix]` and `[windows]` keeps the first definition and says nothing about the second, so the
  graph stays host-independent by construction). Unknown invocation shapes are reported as
  problems rather than skipped, which is the property that makes the rest of this list safe.

## Rejected

**Heuristic reachability with an opt-out list** (RT-11's own sketch): enumerate recipes whose
names look like gates, assert each is reachable from `just ci` or named in `jobs.rs`, keep
exceptions. Two findings kill the naive form. A prefix match over `check` invents eleven gates
out of the `lib::check-*` helpers, all of which are `doctor`'s internals, so the opt-out list
outnumbers the findings before it starts. And `proto::lint` is *substantively* covered and
*name-wise* not, so its opt-out entry has to read "its body is duplicated inside
`proto::check`" — an unverified claim about a recipe body, which is the defect shape this ticket
exists to stop. A heuristic that needs prose exceptions to be correct is a heuristic that drifts
toward the exceptions.

**Making `apps/*/check` recurse into `platforms/*`** fixes Gradle lint and swiftlint and nothing
else — not `typos`, not `deny`, not `check-data`, not the comment that was wrong. It also breaks
`just check` on any machine without an Android SDK or swiftlint, contradicting the contract the
app's own Justfile states, and it leaves the false comment standing, which is the actual bug.
It is the obvious first idea, which is why it is recorded here.

**Asserting direction B flatly** — that `just ci` runs everything CI runs — was tried against
the repository before it was designed. Six gate recipes are in CI and not in `just ci` (the
three Android ones, `deny`, `typos`, `proto::breaking`), and beyond the gate set so are
`check-template` and `check-platform-templates`; almost all of them are correct — Dagger, the
network, minutes of wall clock, a macOS toolchain. The
finding was that the exclusion is normal and the *reason* has to be first-class, or a check
with a hundred correct exceptions is read as noise and then ignored, which returns the
repository to five gates running nowhere with more paperwork. Hence `reason`, required exactly
when a gate does not run in both places.

**A Dagger job of its own for `i18n::check-data`**: a job is a schedule, a runner and a cache
policy; one recipe does not outweigh that when a networked job that already pulled four remote (all local since RT-50)
buf plugins was sitting there. See ADR 0021, whose named gap this closes.

## Amendment, 2026-10-08: the fifth direction (RT-14)

**What was unenforced.** This record says the manifest is checked in "either direction", and
the module doc, `CI_JOBS_PATH`'s own comment and `gates.toml`'s header each close their list of
failures with "a CI invocation nobody has thought about yet". Four of those were enforced. The
fifth was not: `require_gates_claimed` demands an entry of every recipe whose *name* is
`check`, `build` or `test`, so a job could invoke any recipe not named exactly one of those and
no entry was ever asked for. `check-template`, `check-platform-templates`, `check-archetype-templates`,
`typos`, `deny`, `proto::breaking`, `i18n::check-data` and `apps/*/generate-android` are all in
that class, and every one of them had an entry because whoever wrote the line chose to write
one. A *wrong* entry already failed, in both directions, since direction 3.

**How it was found.** Not by this ADR. RT-12's plan asserted that a gate recipe added to the
proto job without its manifest entry would fail this invariant; the agent that implemented RT-12
measured it and reported the plan wrong — 407 checks, no problem raised — and that correction
block on RT-12 is the primary record. RT-14 was filed from it.

**What now fails.** `require_ci_seeds_claimed` in `tools/xtask/src/gates.rs`: for every job in
`jobs.rs`, every recipe that job invokes directly and resolves to must be claimed by an entry.
The seeds were already being resolved and thrown away — `ci_reaches` folded them into `closure`
and returned only the closure — so the change is to keep them, and `ci_seeds` is the function
that resolves them now. Gate-shaped seeds stay with `require_gates_claimed`, which reports them
in better words; one defect, one message.

**The measured cost.** 409 invariants to **417** — one per CI invocation whose claim this
direction is the only one to assert. That is eight of the fifteen invocations across the five
jobs, not fifteen: the other seven are gate-shaped, and `require_gates_claimed` already counts
and reports those by name. Seven rather than the three a first reading suggests, because
`is_gate` is read off the bare recipe name, so `proto::check` and the Android project's `check`,
`build` and `test` are gates whose ids merely carry a module path. Deleting `check-archetype-templates`' entry from `gates.toml` — the
case that passed at 407 — fails naming the `proto` job and that recipe. Deleting
`check-template`'s fails too, naming the `template` job: `terminal` affects the closure, not the
seeds, so a terminal recipe is still an invocation somebody thought about.

**What the manifest is now responsible for.** Nothing new was claimed. What changed is that the
claim has teeth at both ends: `gates.toml` is the list of where every gate-shaped recipe runs
*and* the list of everything CI runs, and a `just` line added to a job now costs an entry —
which is the same rule that already applied when a recipe was added to a Justfile, and is the
reason the two lists cannot drift apart by omission any more.

**What it still does not do.** It demands the *direct* invocations, not each job's transitive
closure. Requiring the closure would be the compute-it-from-reachability answer this record
rejected above, and it would ask for entries for hundreds of recipes a job merely reaches. A
recipe a job reaches indirectly and no entry claims is still invisible to direction 5 exactly as
it was; what is no longer invisible is the line someone typed into `jobs.rs`.

## Amendment, 2026-10-08: the two wired gates are watched failing (RT-8, RT-10)

This record was written on the principle that wiring is not verification, and its own table
says the android lint stage was "verified passing (0 errors, 23 warnings)… known to be green
rather than hoped to be". Green was never the test. RT-8 and RT-10 committed breaks and
watched the jobs, and three things came of it.

**RT-10: `just i18n::check-data` fails the proto job.** The break was one string literal in
the committed payload — `crates/i18n-data/src/data/datetime_names_month_gregorian_v1.rs.data`,
"January" to "Junuary". Length-preserving, one word: the locale list is still `[en, hr]`, no
marker moved, `cargo build -p i18n-data` passes and `cargo test -p i18n --features format`
passes — the stale payload is perfectly usable, which is exactly the failure mode the locale
invariant cannot see. The proto job passed `proto::check` and `proto::breaking` and died at
its third command, `withExec devenv shell -- sh -c 'just i18n::check-data'`, exit 1: "the
proto job failed; the failing command is named above" (`tools/ci/src/jobs.rs:57`). Dagger's
error carries none of a step's output and the live stream cut off mid-step, so the diff line —
`✗ crates/i18n-data/src/data is not what the generator produces`, `datetime_names_month_
gregorian_v1.rs.data differ` — is worth recording here for triage; the same recipe on the same
tree prints it locally and exits 1.

**RT-8: the android job fails on a hand-written plurals, and the stage needed repair first.**
The break was a Croatian plurals family that does not define `few`, in
`app/src/main/res/values-hr/strings.xml`:

```
> Task :app:lintDebug FAILED
/src/apps/counter/platforms/android/app/src/main/res/values-hr/strings.xml:4: Error:
For locale "hr" (Croatian) the following quantity should also be defined: `few`
(e.g. "2 dana") [MissingQuantity]
BUILD FAILED
```

and the job with it, "the android job failed; the failing command is named above"
(`tools/ci/src/jobs.rs:129`). No cheaper gate sees that break at all: `cargo xtask check`
stayed at 53 versions and 417 invariants with it in the tree, the `i18n-keygen` fixture tests
pass (they never read platform XML), `assembleDebug` passes, `testDebugUnitTest` passes.

Two things around that came out wrong in the ticket and one in the recipe. The break as
RT-8 first named it — a plural quantity removed from the catalog TOML — cannot reach an
android job: `plurals_match_cldr`, a `cargo xtask check` invariant, fails it in the `rust`
job, cheaper and earlier, and should. Lint's coverage is resources: hand-written app
resources, which nothing else in the repository reads, and generated resources behind whatever
the fixture tests fail to pin. And the recipe was repaired, as follows.

**`:app:lintDebug` cannot fail on a generated resource.** It *analysed* the library's
resources — the finding landed in `shared/build/intermediates/lint_partial_results/.../
lint-definite.xml` — but its verdict covers the app module's own sources, and
stripping `few` from the generated `values-hr/strings_i18n.xml`, precisely the half the stage
was added to protect, left `just apps/counter/platforms/android/check` exiting 0.
`:shared:lintDebug` fails on the same tree, on the same line, naming the generated file. The
recipe is now `./gradlew --no-daemon :app:lintDebug :shared:lintDebug`, in the app's platform
Justfile and in the skeleton together, because a stage that only fails in the app it was
tested against is a stage that fails in one app.

**Severities, measured rather than looked up** (the lint shipped by AGP 8.13.2, in-repo,
with no `lint {}` block, so defaults are what runs): MissingQuantity is **Error**,
ImpliedQuantity is **Error**, UnusedQuantity is **Warning**. Only the first two can fail the
job. The warning budget is 23 in the app and 1 in `shared` (`UnsafeDynamicallyLoadedCode`,
boltffi's loader); `warningsAsErrors` stays off and needs no change for any of the three
named checks — making UnusedQuantity an error would be an options-block decision and its own
ticket.

**A PluralsDetector fact worth knowing before writing the next probe**: all three checks skip
unqualified `values/` files entirely — `visitElement` returns when the file carries no locale —
so a plurals family present only in `values/` can never fire MissingQuantity, ImpliedQuantity
or UnusedQuantity; and ImpliedQuantity fires only for `zero`, `one` and `two` in locales whose
rules match more than one number for them (`few` and `many` are exempt by construction). A
first probe that put all three shapes in `values/` reported nothing at all, and exited 0.

**Measured, and the cadence.** Warm caches, container, one app: the lint stage (both modules)
is ~39 s; the android job is 36 s without the stage and 68 s with it. Locally warm the stage
is 21 s. Cold gradle in a container ran ~5.5 min, almost all of it gradle distribution
download. The decision is **every push**: under a minute of marginal cost against a failure
mode that ships a broken catalog to every device, and a fresh CI checkout earns none of the
warm numbers anyway — the dagger engine here returned a byte-identical second run in 2 s,
which is a preview of nothing real. The `gates.toml` entry carries the numbers now.

## Amendment, 2026-11-04: the Nix pipeline runs alongside Dagger

A second CI pipeline (`.forgejo/workflows/ci-nix.yaml`, `nix/ci.nix`) now runs on the `nix`
label alongside the Dagger one. The Nix pipeline splits the jobs into pure crane
derivations — one `devenv build outputs.ci-<stage>` per gate — and runs the stages that
still need the network, `.git` history or the Android SDK (`proto::breaking`,
`i18n::check-data`, `deny`, `check-template`, `check-platform-templates`, the Android
steps) as host steps with `devenv shell -- just <recipe>`.

`proto::check` and `check-archetype-templates` started as host steps too — codegen
needed five remote buf plugins — and became derivations when #66 made every plugin local:
sandboxed `buf generate` is offline now, and a store hit on ncps replaces the codegen a
host step re-ran on every runner.

**The manifest now tracks both pipelines.** `gates.rs` has a `require_nix_workflow_claimed`
function that parses `.forgejo/workflows/ci-nix.yaml` and asserts two things:

1. Every derivation stage listed in a static mapping (`DERIVATION_GATES`) must appear in
   the workflow. The mapping records which `just` recipe each stage covers
   (`ci-fmt` → `rust::fmt-check`, `ci-clippy` → `rust::clippy`, `ci-build` → `rust::build`,
   `ci-test` → `rust::test`, `ci-trunk-build` → the web shell's `build`,
   `ci-proto-check` → `proto::check`, `ci-archetypes` → `check-archetype-templates`,
   `ci-typos` → `typos`). `ci-xtask-check` has no corresponding `just` recipe (it runs
   `cargo xtask check`, which the Dagger pipeline invokes as a bare cargo command). The
   acceptance test: deleting any single derivation step from `ci-nix.yaml` fails
   `cargo xtask check`, naming the missing stage and the gate it covers.

2. Every `devenv shell -- just <recipe>` line must be claimed by a `gates.toml` entry, the
   same rule that already applied to the Dagger pipeline's invocations in `jobs.rs`. The
   host steps run the same recipes the Dagger jobs do, so the existing entries already
   cover them.

**The Dagger pipeline is still tracked through `jobs.rs`.** When it is retired and
`tools/ci/` is deleted, the `ci_jobs` functions will be removed and the Nix workflow check
will be the sole CI completeness assertion.

**`just pipeline` is unchanged** and still drives the Dagger pipeline. The new local entry
point is `devenv tasks run ci:run`, which builds the derivation stages and runs the host
steps in sequence: one command reproduces what the PR runs.

## Amendment, 2026-11-04: the Dagger pipeline is retired, and a derivation is coverage

[ADR 0024](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0024-the-pipeline-is-derivations-and-a-shell.md) replaced the pipeline this record was written against. `tools/ci` is gone, so the
half of the invariant that read CI's invocations out of `jobs.rs`'s string literals has nothing
to read, and the amendment above that promised "the `ci_jobs` functions will be removed and the
Nix workflow check will be the sole CI completeness assertion" is what happened.

**The workflows are the only CI entry point.** `gates.rs` parses `.forgejo/workflows/ci.yaml`
and `.forgejo/workflows/advisories.yaml`, and a third workflow would have to be added to
`CI_WORKFLOWS` — a workflow nobody parses is a hole in the manifest that reads exactly like
coverage, which is the failure this record exists to make impossible. The `ci` fields in
`gates.toml` now name workflow jobs (`ci-pure`, `ci-impure`, `ci-android`, `advisories`) rather
than `pub async fn` names.

**A job reaches a recipe two ways, and the second needed a table.** A `devenv shell -- just
<recipe>` step is an invocation in exactly the sense a Justfile body's is, and goes through the
same lexer. A `devenv build outputs.ci-<stage>` step invokes nothing: the stage runs cargo, or
buf, or trunk, or datagen, and no recipe appears anywhere in the graph. `DERIVATION_GATES` is
the connection, mapping each stage to the recipes it reproduces — `ci-clippy` to `rust::clippy`,
`ci-trunk-build` to the web shell's `build`, `ci-android-generated` to `generate-android`. A
stage with no entry is a gate nobody runs and fails the check; a stage no step builds fails it
too, which is the direction that makes deleting a step a finding rather than a quiet reduction.

**The five directions are unchanged, and the promise still holds.** A gate no entry claims, an
entry matching no recipe, a claim the graph disagrees with in either direction, an unexplained
exclusion, and an invocation nobody thought about — all five still fail, and the last one now
covers both kinds of step.

**What the manifest lost, stated because it is a real reduction.** The aggregates `check`,
`build` and `test`, and the per-module dispatch under them, are now `local = true, ci = []`:
no CI job invokes them, because the derivations run the cargo commands they dispatch to once
over the whole workspace instead. The entries say so in their `reason`, and the claim about
CI's Rust coverage moved to the leaves — `rust::fmt-check`, `rust::clippy`, `rust::build`,
`rust::test` — which is where `DERIVATION_GATES` points. Nothing now asserts that `ci-clippy`'s
flags still match what `apps/<app>/check` would have run; that agreement is a comment in
`nix/ci.nix` and a reviewer's job, where before it was the graph's. The alternative was mapping
the stages onto the aggregates and letting the closure imply the leaves, which would have made
the manifest claim that CI runs `just check` when it does not, and a claim that reads stronger
than the truth is worse than one that reads weaker.

**The parser learned two shapes it did not have.** YAML block scalars, because a step that
extracts a store path is multi-line and a parser reading only single-line `run:` values silently
claims that step runs nothing; and arguments after a recipe name, because the nightly runs
`just deny --fix` and `just check-template yes`, and a naive split reads the recipe as
`deny --fix` — a name that resolves to nothing, reported as a rename that never happened. Both
were found by the check failing on a real workflow rather than by reading the parser, which is
the argument for having it.
