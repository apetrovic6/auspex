# 0017. Numbers are formatted by the shell, not stringified by it

- **Status:** Accepted
- **Date:** 2026-10-01

## Context

ADR 0016 moved the sentence out of the core: `ViewModel` carries `count: i64` and a
`Status`, and each shell looks up `count_is` and interpolates. What it recorded but
refused — *"the crate choice deserves its own look … locale-aware formatting is
unobservable in the example app"* — is what no shell did next: none of them *formatted*
the number. Every one stringified it with its language's default, in ten places rather
than five, because the skeletons carry the same code the example app does:

| shells | before |
|---|---|
| tui, web, desktop (`apps/counter` and `templates/crux-app`) | `count = view.count` — `translate_with!` interpolates via `Display` |
| android (`apps/counter` and `templates/platform-android`) | `stringResource(R.string.count_is, core.view.count)` — `Long.toString()` into `%1$s` |
| apple (`apps/counter` and `templates/platform-apple`) | `String(core.view.count)` into `%1$@` |

So the Croatian build rendered `1234567` where it owes `1.234.567`.

The markers were innocent. `%1$s` and `%1$@` are both correct for the `String` that
reaches them: the emitter has no type information, only the order of placeholders. ADR
0016 read that as *"the emitter will need typed markers before a number is formatted
correctly on a phone"*; this record supersedes that expectation. The defect was the
unformatted number one layer up, and fixing the number leaves every marker correct as it
stands — `aapt2 dump resources` still showing `%1$s` for `count_is` after this change is
the point, not a miss.

## Decision

**The catalog stays dumb; the shell formats the number before it interpolates.**

`count_is = "Count is: %{count}"` still does not say what kind of value fills the
placeholder, and the catalog grows no type vocabulary. Each shell formats through its own
platform's answer instead:

| shell | formats with |
|---|---|
| tui, web, desktop | `i18n::number(value, locale)` — ICU4X `icu_decimal`, behind a new `format` feature |
| android | `java.text.NumberFormat.getInstance(…)` on the configuration locale |
| apple | `value.formatted()` — `Locale.current`, unverified like every Swift line here |

- **The `format` feature is off by default**, for the reason `leptos` and `detect` are
  (ADRs 0014, 0015): `services/` and the worker template may link this crate for one
  user-facing string, and have no business acquiring a compiled CLDR blob for it. The
  three Rust shells switch it on; `cargo check -p i18n` with no features remains the
  guarantee. The `web-app` skeleton links `i18n` and renders no number, and stays without
  the feature — which is the opt-in earning its keep on day one.
- **ICU4X was measured against its alternative, not assumed** — ADR 0011's habit, same
  reason: the number is what makes the decision hold. `cargo build --release -p
  counter-tui` and `trunk build --release` for the web shell, before and after, on
  x86_64-linux:

  | artifact | before | after | delta |
  |---|---|---|---|
  | `counter-tui` release | 5,018,776 B | 5,136,288 B | +117,512 (+2.3%) |
  | `counter-tui` release, stripped | 3,508,032 B | 3,592,408 B | +84,376 (+2.4%) |
  | `counter-web` `.wasm`, wasm-opt'd — what a browser downloads | 790,761 B | 852,979 B | +62,218 (+7.9%) |

  That is the whole cost, and it buys all of CLDR's integer formatting: grouping
  positions that are not every-three (Indian lakh grouping), sign placement, numbering
  systems, and every locale a product adds later with no code. What the lockfile shows
  is worth knowing too: `icu_locale_core` was already in the graph (`idna` pulls it
  through `url`), and adding `icu_decimal` unified the ICU stack on one 2.3 generation
  rather than duplicating it — seven new packages, no second version of anything.
- **Android formats with the configuration locale** — the locale `Resources.getString`
  itself formats the sentence with — so the number and its surrounding words cannot
  disagree inside one app.
- **The dependency is declared in `crates/i18n/Cargo.toml`, not `[workspace.dependencies]`
  and not `versions.toml`.** One consumer names it — the shells ask this crate for a
  feature, they do not name ICU — which is the call `rust-i18n` and `sys-locale` already
  make; and `versions.toml` single-sources versions two *tools* must agree on, which
  nothing outside Cargo reads (ADR 0005).

## Consequences

**Five shells can print two different strings for the same `i64`, deliberately.** The
platform shells format with the OS's CLDR; the Rust shells format with ICU4X's, and the
two track Unicode releases independently. At the edges — an exotic numbering system, a
grouping rule CLDR revised between vintages — they can disagree, and a product's web and
Android surfaces can disagree with each other on the same device. On a phone the OS is
the *right* answer: a number should look the way every other app on that device renders
it. Uniformity across surfaces was purchasable only by shipping ICU4X into the mobile
binaries too, alongside the platform's own CLDR — a second source of locale truth in each
binary, at a size cost, for consistency nobody on either device can see. Within a device
this design is coherent; across devices it is as coherent as the CLDR vintages are. That
is the trade, recorded here so it is a decision rather than a discovery.

**The gap the TUI comment named is now a check, and it ships beside this one.** The first
interpolated call site carried a comment saying the quiet part: the key is generated, the
placeholder is in the locale file, and nothing related the `name = value` pairs of
`translate_with!` to either — a typo in `count` would render the sentence with
`%{count}` still in it, at runtime, on screen, past every gate. An uninterpolated
placeholder is louder than an ungrouped number, so a formatting change that left it alive
would have fixed the quieter bug. `translate_with_arguments_match_the_catalog` in xtask
now compares every `translate_with!(Key::…)` call against the placeholders the *compiled
crate* declares — the union across all locales, so a name only the Croatian copy uses is
checked too — in both directions: a placeholder with no argument and an argument with no
placeholder are each a problem. It walks templates as well as apps (`rename::walk`), so
the skeletons are covered for free, which is the only reason to write it in xtask rather
than as a test in one shell. Negative-tested the way `locale_files_are_complete` is:
renaming `count` to `contn` in the crux-app desktop skeleton turns a clean run into two
problems naming the file and line. Stated limit: a key that is not spelled `Key::…` —
a literal, a variable — is skipped, because there is no catalog entry to compare it with;
every rendered sentence in the tree names a generated variant.

**Both skeletons moved in this change, not after.** All five template copies — the three
crux-app shells, `templates/platform-android`, `templates/platform-apple` — changed with
their five app copies, which is `b4dfb9b`, `1bd7bf5` and ADR 0016's lesson applied
rather than cited. The gates that compile them are `just check-platform-templates` and
`just check-template`, and the Swift half of the Apple change is verified by nobody until
someone builds it on a Mac: `count.formatted()` is the fifth unverified Swift construct
in this repository's ledger.

**The on-screen proof, and its honest limit.** The substance is a test:
`number(1_234_567, Locale::Hr) == "1.234.567"` and `Locale::En == "1,234,567"` run on
every `cargo nextest run -p i18n`. `LANG=hr_HR.UTF-8 cargo run -p counter-tui` still
proves the whole sentence path end to end, but the counter starts at 0 and grouping is
invisible below a thousand — the example app renders a four-digit count only after a
backend has been asked four digits' worth of increments. ADR 0016 called formatting
"unobservable in the example app" and wanted fixtures; the fixtures are the tests, and
the screen shows the wiring.

## What was rejected, and why

**Typed placeholders.** `%{count:number}` in the catalog, with the emitter choosing
`%1$d`, `%1$lld` or a grouping-aware marker per platform. This is the more principled
answer — the catalog becomes the one place that says *this is a number* — and it is what
ADR 0016 expected ("the emitter will need typed markers"). It lost on cost and on reach.
The cost: a template-syntax extension, changes to `tokens`/`format_placeholders` in
`i18n-keygen`, a per-platform conversion table, and a vocabulary every translator of
every locale file has to learn — to buy what a pre-formatted string buys for free. The
reach: it does not so much solve Android as relocate the problem, because
`Resources.getString` formats through Java's `String.format`, where `%d` does not group
unless the `,` flag asks it to — so the table has to encode each platform's integer
marker *and* its grouping flag, and stay right as platforms add markers. And it would
still leave the Rust shells needing a formatter for the typed value, which is this
change wearing a syntax extension on top.

**The declared separator table.** `grouping`/`decimal` per locale in the catalog, read by
the generator onto `Locale`, and a hand-rolled grouper. This was the designated fallback
if ICU4X's compiled data proved unacceptable — and the measurement above is why it was
not invoked: +62,218 B (+7.9%) on the wasm bundle a browser downloads, +84,376 B (+2.4%)
on a stripped native binary. The table would cost a few hundred bytes and then be a
second source of locale truth, silently wrong for the first locale whose conventions two
columns cannot state — lakh grouping, a minus sign that does not lead, a numbering
system that is not Latin. It remains a live fallback for a product that needs the bytes
back; the figures here are the record of what ICU4X costs and the trigger to revisit.

**`Intl.NumberFormat` in the web shell.** Free in wasm — the browser already carries
CLDR, and the web shell was the one artifact where the ICU4X blob is downloaded rather
than installed. Rejected because it makes the same product render its numbers two ways
on one machine — the browser's CLDR in the web shell, ICU4X's in the TUI and desktop
shells — and the disagreement would be invisible to every test this repository runs,
since none of them drives a browser. One formatter across the three Rust shells is one
behaviour and one test; the +7.9% was measured and accepted rather than dodged into a
second source of truth.

**Formatting in the core.** The core would have to know the locale, and the locale is
presentation: ADR 0015 gives detection to the shell, ADR 0016 gives the sentence to the
shell, and a core that formats emits strings again — the exact thing 0016 exists to
prevent.

**Plurals, dates, currency and percent.** Not in this change, and one claim from the
catalogue's early days deserves correcting while it is on record: plurals are *not* "one
key away". `plural_families` in `i18n-keygen`'s resource emitter exists to **reject** —
`Error::Plurals` fires because rust-i18n's `key.one`/`key.other`, Android's `<plurals>`
and Apple's String Catalog variations do not map one-to-one, so adding a plural key
today is a build error rather than a new path exercised. Plurals means implementing two
emitters and getting Croatian's `one`/`few`/`other` right; it is separate, larger work.
Dates have nothing in the repository that renders one. `number` over integers is the
whole surface here.
