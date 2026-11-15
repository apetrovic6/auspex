# 0015. The shell detects the locale, and matching it is a lookup

- **Status:** Accepted
- **Date:** 2026-09-30

## Context

ADR 0014 left the choosing of a locale to a later decision, and this is it. Stage 1 seeded
every Rust shell with `Locale::FALLBACK` and gave it no way to change:

```
apps/counter/shells/web/src/main.rs:32      apply_locale(Locale::FALLBACK, locale);
apps/counter/shells/desktop/src/main.rs:46  locale: Locale::FALLBACK,
apps/counter/shells/tui/src/main.rs:93      locale: Locale::FALLBACK,
```

Android and Apple do not have this problem: their shells read the generated `values-hr/`
and `Localizable.xcstrings` through the platform's own resource lookup, so the OS picks
the language. That was proved on a device — a Croatian emulator renders
Poništi/Povećaj/Smanji. The consequence is that `hr.toml` reaches **two of five shells**,
and the three written in Rust render English on any machine, whatever the user's system
says. A translation nobody can reach is indistinguishable from one that does not exist.

0014 named the three ways a product picks a locale — a URL scheme, `navigator.language`,
a stored preference — and shipped none of them. What it did not consider, because the port
it recorded was Leptos-shaped, is that the native shells have a fourth and more basic
source: the operating system, which is where `navigator.language` gets its answer too.

## Decision

**The system's preference list is the source, `crates/i18n` owns the matching, and the
shell owns the result.**

- **`sys-locale` behind a `detect` feature, off by default.** It is already in the
  dependency graph — `cosmic-text 0.19.0` (gpui's text stack) pulls it in, and its own
  dependency is `libc`. One crate covers every Rust shell: POSIX `LANG`/`LC_ALL`, macOS
  `AppleLanguages` (which is not an environment variable, so a hand-rolled version is
  wrong on the platform it looks most correct on), Windows `GetUserDefaultLocaleName`,
  and — with its `js` feature, on `target_family = "wasm"` — `navigator.languages`. The
  feature is off by default for the reason the `leptos` feature is: a worker or service
  that links `i18n` for a user-facing error message has no business acquiring platform
  detection, and `cargo check -p i18n` with no features is what keeps that honest.

- **`get_locales()`, not `get_locale()`.** It returns the user's preferences in descending
  order. A user who lists Croatian second should get Croatian when we have no first choice,
  and only the list can say so.

- **Matching is RFC 4647 lookup, not equality.** `sys-locale` hands over BCP 47 tags —
  `hr-HR` from a browser, and also `hr-HR` from a POSIX `LANG=hr_HR.UTF-8`, which it
  converts itself — while the locale codes are whatever `locales/` names them, `"hr"` here.
  Neither is the other. So each candidate is truncated at each `-` until something matches:
  `zh-Hant-TW` → `zh-Hant` → `zh`. The first candidate that resolves wins; if none does,
  `Locale::FALLBACK`. Measured rather than assumed: `LANGUAGE=de:hr:en LANG=de_DE.UTF-8`
  yields the preference list `["de", "hr", "en", "de-DE"]`, which resolves to Croatian —
  the second choice, because the first is a language this product does not speak.

  `negotiate` also drops a `.encoding` or `@modifier` suffix and maps `_` to `-` before
  looking anything up. On every path measured `sys-locale` has already done that; it is
  there because `negotiate` is public and its input need not have come from `sys-locale`.

- **The comparison is ASCII-case-insensitive, against `Locale::ALL` rather than
  `Locale::try_from`.** `try_from` is a `match` on the code exactly as the directory spells
  it, and the conventional spelling of a script subtag is titlecase — a product with
  `locales/zh-Hant/` would have every `zh-hant` from the system miss it. Case folding the
  candidate instead would trade that for the mirror-image bug. Comparing without regard to
  case is the only version that is right in both directions.

- **The negotiation is a pure function, the platform call is a three-line shim.**
  `negotiate(preferences) -> Locale` takes an iterator and touches nothing, so the whole
  algorithm is table-testable; `from_system() -> Locale` behind the feature is
  `negotiate(sys_locale::get_locales())`. Everything worth getting wrong is in the half
  that needs no platform to test.

- **The shell calls it where it seeds its `Locale`, and pairs it with `set_locale`.** That
  pairing is ADR 0014's hazard: rust-i18n's lookups read a global, a `Locale` field or
  signal is invisible to it, and the two drifting apart shows as reactive text in one
  language and everything else in another. The web shell already names the pairing
  `apply_locale`. Detection moving into the crate is what stops that two-line function
  being copy-pasted into three shells — the duplication `crates/i18n` exists to prevent.

- **A shell that holds a plain `Locale` gets one call, not two: `adopt_system_locale`.**
  It reads the system, sets the global and hands back what it set, so a struct field is
  initialised with `locale: i18n::adopt_system_locale()` and the forgettable half cannot
  be forgotten. Written after the rest of this record, because implementing it made the
  omission obvious: detection had moved into the crate and the *pairing* was still about
  to be pasted into three shells. The web shell does not use it — it has a signal to write
  as well, which is what `apply_locale` is for.

- **Both templates get it in the same change**, not afterwards. A skeleton that misses an
  i18n change ships the defect into every generated product, and `templates/` is compiled
  by nothing, so no gate notices. That is not hypothetical: it is what the crux-app desktop
  skeleton did between 71aa169 and the commit that added `gpui_labels_are_translated`.

## Consequences

**The chrome becomes Croatian and the body stays English.** `ViewModel { count, status }`
is formatted by the core — `format!("Count is: {}", …)` — so a detected `hr` translates
the buttons and nothing else. That is exactly what the Android emulator already shows, now
reproduced on desktop, tui and web. This makes stage 2's absence more visible rather than
less, which is an argument for doing it, not for delaying this.

**The TUI becomes the first end-to-end proof that runs on Linux.** `LANG=hr_HR.UTF-8`
against `just apps/counter/tui` exercises detection, negotiation, the rust-i18n global and
the rendered result in one command, with no emulator, no device and no Mac. Nearly
everything else about this repository's native surfaces needs hardware it does not have.

**Nothing in the repository selects a locale other than the system's.** `path::split` and
`path::with_locale` stay generated and stay unused; a product that wants a URL scheme or a
remembered choice has the seam and the reasoning below.

## What was rejected, and why

**Splitting on the first `-`.** One line instead of ten, correct for `en` and `hr`, and
wrong the first time a product ships `zh-Hant` beside `zh-Hans`, or `pt-BR` beside `pt-PT`
— at which point every user of the second variant silently gets the first. This is a
template: the naive version would be inherited by every product generated from it, and the
failure is invisible to whoever inherits it. The lookup loop exists to be *not* simplified,
which is why it is written down here.

**Exact matching against `Locale::ALL`.** Honest and useless: no operating system and no
browser hands out a bare `"hr"`.

**Detecting in each shell instead of the crate.** Three copies of "read the system, map it,
call `set_locale`", diverging the first time one of them is fixed — and `crates/i18n` exists
because the same three labels lived in five places.

**A language picker in the template.** A dropdown is product UI and a product decision: it
needs a place in a layout, a persistence story and an opinion about whether it overrides
the system. The template ships the seam and lets the product spend that decision.

**A URL scheme, and a stored preference.** Still rejected, for ADR 0014's reasons rather
than new ones: neither web surface has a router, and `fishing`'s `localized_routes!` — the
port that would have supplied one — opens by saying it is a hack into `leptos_router` that
should not be used. A `localStorage` preference has no such obstacle, but it is the second
half of a picker, and without one there is nothing to store.

**Making detection the default rather than a feature.** It would put `libc` (and on wasm,
`web-sys`) into every consumer of the translations, including a worker whose only use is a
string in a log line.
