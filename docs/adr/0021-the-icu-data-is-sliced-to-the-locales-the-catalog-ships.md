# 0021. ICU data is sliced to the locales the catalog ships

- **Status:** Accepted
- **Date:** 2026-10-05

## Context

ADR 0020 measured what shipping a formatted date cost: **+314,279 bytes on the web shell's
`.wasm` (874,940 → 1,189,219, +35.9%)**, and it named three ways to pay less — narrow the
`icu_datetime` surface, swap the web shell alone to `Intl.DateTimeFormat`, or don't show
dates on web. The repository took none of them, and the reasoning still stands: all three
change *what gets formatted*, and the answer to that was already given in ADR 0016, 0017 and
0018. None of the three was about the other axis of the same bill, which nobody had looked
at: **which locales the payload covers**.

`crates/i18n/locales` ships two — `en` and `hr`. `icu_datetime`, `icu_decimal`,
`icu_calendar` and `icu_time` with their default features carry `compiled_data`, which is
ICU4X's own whole-CLDR blob: every locale Unicode publishes, in the binary, whether or not
anything asks for it. The catalog asks for two. That is the shape of the thing — not a
formatter that could be smaller, but a library that had already shipped ~500 locales to
answer a question about two — and the fix does not touch a single formatting decision, a
single call site, or a single rendered string.

The slice is `icu4x-datagen`, ICU4X's own generator, run over the marker set this crate
actually asks for and the locale list the catalog actually ships, emitting a Rust module
(`--format baked`) that is committed as `crates/i18n-data/src/data`. The `format` feature
turns `compiled_data` off on every ICU crate whose data this crate ships, and
`src/format.rs` builds its formatters with `try_new_unstable(&CLDR, …)` against that
provider. Nothing else about how the formatters are constructed changed.

## Measured first, decided second

The same three artifacts as ADR 0017, 0018 and 0020, x86_64-linux release builds. Base is
`d2c48f3` (before any ICU formatting), unsliced is `afd1b50` (ADR 0020 as merged), sliced is
this commit:

| artifact | base | unsliced (ADR 0020) | sliced | vs unsliced | vs base |
|---|---|---|---|---|---|
| `counter-tui` release | 5,232,992 B | 5,761,880 B | **5,450,000 B** | **−311,880** | +217,008 |
| `counter-tui` release, stripped | 3,657,304 B | 4,130,408 B | **3,817,232 B** | **−313,176** | +159,928 |
| `counter-web` `.wasm` — what a browser downloads | 874,940 B | 1,189,219 B | **958,275 B** | **−230,944** | +83,335 |

**Of ADR 0020's +314,279 bytes on the browser artifact, 230,944 were CLDR for locales nobody
ships** — 73% of the growth, recovered by one command and a directory.

The residual is the honest part of the table, so it is worth stating where the remaining
bytes are. The payload itself is small and countable, and the generator writes the numbers
into each file's own doc comment: the nine markers together are **2,273 B of lookup tries
across 403 data identifiers plus 19,676 B of data structures — about 22 KB**. The generated
module is 105,250 bytes of Rust text (124 KB in blocks). So the +83,335 B left on the wasm
artifact over base is not data: it is `icu_datetime`'s code — pattern parsing, the packed-
pattern runtime, `zerovec`, `fixed_decimal` — which is what ADR 0017's and 0020's tables were
also paying and never separated out. If a future change needs the web artifact smaller
again, **the locales are not where the next byte is**; the formatter machinery is.

Verification that the blob is really gone, rather than merely unreferenced: `strings` on the
sliced `counter-tui` binary and the sliced `.wasm` finds no `Freitag`, `janvier`, `octubre`
or `dezember`; the unsliced pair from ADR 0020 finds all four.

`Cargo.lock` moves **1,172 → 1,168 packages (−4)**, which is the whole feature turning off
in text: `+i18n-data` (this crate), and `−icu_calendar_data`, `−icu_datetime_data`,
`−icu_decimal_data`, `−icu_time_data`, `−ixdtf` — the last of which is ICU4X's RFC 9551
date-time parser, which existed in the graph only because `icu_time`'s compiled data needed
it. A whole crate leaves the binary because the data that pulled it went.

Adding a locale now has a price, and the price is small and known: regenerating with `de`
added to the list takes the payload from 21,949 B to 24,576 B (**+2,628**), the generated
text from 105,250 B to 112,841 B (**+7,591**), and the identifier count from 403 to 452.

## Decision

**`default-features = false` on every ICU crate whose data this crate ships** —
`icu_datetime`, `icu_decimal`, `icu_calendar` in `crates/i18n`, and the same set in the new
`crates/i18n-data` — and a generated baked provider passed explicitly to the
`try_new_unstable` constructors. Off rather than on-with-a-lucky-override because the failure
modes differ: with `compiled_data` on, a build that loses its slice silently regains ~300 KB
and nothing fails; with it off, only the explicit-provider constructors exist, so a marker
the payload does not carry is a **compile error in `src/format.rs`**. That asymmetry is the
load-bearing part of the whole design and the reason `ICU4X_DATA_DIR` is rejected below.

**The payload is committed**, at `crates/i18n-data/src/data` (a `mod.rs` and nine
`*.rs.data` files), exactly as ADR 0004 commits generated protobuf: a plain `cargo build`
needs no generator, no CLDR download and no network. There is no `build.rs`. Regenerating is
`just i18n::generate-data`, checked by `just i18n::check-data`, which sits in
`just check-repo` beside `proto::check` because generated artifacts must match their
generators. CI does not run that recipe, and Consequences says what that leaves open.

**The locale list is never typed.** The recipe derives it from the file stems under
`crates/i18n/locales`, which is exactly what `i18n-keygen` turns into `Locale::ALL` — so the
generator and the catalog cannot be handed different lists, and there is no second place to
forget a locale.

**Two versions are pinned in `versions.toml`, and both are checked**: `tools.icu4x-datagen`
(the tool, packaged in the dev shell by `nix/icu4x-datagen.nix`) and `tools.cldr-json` (the
CLDR release the slice is cut from, passed as `--cldr-tag`; the tool's default is `latest`,
which floats). `cargo xtask check` asserts the datagen pin's `major.minor` against every
`icu_*` requirement in every workspace manifest — twelve of them today. Moving the tool pin
alone also stops the dev shell from evaluating at all, since Nix builds the tool from that
same number, which was observed while testing the check rather than imagined.

**The provider stays private.** `format.rs` holds `const CLDR: i18n_data::Cldr =
i18n_data::CLDR;` and no one else gets to choose which CLDR a shell loads. `i18n-data` is
not a public API; it is this crate's data segment with a directory of its own.

## Consequences

**Three gates, because the failure has three independent shapes.** Each was watched to fail.

1. **`just i18n::check-data` — the payload is current.** It regenerates into a temporary
   directory and diffs against the committed tree, mirroring `proto::check`'s compare
   rather than ask-git rule (a git-based check passes silently before the tree is first
   committed, and after a `git rm`). The recipe also asserts that its own nine
   `--markers` flags and the nine `impl_*!(Cldr, ITER)` calls in `crates/i18n-data/src/lib.rs`
   have the same length, because a marker generated and not implemented is dead weight with
   nothing comparing the two lists. Failing direction: delete a generated file — reported.
2. **`the_icu_data_slice_covers_the_locales_the_catalog_ships`, in `crates/i18n` — the
   payload and the catalog agree, both ways.** Forward: every `Locale::ALL` member must be
   present in the data, where "present in the data" is read from the provider's own tries
   (`i18n_data::locale_codes()` via each marker's `IterableDataProvider` half), so the two
   sides of the comparison come from two generators over two inputs. Backward: every
   identifier in the payload must fall back — using the fallbacker the payload itself
   carries — to a locale the catalog ships. `und` is exempt in both directions.
   **The backward direction is deliberately not set equality**, and writing it as equality
   would encode a wrong idea of what a slice is: asking for `en hr` yields some 120 more
   identifiers, because CLDR keeps English apart by region (`en-001`, `en-AU`, …), lists the
   locales that inherit from what was asked for (`hr-BA`), and includes locales like
   `hi-Latn`, whose chain runs `hi-Latn → en-IN → en-001 → en`. The fallback rule admits all
   of that and rejects `de`, which falls to `und` and nowhere else. Negative-tested twice:
   adding `locales/common/de.toml` fails it with `shipped but not in the slice: ["de"]`;
   generating `--locales en hr de` fails it with six `de-*` identifiers named.
3. **`cargo xtask check` — the slice cannot be silently relinked whole.** Cargo unifies
   features across the whole graph, so one ordinary `icu_datetime = "2.3"` anywhere re-links
   all of CLDR into every binary containing `i18n`, leaves the committed payload untouched
   and passing, and every formatter still returns `Ok`. `icu_<crate>_data` appears in
   `Cargo.lock` if and only if something resolved that crate's `compiled_data` feature, so
   the lockfile is a witness that costs nothing to read, and
   `cldr_data_stays_sliced` holds an allowlist of four tolerated data crates with a reason
   each: `icu_locale_fallback_data` (the parent-locale rules, which `i18n-data` turns on
   deliberately and are the same size whatever the locale count), `icu_normalizer_data` and
   `icu_properties_data` (`idna`'s character tables, in every binary since before `i18n`
   existed), and `icu_plurals_data` (`i18n-keygen`'s build-time plurals, which run on the
   host and never reach a shipped binary). Negative-tested by injecting an
   `icu_datetime_data` package into `Cargo.lock`.

   **Checked counts, attributed rather than totalled:** `cargo xtask check` moves from **41
   version checks to 53** (one per `icu_*` requirement across the workspace's manifests, which
   is also the number the negative test printed) and from **294 invariants to 304**: **+5** for
   the lockfile check (one for the lock, one per tolerated data crate — attributed by
   unregistering it and watching 302 become 297), **+1** for the new member manifest in the
   lint-inheritance pass, **+1** for the new Justfile module and **+1** for the new Nix module,
   both of which count per item, and **+2** for this record and its row in the index, which
   `decision_records_are_indexed` counts one per record file and one per link. That last pair
   is why the numbers in this ADR are not reproducible in the naive way: **the act of writing
   the record moves the counter it quotes.**

**`crates/i18n-data` is the one crate in this workspace that does not inherit
`[workspace.lints]`, and that is a Cargo rule rather than a preference.** The workspace lints
set `unsafe_code = forbid`. The generated payload is databake output, and databake
materialises a payload it validated at generation time with `from_bytes_unchecked` — 174
`unsafe` blocks across the nine files. Cargo refuses to let a member inherit
`[workspace.lints]` and override a lint from it in the same manifest, and an in-source
`#[allow(unsafe_code)]` cannot lift a `forbid`. There is no configuration in which this
generated code and this workspace's lint set share a crate.

So the exception has a directory, and the directory holds no hand-written code: `i18n-data`
declares `[lints.rust] unsafe_code = "allow"` and nothing else, and `format.rs` — the code a
person edits — stays under the workspace set. `members_inherit_workspace_lints` was widened
instead of waived: an exempt crate must declare a `[lints.rust]` table of its own ("exempt"
means exempt from *inheriting*, never from being linted), and an exemption naming a directory
that is no longer a workspace member fails the check, so the hole cannot outlive the crate it
was cut for. Both directions were watched to fail.

**Sliced data is not nothing, and the ADR that measured it is not wrong.** ADR 0020's table
stays in place as the before-picture, with a pointer here. What that record rejected was
narrowing the *field sets* (a functional change: `medium` date-and-time down to something a
translator could not use) and per-shell `Intl` (a correctness change). It never considered
the locale axis, because nobody had counted what `compiled_data` actually contained. Reading
0020's +35.9% as an argument that the data was irreducible would be a misreading the record
itself invites, so the sentence is here: 73% of that number was ~500 locales, and the
remaining 27% is code.

**`just i18n::check-data` was the one gate CI did not run**, which is worth stating rather
than implying from the word "checked". The `rust` job in `tools/ci/src/jobs.rs` runs
`cargo xtask check`, `just check`, `just build`, `just test` — and its own comment explains
why it names `cargo xtask check` instead of `just check-repo`: check-repo "would also run
`proto::check`", which needs the network and belongs in the job that has that excuse. There was
no i18n job, so `check-data` ran locally, as part of `just ci`, and CI enforced the two
artifacts of this change that need no generator: the lockfile witness and the version pins
(`cargo xtask check`), and the locale invariant (inside `just test`).

It has since been folded into the `proto` job, which is the job that already carries the
network excuse — `proto::check` pulled four remote buf plugins (all local since RT-50), so an ~80 MB CLDR download is
not the first thing in the pipeline to need the internet. The other option open at the time, a
job of its own, was never taken: a job is a schedule, a runner and a cache policy, and one
recipe does not outweigh that.

*(Since [ADR 0024](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0024-the-pipeline-is-derivations-and-a-shell.md) neither job exists. The CLDR release is pinned, so `nix/cldr.nix` fetches it
once as a fixed-output derivation and `icu4x-datagen` runs offline against the store path —
which made the gate a derivation, `outputs.ci-i18n-data`, and the network excuse along with the
job it belonged to went away. The gap this record names is closed by a check that now runs on
every push rather than by finding the right job to put it in.)* What the gap cost was the failure mode named above — a
payload stale in a way the locale invariant cannot see, regenerated from a different CLDR
release or with a marker quietly dropped — and that is now caught on a push rather than on the
machines that happened to run `just ci`.

The reason it moved is ADR 0022. Once `gates.toml` says where each gate runs and an invariant
reads the Justfiles and the jobs and disagrees out loud when they do, "locally only" stops
being a sentence nobody checks and becomes an entry that has to carry a reason. This is the
first entry to have needed one.

**`just i18n::check-data` needs the network**, the same way `just proto::check` needs the
network for its four remote buf plugins (all local since RT-50): the proto job runs the generator. The ~80 MB CLDR archive lands
in `icu4x-source-cache/` beside the Justfile — the tool has no flag for the location, it
writes relative to its working directory, so that path is gitignored and a second run reuses
it. Both facts are on the record because both would otherwise be discovered during a failing
CI run.

**rustfmt and typos have no entries for `crates/i18n-data/src/data`, and that is not an
oversight.** `mod.rs` is pulled in with `include!`, which rustfmt does not walk (it follows
`mod` declarations), and the payload files are not `.rs`. `cargo fmt --all --check` and
`typos` both pass over the tree untouched. If the crate ever declares `mod data;`, that
changes and the ignore lists gain a line — which is worth writing down precisely because the
two config files' comments imply they should already be there.

**What is not sliced, stated so the next reader does not go looking.** `icu_properties_data`
and `icu_normalizer_data` ride in every binary through `idna`/`url` and have nothing to do
with locale formatting. `icu_locale_fallback_data` is the fallback rules, deliberately kept
whole — the chains are what make `hi-Latn` resolve sensibly and they do not shrink with the
locale list. `icu_plurals_data` exists in `Cargo.lock` for the host-side build dependency and
not in any shipped graph (`cargo tree -p counter-tui -e normal` does not contain it). And
`und` ships in the payload always, because every chain ends there, whether or not anyone asks
for it.

## What was rejected, and why

**`ICU4X_DATA_DIR`, or any runtime-loaded provider.** The cheap option, and the one ICU4X's
docs reach for first. It moves the same bytes from the data segment to a file the app must
ship and read — for wasm that is an extra fetch, on the critical path, in front of the first
formatted string; for the native shells it is an asset in five packaging systems with a
runtime failure mode (`DataErrorKind::MissingDataFile`) that no `cargo test` in this
repository can produce. It also leaves `compiled_data` conceptually available, which is the
exact state that fails silently. Rejected for the general reason ADR 0004 exists — committed
artifacts and a build that needs no generator — and for the specific one: with the data
compiled in and `compiled_data` off, a missing marker is a compile error; loaded at runtime,
it is a blank date on a phone.

**`--markers-for-bin`, i.e. deriving the marker list from a probe binary at generation
time.** It works — a probe that calls every function in `format.rs`, run through
`icu4x-datagen --markers-for-bin`, produced byte-identical output to the committed explicit
list, which is how the list was derived in the first place. It is not in the recipe because
it makes the payload a function of what a *linker* kept alive: a formatter that becomes
unreachable but stays in the source would silently shrink the slice, and `check-data` would
happily regenerate the smaller payload while `format.rs` still contains the call. The
explicit list is a reviewable diff, the compile error in `format.rs` is the real guard
against a missing marker, and the length comparison against `lib.rs` is the guard against a
redundant one.

**The aggregate macro, `impl_data_provider!(Cldr)`.** It implements `DataProvider` for every
marker in one line. It does not implement `IterableDataProvider`, which is the half the
locale invariant reads its locale list from — and the invariant was not negotiable, because
without it this whole change is a measurement rather than a property. The per-marker
`impl_*!(Cldr, ITER)` calls cost nine lines and one naming rule (the length assertion in
`check-data`).

**Relaxing `unsafe_code` in `crates/i18n` and keeping the data in there.** It is one manifest
instead of two, and it puts permissive unsafe over `format.rs`, which is the file a person
edits, silently — the next contributor to that file loses a lint they do not know they had.
The counter-argument, at full strength: generated code is read line-by-line precisely when it
is not, so a crate that allows unsafe around a generator nobody audits is a bigger hole than
one that forbids unsafe and therefore cannot exist, and `crates/proto`'s comment on this
subject says `unsafe_code` is deliberately NOT relaxed. That comment still holds for generated
*code*, and it does not hold for data deserialised by construction from bytes the generator
itself validated. The exception exists; it has a directory and no prose in it.

**A `build.rs` that runs the generator.** It would keep the payload honest automatically, and
it would make every build of `i18n` need `icu4x-datagen`, the network and an 80 MB archive —
a compile-time dependency on Unicode's release schedule, which is precisely what pinning
`tools.cldr-json` was there to remove. ADR 0004 settled the general case for generated code;
this is that decision applied to a new artifact rather than a new argument.

**A hand-written locale list in the recipe, or a `--locales` superset maintained by hand.**
Both drift the way the invariant is designed to catch, and both would have been caught — the
invariant exists either way. Derived from `locales/` is strictly better: one input, two
consumers.

**Setting `unsafe_code = "allow"` in `[workspace.lints.rust]`** so the workspace set covers
the generated crate too. That is a workspace-wide loosening of a rule that exists for the
hand-written 100% of the repository, paid for 174 blocks in nine generated files.

**Accepting the 300 KB, i.e. ADR 0020's position, unchanged.** It was an honest position at
the time and it is recorded there. It is not a reason not to do this: 0020 rejected changing
what gets formatted, and this changes nothing that a translator or a user can see. The
rejection that would have been reasonable and is not on this list is doing nothing until
something needs the bytes — because the silent kind of bloat is the kind that is still there
when someone finally measures it, and this one had already been measured once and left alone.
