# 0014. Translations are a crate, and the language is the shell's business

- **Status:** Accepted
- **Date:** 2026-09-29

## Context

Before this change the repository shipped the same three button labels five times — once
each in `apps/counter/shells/web`, `shells/tui`, `shells/desktop`, the Kotlin and the Swift
shell — plus copies in both web templates, and a core that formats English prose into the
view model (`format!("Count is: {}", …)`, `"working…"`, `apps/counter/core/src/app.rs`).
That is fine until the first non-English product, at which point every product instantiated
from this template inherits the problem, five copies deep.

The translations were ported from `fishing` (`i18n-keygen` and `i18n`, ~1800 lines with
tests), which had already worked out the shape and the one constraint that makes it look
unusual: **rust-i18n's macros are crate-local by construction.** `i18n!` defines
`_RUST_I18N_BACKEND`, `_rust_i18n_backend()` and a `pub(crate)` `_rust_i18n_t` at the
*invoking* crate's root, and `t!` expands to `crate::_rust_i18n_t!(…)`. A library function
that calls `t!` cannot exist — not in a shared crate, not anywhere — and a dependent crate
cannot use `t!` against another crate's translations however it imports them. So the
generator emits the wrappers (`translate`, `try_translate`, `available_locales`,
`translate_with!`) *as source* into the crate that called `i18n!`, and that crate `include!`s
the result **at its root, not inside a `mod`** — the generated code calls `t!` itself. What
crosses the crate boundary is a plain function call. An implementer who "tidies" the
`include!` into a module breaks it.

The generator exists at all because rust-i18n globs `locales/**/*.toml` into a **single flat
key space** — the directory contributes nothing to the key — so `login/hr.toml` and
`lakes/hr.toml` can both define `header` and glob order decides the winner with no
diagnostic. The fix is a build-time rule, recorded below as a hard error.

## Decision

Two crates, in the split `fishing` proved: **`crates/i18n-keygen`**, a host-only build
dependency whose sole dependency is `toml`, holding every rule about the locale tree; and
**`crates/i18n`**, the translations plus the generated `Key`, `Locale`, `path` helpers and
translate wrappers, with the Leptos adapter (`reactive::tr`, 30 lines) behind a `leptos`
feature that is **off by default** — `cargo check -p i18n` passing without features is the
guarantee, and it matters here more than it did in `fishing` because a `services/` or worker
dependents may link `i18n`, and a default-on Leptos would put a web framework in a worker.

- **Generated `Key` and `Locale` so a typo is a compile error**, each with `ALL` (which is
  what the coverage test iterates), and `Locale::FALLBACK` paired with `FALLBACK_LOCALE`.
- **The namespace rule is the one hard error:** every file under `locales/<name>/` must
  prefix its keys with `<name>.` (`check_namespaces`, `check_key_prefixes`,
  `check_namespaces_unique`). `common/` and `pages/` are *grouping directories* — filing
  decisions that name nothing, so `locales/pages/counter/` owns `counter.*` and not
  `pages.counter.*`.
- **Missing translations stay `Drift::Warn`,** not errors: a missing key still renders, in
  the fallback language, so it is a defect rather than a broken build. The gap is closed by
  the ported `every_locale_translates_every_key` test instead, which names every missing key
  at once rather than failing the build on the first.
- **The fallback is `en`, second locale `hr`.** `fishing` falls back to Croatian; a product
  cut from this template is written in English first and translated second, so the canonical
  key set is the English file. Two locales ship because one cannot exercise a fallback.
  Which locale is the fallback is *not* free: the generated `path::with_locale` drops the
  prefix for the fallback and adds it for the others, and `fishing`'s own path tests encode
  the Croatian choice — swapping the fallback inverted half of them, which is now a test
  comment in `crates/i18n/src/lib.rs`.
- **The fallback locale is written twice** — `i18n!`'s literal and the generator's
  `.fallback(..)` — and only the ported `the_i18n_macro_and_the_generator_agree_on_the_
  fallback` test ties them together. The generated `const assert!` cannot: it compares two
  values derived from the same input, and rust-i18n's own copy lives in a `static` const
  evaluation may not read. Left unchecked the failure is near-invisible: every key renders
  as itself.
- **`leptos` becomes a `[workspace.dependencies]` entry** and all four manifests inherit it
  (both counter-web, both template web shells, `crates/i18n`'s optional one). It was
  already three verbatim copies of `0.8.20`; the fourth is what would have made them drift,
  and Leptos context — `expect_context::<RwSignal<Locale>>` in `tr` — is matched by *type
  identity*, so what the entry buys is a single `leptos` in the graph. Which pins threaten
  that was measured rather than assumed, and the assumption was wrong in both directions: a
  compatible local pin (`leptos = "0.8.19"`) unifies to one version and changes nothing, and
  an exact one (`"=0.8.19"`) cargo refuses to resolve at all — `all possible versions
  conflict with previously selected packages` — because `crates/i18n` requires `^0.8.20` and
  every consumer links it. Cargo catches those itself, loudly, at build time. The case it
  does **not** catch is a *major* drift — a shell on `0.9` while `crates/i18n` stays on `0.8`
  — both of which cargo happily puts in the graph; it compiles, and `expect_context` misses
  at runtime with what reads as "no locale provided". That one case is the whole argument for
  the entry, and it is why no xtask check was added on top: the two cheap mistakes are
  already build failures, both templates inherit via `workspace = true`, and what is left is
  a hand-written crate doing a major bump. Not `versions.toml`: that file is for numbers two
  *tools* must agree on (ADR 0005); this is four Cargo manifests, which is what a workspace
  dependency is for.
- **One xtask check, `locale_files_are_complete`:** every directory under
  `crates/i18n/locales/` that holds a locale file must hold one per locale. The generator
  compares keys across the files that *exist*; a whole missing `hr.toml` is not missing
  keys, it is a missing locale, and rust-i18n globs it away silently. Negative-tested by
  deletion. "No user-facing string literal in a shell's view code" was *not* added — a text
  scan cannot tell `"Increment"` from a CSS class or a fixture, and
  `telemetry_guards_are_bound`'s doc comment is the precedent for saying so rather than
  shipping a scan wrong in both directions.

## Consequences

The reactive story is exactly `tr(key)`: `translate` reads whatever locale it is handed and
rust-i18n's global is invisible to Leptos — a string looked up through it renders once and
never changes — so `tr` reads the `RwSignal<Locale>` from context and returns a subscribed
closure. The shells provide the signal at the mount point and pair every write with
`i18n::set_locale` via a named `apply_locale`, because the two halves getting out of step
is invisible: reactive text in one language, anything non-reactive in another.

**Nothing chooses the locale, on purpose.** The signal is seeded with `FALLBACK`; there is
no URL scheme, no `navigator.language`, no stored preference. Those are the three ways a
product picks a locale and the template ships none of them — `path::split` / `with_locale`
are generated and ready for the first. `fishing`'s `localized_routes!` macro (route tree
declared twice, unprefixed + `/:locale`, self-validated segment, `replace: true` redirects,
a language picker that navigates rather than writes the signal) depends on `leptos_router`,
which this repository does not use: both web surfaces are single-page CSR mounts with no
routes at all. Porting it would have put a routing architecture into a template that cannot
show it working.

[0015](0015-the-shell-detects-the-locale.md) has since filled the first of those gaps: the
shells detect the system locale, which is the source `navigator.language` itself reads. The
URL scheme and the stored preference stay unshipped, for the reasons in this paragraph.

**The core's strings stay English.** `ViewModel { count, status }` is `String`-typed and
crosses the FFI into Kotlin and Swift, and changing that is a view-model change in five
shells plus `boltffi` typegen, revising the rationale in the web shell's own comment — it
needs its own ADR and its own review, not a ride in a mechanical port. Stage boundaries:
if a port of this one ever touches the core, `ffi.rs`, or a generated mobile file, it has
become the other change.

`locales/` is a plain TOML tree in a crate, so a Kotlin or Swift shell can have it converted
rather than dug out of a wasm frontend — which is `fishing`'s stated reason for the crate
existing at all, and survives the port.

Two things the port found that are not obvious from the shape: **the generated code must be
clean under the consumer's lints, not the source repository's.** `fishing`'s workspace does
not run `pedantic`/`nursery`; this one does, under `-D warnings`, and `include!` at a crate
root means the generated file inherits them. `emit.rs` now emits `#[must_use]` and
lint-clean bodies so consumers get the code they have to compile. And `crates/i18n`'s
inherited `leptos` entry carries `csr`, which the 30-line adapter does not need on its own;
the alternative was a second version literal, which is the thing this record's own entry
above exists to prevent.

## What was rejected, and why

**`leptos_i18n` and `leptos_i18n_router`.** The integrated option, and the reason the
hand-rolled generator looks strange. `fishing`'s `localized_routes!` doc comment records
why `fishing` itself did not use the router crate: its source opens with "this whole file
is a hack into leptos_router, it absolutely shouldn't be used like that". That reasoning
belongs in a product that has a router; this repository has none, so it was rejected twice
over.

**Translating in the core.** `crates/i18n` into `counter-core` and the smallest possible
diff. Rejected for this stage: the core would need the current locale as an *input*, so
every shell feeds it and a Crux core acquires a presentation concern. `cores_stay_domain_
level` would not have stopped it — `i18n` is not on the forbidden list — which is worth
noting because an invariant passing is not the same as a design being right. It comes back
as stage 2 with its own ADR, where the recommended shape is the core emitting keys.

**One crate holding generator and translations together.** The split is what lets the
namespace and drift rules be unit-tested against fixture trees — a guard that fires on the
real tree cannot be demonstrated on the real tree without breaking the build that ships it.

**`leptos` in `versions.toml` with a `cargo_pins_match` 4-tuple.** Heavier, and right only
if something outside Cargo also needs the number. Trunk does not.

**`Drift::Deny` in `build.rs`.** A half-translated page fails the build rather than
rendering. Considered deliberately per the port plan and rejected in favour of `Warn` plus
the coverage test, because the test names every missing key at once instead of failing on
the first — which is the better arrangement while the tree is a template's, and a product
that would rather never ship partial translations has a one-line knob.
