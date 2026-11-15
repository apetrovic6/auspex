//! Turns `locales/**/*.toml` into `Key`, `Locale` and the runtime wrappers.
//!
//! The work itself lives in `i18n-keygen`, where the namespace and drift rules
//! can be unit-tested against fixture trees; this file is only the
//! configuration, which is the part that is specific to these translations.

fn main() {
    i18n_keygen::Generator::new("locales")
        // English is the fallback here — `fishing` falls back to Croatian, but a
        // product instantiated from this template is written in English first and
        // translated second, so the canonical key set is the English file. Two
        // locales ship because one cannot exercise a fallback: `Locale::ALL` with a
        // single variant makes every drift test vacuous, and `hr` is the second
        // because it is the one the source of this crate was proved against.
        //
        // Must match the `fallback` given to `i18n!` in src/lib.rs; nothing at
        // compile time ties the two, so a test does (see src/lib.rs).
        .fallback("en")
        // `locales/pages/` mirrors a `src/pages/` layout and names nothing;
        // `common/` holds the shared chrome under bare keys — `increment`, not
        // `common.increment`.
        .grouping_dirs(["pages", "common"])
        // Drift stays `Warn`: a missing translation still renders, in the fallback
        // language. `fishing` closes the gap with `every_locale_translates_every_key`
        // rather than failing the build, because the test names every missing key at
        // once instead of failing on the first.
        .emit()
        .expect("generating i18n keys");
}
