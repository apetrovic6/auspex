# 0018. A plural family is one key, and the locale picks which of its strings to read

- **Status:** Accepted
- **Date:** 2026-10-02

## Context

ADR 0017 closed by naming what it left out, and correcting itself while doing so: plurals
are *not* "one key away", and `plural_families` in `i18n-keygen`'s resource emitter exists
to **reject** — `Error::Plurals`, deliberately, because `key.one` in a locale file,
`<plurals>` in Android and `variations.plural` in an Apple String Catalog do not map
one-to-one and the emitter was not about to pretend. This record is that larger work.

The upstream answer does not exist. rust-i18n 4.2.2 has no plurals: the strings `plural`
and `quantity` do not occur in its sources at all, and its lookup is keyed by locale and
key alone (`translate("en", "hello")`). So the `apples.one` / `apples.other` spelling that
the locale files now use is this repository's convention end to end, and no library
downstream of it will ever interpret it. What the platforms do have is two different models
of the same idea:

| platform | one plural looks like |
|---|---|
| rust-i18n (the catalog, and therefore Rust shells) | flat keys with a category suffix: `apples.one`, `apples.other` |
| Android | one resource with variants: `<plurals name="apples"><item quantity="one">…</item></plurals>` |
| Apple | one entry with a variation tree: `apples` → `variations.plural.one.stringUnit.value` |

The emitter already knew the flat spelling and refused the other two. The catalog already
ships the language pair where this matters — `en` and `hr`, English with `one`/`other` and
Croatian with `one`/`few`/`other` — and ADR 0017 already puts a bare `i64` into a Croatian
sentence. `1 jabuka`, `2 jabuke`, `5 jabuka` was the only thing missing from that path, so
a count in a sentence was one grammatical number away from being wrong in the language this
repository demonstrates.

## Decision

**A plural family is one `Key` variant, named for the family, and the count and the locale
select its member at runtime by CLDR's rules.**

| piece | what it is |
|---|---|
| `Key::Apples` | the family `apples.*`. One variant, whatever categories the locale files happen to hold |
| `Key::CATALOG` | `(&'static str, &'static str)` — `(key as the locale files spell it, its family)` for every string any locale defines |
| `Key::PLURAL_FAMILIES` | the families by name, so `apples.other` can be enumerated without knowing which categories exist |
| `plural_category(count, locale) -> Category` | CLDR cardinal rules, via `icu_plurals::PluralRules`, behind the `format` feature |
| `plural_key(Key::Apples, count, locale) -> String` | `"apples.few"` — the key the catalog actually holds |
| `plural_categories(locale) -> Vec<Category>` | the categories CLDR gives that locale, in CLDR's order |
| `<plurals name="apples">` / `variations.plural.one` | what the Android and Apple emitters write, from the same `Entry` list the `Key` enum is generated from |

Four rules sit underneath that, and each of them is a decision the previous sentence hides.

**Canonical keys are per family, not per key.** ADR 0016's rule — the fallback's keys are
canonical, and a key one locale has that the fallback lacks is not a key — was applied to
each key, which for plurals means `en` would have to declare `apples.few` for Croatian's
`few` to exist, and `en` has no such category and never will. It is now applied per family:
`en` declares `apples`, therefore `apples` exists; which categories of it `hr` holds is not
`en`'s business. Whose business it is, is CLDR's, and that check is separate and runs both
ways (below). A family that only a non-fallback locale declares is still not a key: drift
warning, the same rule as any other key, so `jabuka` in a Croatian-only family stays visible
as a warning instead of becoming an untranslatable variant.

**CLDR decides coverage, and coverage is a hard error.** `plurals_match_cldr` refuses a
locale that defines a category its own cardinal rules can never select (`few` in English —
the string is unreachable, and a translator will keep maintaining it) and a locale whose
rules select a category nobody wrote for a family the fallback declares (`few` missing from
Croatian — 2 readers fall through to `other` and get a form a translator meant for
fractions). Both directions, because a silent fallthrough and a silent dead string are the
same defect seen from opposite ends. This runs on the fallback's families: a family the
fallback does not declare is not a key and gets no categories demanded of it.

**The count is still formatted, and `plural_key` is not a translation function.** ADR 0017
stands: `%{count}` is filled by `number(count, locale)`, and `plural_key` only answers the
name of the string. One value, two uses, at one call site:

```rust
translate_with!(
    plural_key(Key::Apples, count, Locale::Hr),
    Locale::Hr,
    count = number(count, Locale::Hr),
)
```

That composes rather than needing a new entry point because the generated wrappers take
`impl AsRef<str>` for the key — so `plural_key` returns a `String` and the macro is
unchanged.

**Three errors, for three distinct failures.** `Error::CanonicalKeys` — a family the
fallback does not declare; `Error::Plurals` — a family defined without `other`, the one
category every language's rules can fall through to, so a family without it can render
nothing at all; `Error::PluralCategories` — the CLDR mismatch in either direction. They are
three messages because a build that says "plurals are not supported" about any of them is
how this feature gets "fixed" by deleting it.

Measured, in the currency ADR 0017 established (`cargo build --release -p counter-tui`,
`strip`, `trunk build --release`; x86_64-linux, before = `391d549`):

| artifact | before | after | delta |
|---|---|---|---|
| `counter-tui` release | 5,148,560 B | 5,152,280 B | +3,720 (+0.07%) |
| `counter-tui` release, stripped | 3,604,488 B | 3,606,672 B | +2,184 (+0.06%) |
| `counter-web` `.wasm` — what a browser downloads | 859,499 B | 860,384 B | +885 (+0.10%) |

That is nearly nothing, and the reason is in the lockfile rather than in the diff: the
change adds **no packages**. `icu_plurals` was already in the graph as a dependency of
`icu_decimal` (ADR 0017 brought it in), so this pays for its own code — the rules it needs
are generated data that was already compiled in. `Cargo.lock` moves by three edges.

## Consequences

**`Key::CATALOG` is the enumeration surface, and using `Key::ALL` instead fails silently.**
A plural family is one variant but several strings, and `Key::Apples.as_str()` answers
`apples`, which no locale file holds. Anything that walks keys to produce output — the
Android and Apple emitters, xtask's placeholder table, the coverage tests — must iterate
`CATALOG`; one that iterated `ALL` and looked each name up would look up `apples`, find
nothing, and emit no plural at all, in a build that passes. The generated doc comment on
`CATALOG` says so where the mistake would be made.

**Android gets `<plurals>`, and the collision rules had to notice.** `R.string.apples` and
`R.plurals.apples` are distinct to `aapt2`, but the resource merger treats a name clash
across the two tables as a clash, so the emitter keeps two name tables and checks a family
against both. That check existed already and was right; what it now sees is a family name
where it used to see only string names.

**`translate_with_arguments_match_the_catalog` reads a plural call as its family.** The
split is depth-aware, so `plural_key(Key::Apples, count, locale)` stays one segment and the
variant is still what follows `Key::`. A family contributes one catalog entry carrying the
union of its members' placeholders, which is exactly the contract a call site has: it does
not know which category the reader's language will select, so it must supply what all of
them need. It also refuses `plural_key(Key::Increment, n, l)` — `plural_key` takes any
`Key`, so that call type-checks, builds `increment.other`, and renders the key at the
reader, and nothing else about it looks wrong.

**Drift became family-aware, in both directions.** A missing `apples.few` in Croatian is
not drift (CLDR is asked about that, separately, and Croatian does have `few`, so it is
also a hard error if the copy does not exist); an extra `apples.few` in English is not
drift either — it is `Error::PluralCategories`, which is louder. Drift is what is left:
keys that belong to no family and to no fallback.

**What is proven, and by what.** The Rust path is a test: `plural_category` for `en`
(one/other) and `hr` (one/few/other), 21 → `one` and 22 → `few` in Croatian, 1234 → `few`
(last two digits, not the whole number), and an end-to-end `translate_with!` rendering
`1 jabuka` / `2 jabuke` / `5 jabuka`. `every_category_a_locale_has_translates` runs the
coverage rule at runtime against the compiled catalog, beside `every_locale_translates_every_key`.
`cargo xtask check` lands on **224 invariants, up from 218** (`391d549` measured in a
second worktree). Six, and none of them new invariant functions: three catalog-checked
`translate_with!` call sites (two in `i18n`'s plural tests, one in xtask's own scanner
fixture — that file is walked, so its fixtures are checked against the real compiled
catalog), two for this record in `decision_records_are_indexed` (the file and its index
link), and one because this record names `apps/counter` somewhere a reader could follow.
A record counts itself.

**What this repository still cannot check.** The Apple String Catalog is written and unit-
tested as JSON; nothing here compiles it — that needs macOS, and no Swift line reads the
result. The Android output was built: `just apps/counter/android` ran for this change and
assembled, with one `<plurals name="apples">` in `values/` carrying `one` and `other` and
the same name in `values-hr/` carrying `one`, `few`, `other`. That build is not in
`just ci`, so nothing re-runs it on a push. And the Rust shells gained no plural call site: the demonstrator family
`apples.*` is in `locales/common/`, reaches the Android and Apple resources of the example
app through the emitters, and is consumed on the Rust side only by tests. The screen still
shows `count_is`, not a plural.

*(That last sentence was true when it was written and is no longer. It is left standing
rather than deleted, because the paragraph above it — what is proven, and by what — is read
against the state of the tree at the date of the record, and the gap is the reason the
following section exists.)*

## Amendment: the plural reaches a screen

The gap above is closed in the Rust shells and in the Android one. `count_is` now has a
second line under it — in `counter-tui`, `counter-desktop` and `counter-web`, and in the
three shells of the crux-app skeleton — rendering `apples.*` for the same count, in a
quieter style, as a legend rather than a second reading of the number. The Android shell
calls `pluralStringResource(R.plurals.apples, …)`. Apple does not, and says so where the
call would be.

**The call site passes the count twice, in two forms, and the two are not
interchangeable.** `plural_key` takes the raw `i64` because CLDR's rules are arithmetic on
it; the sentence takes `number(count, locale)` because `%{count}` is text. Android is the
same rule with types made explicit — `pluralStringResource(id, core.view.count.toInt(),
count)` — and it is where the mistake is easiest to see, since passing the formatted
`count` as the quantity is a type error, and passing the raw one as the argument would
print `1234567` in a language that owes `1.234.567`.

**The legend repeats the value and not the information, and that was argued rather than
assumed.** `apples.*` carries `%{count}` already, so a call site that renders the noun must
supply the number, and `Broj je: 22` above `22 jabuke` says the digits twice. Dropping
`%{count}` from the family would leave a bare noun and cost the demonstration: the line as
written is ADR 0017 and ADR 0018 composing in one string, and `%1$d apples` is the shape a
translator of `<plurals>` expects to write.

**Measured on the screen, in both languages, at the boundary.** The TUI was run against the
service (`devenv up postgres api -d`) in `hr` and in `en`, its count set through the API and
redrawn with `r`, at 0, 1, 2, 5, 20, 21, 22, −1, −2, −5, −21. Croatian: `20 jabuka`,
`21 jabuka`, `22 jabuke`, `−2 jabuke`, `−5 jabuka`, `−21 jabuka`. English: `20 apples`,
`21 apples`, `22 apples`, `−1 apple`. So `few` and `other` are adjacent on the screen and
indistinguishable from the number alone, which is the point of putting them there.

**Negative and zero were not assumed either.** `plural_category(-1, hr)` is `one`,
`plural_category(-21, hr)` is `one`, `plural_category(-2, hr)` and `plural_category(-5, hr)`
are `few` and `other`, and `plural_category(0, …)` is `other` in both locales — CLDR's
cardinal rules are arithmetic on the digits and are not symmetric about zero. These are
pinned in `crates/i18n/src/format.rs` beside the categories, together with the sign: a
negative count arrives in Croatian as `−1` with U+2212, the minus the locale's own data
declares, and in English as `-1` with the ASCII hyphen, in `number()` and therefore in the
sentence. A shell that formatted the sign itself would be writing Croatian's minus wrong in
a way no test would notice, which is what that test is for.

**`plural_families_reach_a_shell` is the new check, and it proves less than it looks like.**
Every family in `Key::PLURAL_FAMILIES` is named by a `plural_key(Key::<Variant>, ..)` call
in the example app's shells and in the skeleton's, both trees globbed so the check still
means something in a generated product where the example app has been renamed. A family that
ships in the catalog, and in the generated Android and Apple resources, and is called
nowhere, is the exact state this amendment was written out of, and it now fails the build.
The check walks the two trees with the same `translate_with!` scanner and the same
prose-comment guard as the placeholder invariant — that guard is now one function,
`mentioned_in_prose`, shared by four checks, because a commented-out call site is not a call
site in any of them. What it cannot prove is the sentence this section is titled with: a
shell that no longer paints is not a shell that lacks a call site. It is a call site, not a
framebuffer, and the TUI run above is the part of "reaches a screen" no check here can be.

**Android's lint ran, and did not object.** `just apps/counter/platforms/android/check`
(`:app:lintDebug`) after the new call: **0 errors, 23 warnings**, none of them about
quantities — `ImpliedQuantity`, `MissingQuantity` and `UnusedQuantity` are silent, which is
what a `<plurals>` with an explicit quantity and a real caller looks like. The Android
*build* does re-run on a push: `Job::Android` in `tools/ci/src/jobs.rs` discovers every app
with a platform project and runs `generate-android`, `build` and `test`. It does not run
`check`. Lint is the one Gradle stage CI never invokes, and it is the stage that reads
resources — so a `<plurals>` regression is caught here, by hand, and nowhere automatically.

**Apple: recorded, not implemented, with the entries already on disk.** The generated
`Localizable.xcstrings` carries the family — `apples` with `variations.plural` of `one` and
`other` for `en` and of `one`, `few`, `other` for `hr` — because the emitter has written
those since this record was accepted. What is *not* known is what SwiftUI does with them:
whether a plural entry is selected from a numeric argument by the framework, or whether the
caller must name the member, or what fails and how. Writing `String(localized: .applesOne)`
or `Text("apples.one", …)` would be guessing a generated symbol on a host that cannot
compile Swift, and a guess there surfaces on someone else's machine. `ContentView.swift`
says this where the line would go, in both the example app and the platform skeleton. The
first macOS host to run `just apps/counter/generate-apple` and build the app closes it, and
the Android shell above is the working reference for what the call is meant to do.

**Checked counts, attributed by reverting each file rather than by arithmetic.** `cargo
xtask check` moves from **262 invariants at `5d99d2c`** to **275 here**. Thirteen, and each
part was measured by putting that one file back and re-running: six `translate_with!` call
sites in the six shells; three in `crates/i18n`'s new negative-count tests; four in xtask,
which is two for `plural_families_reach_a_shell` (one family by two trees) and two for the
test fixtures in that same file — the file is walked, so a fixture that names a key is a
call site, and the third fixture line is a comment, which the guard above refuses to count.
This record contributes nothing, and that is worth saying because the accepted text above
claimed *a record counts itself*. It did — as a new file, once per app, in the path rule.
Amending it adds zero, because the file was already counted and already indexed. A plural
that reaches a screen is not an invariant; it is a call site in a shell, and only the
shells' six are new.

## What was rejected, and why

**A variant per category — `Key::ApplesOne`, `Key::ApplesFew`.** It looks like the honest
mapping of the locale files, and it is the one thing the design cannot survive: the set of
categories is per language, so the enum would differ per locale, and a shell that names
`Key::ApplesFew` is naming Croatian grammar. Selection would then live in every call site —
a `match` on category in each of five shells, which is the plural engine forked five times.
One family variant means the selection happens once, in `plural_key`, and a call site cannot
get it wrong.

**A generated `translate_plural!(Key::Apples, locale, count = n)` macro.** The obvious
convenience, and the reason it lost is that it is not one thing but two: it would re-implement
`translate_with!`'s argument machinery for a second grammar (`split_arguments`, the
`name = value` rule, the invariant that reads it), and it would still take the count twice,
since the name it picks and the digits it renders come from the same value by different
rules (`plural_key` for the category, `number` for the string). `translate_with!(plural_key(…))`
is the same three pieces, already parsed by the existing invariant, with no new syntax for a
translator or a scanner to learn.

**Hand-rolled rules, or another plural crate.** `plural_category` is ten lines of modulo
arithmetic for the six CLDR categories if you only want English, and wrong for Croatian,
Welsh, Arabic and Slovene in ways that do not fail. It is the same argument ADR 0017 made
about the declared separator table: the hand-rolled version costs fewer bytes and becomes a
second source of locale truth, silently wrong at the first locale whose rules its columns
cannot state. `icu_plurals` is also the *same harness* as `icu_decimal` — same ICU version,
same generated data, one answer about what `11` is in Croatian — and that is the reason not
to add a second plural crate alongside it.

**CLDR coverage as a warning.** Drift is a warning because a missing translation is a
translation-queue problem. A category mismatch is not: a fallthrough to `other` renders a
form the translator chose for a different number class, in a release build, on someone's
phone, and nobody in the repository reads warnings on a phone.

**`Intl.PluralRules` in the web shell, or the platform's own plural API on a phone.** Free
in the browser, native on both mobile platforms, and rejected for the reason ADR 0017
rejected `Intl.NumberFormat`: two implementations of one grammar, agreeing by coincidence,
invisible to every test this repository runs because none of them drives a browser or a
device. The Rust shells answer with ICU4X, the platform shells answer with their own CLDR
inside their own resource system, and that split already exists for numbers.

The last of those is now the honest rule rather than the observed behaviour: the Rust shells
answer with ICU4X because ICU4X is what they call (measured, above). The platform shells
call their own resource system on Android, and on Apple do not call it at all yet — the
same rejection, recorded again where the gap is, because on Apple it is currently a
limitation rather than a decision carried out.

**Typed placeholders — `%{count:plural}` in the catalog.** ADR 0017 rejected typed markers
and plurals did not need them: the family suffix *is* the marker, visible to the generator
and to the translators, and it adds no vocabulary to the template syntax. The count inside
the string stays a plain `%{count}`.

**Letting `Key::Apples` translate to something — the `other` form, say.** It would make the
mistake of writing `translate(Key::Apples, locale)` instead of
`plural_key(Key::Apples, count, locale)` look like a working line of code, rendering a form
for a number the reader never saw. Naming the family alone renders `apples` on screen, which
is quiet but is at least the family's own name, and the invariant above refuses the shape
where it happens in this repository's own sources.
