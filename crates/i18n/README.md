# i18n

The translations, and the compiler-checked way to name them: `Key`, `Locale`,
`translate`, the `path` helpers, and — behind the `leptos` feature — the reactive `tr`.
Behind `format`, also `number`: writing an `i64` the way a locale writes numbers, which
is what the shells interpolate into `count_is` (ADR 0017).

Ported from `fishing`'s `i18n` crate; the locale *tree* here is this repository's own
(`locales/common/`, plus one `locales/pages/<app>/` per app — `just add` creates it, and
`cargo cli rename app` moves it with the app). English is the fallback, `hr` the second
locale — see `build.rs` for why.

## What belongs here

Translations for the whole product, in `locales/`, and the framework-free API over them.
A string that any shell renders lives here once; a shell's copy of a label does not.

What does **not** belong here: the generation rules (that is `crates/i18n-keygen` — the
crate this one drives from `build.rs`), any framework code outside `reactive.rs` behind
its feature, and any *policy* about how a locale gets chosen. The locale signal exists;
URLs, `navigator.language` and stored preferences are products, not templates.

## The two things to know before editing

- **Namespaces are hard errors.** A file under `locales/<name>/` must prefix its keys
  with `<name>.` because rust-i18n flattens every file into one key space and glob order
  decides the winner. `common/` and `pages/` are grouping directories and name nothing.
- **The fallback locale is written twice** — in `i18n!` and in `build.rs` — and only a
  test ties them together. Change one, run `cargo nextest run -p i18n`.
