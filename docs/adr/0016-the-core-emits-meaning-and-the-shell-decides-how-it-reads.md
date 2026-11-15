# 0016. The core emits meaning, and the shell decides how it reads

- **Status:** Accepted
- **Date:** 2026-10-01

## Context

ADR 0014 put the translations in a crate and stopped at the core's door, leaving
`ViewModel { count, status }` as two `String`s written in English by `view()` —
`format!("Count is: {}", …)`, `"working…"`, and a clone of whatever the shell's client had
said about a failed request.
It deferred the rest with a sentence and a rule: *"it comes back as stage 2 with its own
ADR, where the recommended shape is the core emitting keys"*, and *"if a port of this one
ever touches the core, `ffi.rs`, or a generated mobile file, it has become the other
change"*. This is that change, and the stage rule is discharged by it: the view model
crosses the FFI as bincode into generated Kotlin and Swift, so `apps/counter/core`, five
shells, two platform skeletons and the crux-app skeleton all move together.

ADR 0015 made the gap visible rather than theoretical. Detection reached the three Rust
shells, so `LANG=hr_HR.UTF-8` renders Poništi/Povećaj/Smanji *and* `Count is: 0` on the
same screen — 0015 predicted exactly that and called it an argument for doing this, not for
delaying it.

Three open items in the i18n catalogue turned out to be one blocker wearing three hats:

- `translate_with!` is generated, unit-tested against string literals, and **no locale file
  in the repository contained a `%{` argument** — machinery that has never had input;
- `plural_families` and `PLURAL_CATEGORIES` exist in the resource emitter, and the catalogue
  contains **no plural key at all** — the same, one stage further along;
- number and date formatting is absent everywhere.

None of the three is reachable while `view()` hands over `count: String` with the sentence
already applied. Nothing downstream can interpolate, pluralise or format a sentence that
arrived finished, and `for_app`'s lesson applies: a seam that cannot be demonstrated on the
real tree is not a seam.

## Decision

**`ViewModel` carries values. The core names its own domain vocabulary. The shell decides
how both read.**

```rust
pub struct ViewModel {
    pub count: i64,
    /// `None` when there is nothing to say.
    pub status: Option<Status>,
}

pub enum Status {
    /// A request is outstanding.
    Working,
    /// The last request failed. `message` is a sentence written above the core by the
    /// shell's client layer — data, not a key, and not this core's to translate.
    Failed { message: String },
}
```

- **`view()` keeps its `match (&model.error, model.pending)`.** Deciding *which* state this
  is — an error outranks a spinner, a spinner outranks silence — is domain logic and stays
  in the core with the `pending` flag it reads. What left are the three string literals that
  arm produced. The line count moved almost nowhere; what moved is which side of the
  boundary the English is on.
- **`counter-core` does not depend on `crates/i18n` and does not name a key.** It emits
  `Status::Working`, and each shell maps that to its own presentation. The dependency
  direction is one-way and it is the interesting one: `crates/i18n` still does not know the
  counter exists, which is what lets a worker link it for one log line (ADR 0014).
- **The sentence became a catalogue key with an argument:** `count_is = "Count is:
  %{count}"` and `"Broj je: %{count}"`. `translate_with!(Key::CountIs, locale, count = n)`
  is the call. It works, and that was checked rather than assumed — the macro takes
  `$key:expr` and expands to `translate($key, $locale)`, `translate` takes `impl AsRef<str>`,
  and the generated `Key` implements it. Its doctest names a string literal only because a
  doctest cannot name a generated type; reading that as a restriction of the macro is a
  mistake this record is partly written to prevent.
- **`count_is` and `working` are `common/` keys, not `counter.*`.** The plan for this change
  put the sentence under `counter.`, and that is wrong in a way only the *scaffolded* app
  shows: `resources::for_app` gives a platform project its own namespace plus the bare keys
  and nothing else, so a skeleton shell that named `counter.count_is` would compile against
  an Android resource (`R.string.counter_count_is`) that is never emitted for an app called
  `payments`. The sentence is said by the example app *and* by every app the crux-app
  skeleton makes, which is what "belongs to no single page" means. Putting it in
  `counter.*` **and** in `common/` would have been the other mistake — the example app's own
  `strings.xml` carries the union, so it would hold the same sentence twice.
- **The mapping is written per shell, and each copy is compiler-checked.**

  | shell | maps with | checked by |
  |---|---|---|
  | tui, desktop | `match` on `Status` → `Key`, then `translate` / `translate_with!` | match exhaustiveness, generated `Key` |
  | web | the same, through `tr` / `translate_with!` | as above |
  | android | `when` over the generated `sealed interface Status` → `R.string.*` | `when` as an expression, generated resources |
  | apple | `switch` over the generated enum → typed symbol | `switch` exhaustiveness (in principle — see Consequences) |

- **The status line is deliberately hybrid, and its second half is not the server's.**
  `model.error` holds a sentence written *above this core, in this repository*: a failed
  request reaches `counter-client::describe`, which turns a transport or Connect failure into
  one line — `"cannot reach the API at {}", base_url` — or tidies the backend's Connect error
  text and hands that instead. So `Status::Failed { message }` is a passthrough rendered
  verbatim beside `Status::Working`, which is a lookup, and the temptation to translate it is
  real rather than absurd: the author is a crate in this workspace and the sentence is
  English. It stays a string because the view model cannot say *which* producer wrote it —
  this client's transport line, or whatever the backend chose to send — and giving up on
  that means turning failure **kinds** into a domain enum on `CounterApiResult`, which is a
  change to the error model rather than to the view model. What was refused here is the
  shortcut of a catalog key for somebody else's sentence; what this change asks for, when a
  translated failure line is actually wanted, is that error-model change, and it is now a
  well-defined one.
  **It was done afterwards, and it is not this one.** `Failure::Unreachable { endpoint }` and
  `Failure::Rejected { code, detail }`, with a `Code`, now sit on `CounterApiResult::Failed` in
  `apps/counter/core/src/api.rs`, and `Status::Failed` carries a `Failure`. The line is still
  hybrid, in the other order: a looked-up sentence per kind and per code, with `detail` — the
  server's own words — still untranslated after it. What that change added to every shell is a
  second `match` underneath the first, which is what "two to eight lines" in the Consequences
  under-priced.
- **`Option<Status>` rather than a `Status::Idle` variant.** "Nothing to say" is an absence,
  not a state the counter is in, and `Option` is the one type the FFI already knows how to
  say: measured against the typegen for this very change, it arrives as `Status?` with a
  `null` default in Kotlin and `Status?` in Swift, so the shells get their nil-check free
  instead of a `_ => {}` arm that would swallow a future third variant.

## Consequences

**The proof fits on one line and runs on Linux.** `LANG=hr_HR.UTF-8 cargo run -p
counter-tui` renders `Broj je: 0`. ADR 0015's "the chrome becomes Croatian and the body
stays English" is what this removes; the body was the last thing in the example app that
was not.

**Five mappings, and no check.** The same mapping now exists in Rust three times, in Kotlin
and in Swift. That is ADR 0002's cost restated one level up, and it is paid for the same
reason: each copy is between two and eight lines, and each is enforced — `match`/`when`/
`switch` exhaustiveness on one side, a generated `Key` / `R.string` / catalogue symbol on
the other. **No xtask check was added** for "a key the core names exists in the catalogue",
because the core names no keys. `telemetry_guards_are_bound`'s doc comment is the
repository's own precedent for refusing a check that restates the compiler, and nothing
found during this change escaped the type system in a way that needed one. Adding one would
be a new decision, not a completion of this one.

**Formatting and plurals are unblocked, and are not in this change.** A shell holding
`count: i64` and a `Locale` can now format it — `Intl.NumberFormat` in the web shell,
`NumberFormat`/`DateTimeFormatter` in Kotlin, `FormatStyle` in Swift, and a crate
(`icu_decimal` versus lighter separators-only `num-format`) in the two native Rust shells —
and one plural key would finally give `plural_families` input. Both were left out on
purpose: the crate choice deserves its own look, and a counter's values are single digits,
so **locale-aware formatting is unobservable in the example app** — the "cannot be
demonstrated on the real tree" problem again, and it wants its own fixtures. What this
change did do is record the two things already visible about formatting a *number* through
the generated resources: Android gets `%1$s` (the emitter has no type information, so
`%{count}` is a string placeholder everywhere) and it renders a `Long` with no grouping
separators whatever the locale; Apple gets `%1$@`, which is the wrong marker for an integer
in a `String(format:)` call. Neither is a regression — there was no formatting before either
— but the emitter will need typed markers before a number is formatted correctly on a
phone, and that is now a job for the formatting change rather than a surprise.

**Both skeletons changed in this commit, not afterwards.** `templates/crux-app` core had
the same `format!("Count is: {}", …)`, and leaving it means `just new crux-app payments`
scaffolds an app reproducing precisely what this change deletes. Its three shells changed
with it — the tui skeleton gained an `i18n` dependency and locale detection it never had,
which is the same gap `gpui_labels_are_translated` was written for on the desktop side. The
platform skeletons read `core.view.count` too (`templates/platform-android/…/MainActivity.kt`,
`templates/platform-apple/…/ContentView.swift`) and both would have failed to compile
against an `Int64`/`Long`. The Android skeleton's three button labels were hardcoded while
its Rust skeletons were not, which is the same gap one level down; they are `R.string.*`
lookups now, since the catalog emits them for every app. This is the third time a change of this family stopped at the
example app: `b4dfb9b` and `1bd7bf5` are the earlier two, and the lesson is not
"remember the template", it is that `templates/` is compiled by nothing except
`just check-template` and `just check-platform-templates`. Measured, not asserted: that
check failed twice on this change's own skeletons — first on
`clippy::too_long_first_doc_paragraph` in a doc comment added here, then on the skeleton's
`core/tests/example.rs`, which still asserted `count == "Count is: 1"`. A skeleton test went
stale against the exact type this change is about, in a file compiled only inside a
generated product, so `just ci` could not have seen it.

**`generated/` is gitignored.** Typegen output produces no `git status` churn, so a stale
binding is invisible; and its failure mode is a bincode error at launch naming nothing.
`just apps/counter/generate-android` was re-run (typegen *and* `boltffi pack` — half is
worse than none) and the APK was re-built and inspected, not assumed.

**`ViewModel::default()` still seeds the web shell**, and `count: i64` defaulting to `0` is
still a placeholder rather than a reading; the comment that says so survived, now next to a
`status` that defaults to `None`.

**Apple is unverified, as it has been throughout.** The Swift `switch`, the typed symbol
names for the new keys, and `%1$@`-with-an-integer are written against the generated Swift
types (which typegen does emit on Linux, and which were read) and against the String
Catalog emitter, and nothing else. The existing `ContentView.swift` already carries an
`UNVERIFIED` comment about the one dotted key; this adds two keys whose symbols are ordinary
bare names, which is the more likely-to-work case.

## What was rejected, and why

**The core naming `i18n::Key` directly.** ADR 0014's own recommended shape, and strictly
fewer moving parts: the core sends `Key::CountIs` plus its arguments and the shells look it
up without owning a mapping. It lost on four grounds, in decreasing order of how much they
hurt:

1. **The arguments have no type.** `Key::CountIs` is meaningful only with a `count`, and
   `Key` carries no arity, so the payload crosses the FFI as a bag of
   `String → String` — a hand-written wire shape, in a repository whose mobile bindings are
   generated precisely so that nobody hand-writes one. A `Status::Failed { message }` and a
   `CounterApi::Increment { by }` are checked by the compiler on both sides of that boundary;
   a placeholder bag is checked by a runtime string lookup, per key, per locale, in the
   language the least tooling reaches.
2. **It puts the catalogue's vocabulary in the crate that must not have one.** `view()` would
   read `Key::CountIs`, which is a fact about `crates/i18n/locales/`, not a fact about the
   counter. `Status::Working` is the same claim made in the only vocabulary the core owns —
   a request is outstanding — and ADR 0002 exists to keep the core's vocabulary domain-level.
   Translating "Count is:" into "Total:" is then a locale edit rather than a core edit and a
   release.
3. **It does not actually remove the per-shell code.** A shell given a key still has to know
   which placeholders it takes, and still has to decide — for `Failed`, which has no key at
   all — what to render. The mapping survives, minus the type that made it checkable.
4. **It drags `crates/i18n` into every mobile binary** that links the core, including one
   whose user-facing strings come from `strings.xml` and never touch rust-i18n.

   Worth stating because it is the trap: `cores_stay_domain_level` would *not* have stopped
   this. ADR 0014 already noticed — `i18n` is not on that invariant's forbidden list, "which
   is worth noting because an invariant passing is not the same as a design being right".

**Keeping the formatted `ViewModel` and having the shells pattern-match on English to
re-translate it.** Recorded because it is the small diff that gets suggested whenever a
translation change is being scoped: leave `count: String` alone and rewrite the sentences at
the other end. It is worse than either coherent option — two formats, one of them matched on
rather than looked up — and it leaves `translate_with!` with no input, which is the centre of
this change.

**A `Status::Idle` variant instead of `Option`.** One enum on the wire, and "idle" would be
a state with a name. Rejected because it is a lie about the domain (the counter is not idle;
there is nothing to report), and because it makes the shells' default branch a real variant
rather than `null` — a `_ => {}` there is where a future variant goes silently missing, which
is exactly the failure `Core.kt`'s assigned-`when` exists to prevent.

**Doing number formatting, or plurals, in this change.** Both are unblocked by it and both
were refused: the crate decision has two credible answers and needs a fixture with four
digits in it, and a plural needs two keys. A change that makes formatting *possible* is a
reviewable change; one that also picks a formatter, in five shells, on copy that cannot show
it working, is not.

**A check that the core's keys exist in the catalogue.** Refused above, under
Consequences: there is nothing to check, since the core names no keys, and every mapping in
the table is enforced by a compiler on both sides.
