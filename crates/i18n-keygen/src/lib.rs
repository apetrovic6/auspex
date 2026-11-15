//! Generates `Key`, `Locale` and the runtime wrappers around them from a
//! rust-i18n locale tree, so translation keys and locale codes are checked by
//! the compiler instead of being loose strings.
//!
//! ```no_run
//! // build.rs
//! i18n_keygen::Generator::new("locales")
//!     .fallback("hr")
//!     .grouping_dirs(["pages"])
//!     .emit()
//!     .expect("generating i18n keys");
//! ```
//!
//! # Why this emits wrappers instead of providing them
//!
//! rust-i18n's macros are crate-local by construction. `i18n!` defines
//! `_RUST_I18N_BACKEND`, `_rust_i18n_backend()` and a `pub(crate)`
//! `_rust_i18n_t` at the *invoking* crate's root, and `t!` expands to
//! `crate::_rust_i18n_t!(...)`. So a function that calls `t!` cannot live in a
//! library at all — not this one, not any other. Emitting it as source into the
//! crate that called `i18n!` is the only shape that works, which is why
//! [`translate`-and-friends] are generated text here rather than functions you
//! could call.
//!
//! For the same reason the generated file has to be `include!`d at the
//! consuming crate's **root**, not inside a `mod`.
//!
//! [`translate`-and-friends]: Generator::translate_wrappers
//!
//! # The rules it enforces
//!
//! The fallback locale is canonical: its keys are the ones that exist. Other
//! locales are compared against it and any divergence is reported according to
//! [`Drift`] — missing translations fall back at runtime, so they are worth
//! knowing about but not usually worth failing a build over.
//!
//! Plural families (`apples.one`, `apples.other`, `apples.few`) are one entry, not three
//! keys, and their categories are checked against CLDR rather than against the fallback:
//! `apples.two` in Croatian builds, ships, and is selected by nothing. ADR 0018.
//!
//! A key in the wrong namespace *is* worth failing over, as are plural categories,
//! which are the hard errors here. rust-i18n globs `locales/**/*.toml` and flattens every file it
//! finds into a single key space — the directory contributes nothing to the
//! key. So `login/hr.toml` and `lakes/hr.toml` can both define `header`, and
//! the loser is decided by glob order with no diagnostic. Requiring each file
//! under `locales/<name>/` to prefix its keys with `<name>.` is what keeps a
//! per-page split from turning into silent overwrites.

mod check;
mod emit;
mod error;
mod locales;
pub mod resources;

use std::path::PathBuf;
use std::{env, fs};

pub use crate::emit::variant_name;
pub use crate::error::Error;

/// The file [`Generator::emit`] writes into `OUT_DIR`.
pub const OUTPUT_FILE: &str = "i18n_generated.rs";

/// What to do about keys that one locale has and the fallback does not, or the
/// other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Drift {
    /// One `cargo::warning` per divergent key. The default: a missing
    /// translation still renders, in the fallback language, so it is a defect
    /// rather than a broken build.
    #[default]
    Warn,
    /// Fail generation with [`Error::Drift`]. For a project that would rather
    /// not ship a half-translated page at all.
    Deny,
    /// Say nothing — for a locale that is deliberately partial.
    Ignore,
}

/// The generated source, and anything worth saying about it.
///
/// [`Generator::generate`] hands this back instead of printing, so a test can
/// assert on the warnings that a build script would otherwise only scroll past.
#[derive(Debug, Clone)]
pub struct Generated {
    /// The contents of `i18n_generated.rs`.
    pub source: String,
    /// Drift lines, already worded as `cargo::warning` text.
    pub warnings: Vec<String>,
}

/// Configures a generation run.
///
/// Everything but the locales directory has a default, and the defaults are the
/// permissive end of each choice: warn about drift, and emit every optional
/// section. A consumer that wants its own path helpers or its own `translate`
/// turns those off rather than fighting a duplicate definition.
#[derive(Debug, Clone)]
pub struct Generator {
    pub(crate) locales_dir: PathBuf,
    pub(crate) fallback: String,
    pub(crate) grouping_dirs: Vec<String>,
    pub(crate) drift: Drift,
    pub(crate) doc_comments: bool,
    pub(crate) path_helpers: bool,
    pub(crate) translate_wrappers: bool,
    pub(crate) crate_name: String,
}

impl Generator {
    /// A generator reading `locales_dir`, which a build script gives relative
    /// to its own manifest — the same way `i18n!` resolves its argument, so the
    /// two cannot end up looking at different trees.
    pub fn new(locales_dir: impl Into<PathBuf>) -> Self {
        Self {
            locales_dir: locales_dir.into(),
            fallback: "en".to_owned(),
            grouping_dirs: Vec::new(),
            drift: Drift::default(),
            doc_comments: true,
            path_helpers: true,
            translate_wrappers: true,
            // The generated doctest has to name the crate the file lands in,
            // and in a build script that is exactly CARGO_PKG_NAME. Cargo
            // always sets it; the literal is only so `generate` stays callable
            // from a plain test.
            crate_name: env::var("CARGO_PKG_NAME")
                .unwrap_or_else(|_| "crate".to_owned())
                .replace('-', "_"),
        }
    }

    /// The locale whose keys are canonical. Defaults to `en`.
    ///
    /// This must be the locale `i18n!` was given as its `fallback`; the two are
    /// written separately because `i18n!` needs a literal.
    #[must_use]
    pub fn fallback(mut self, locale: impl Into<String>) -> Self {
        self.fallback = locale.into();
        self
    }

    /// Directories that organize files without naming anything.
    ///
    /// `locales/pages/login/hr.toml` can mirror a `src/pages/` layout, but
    /// `pages` is then a filing decision rather than part of what the key
    /// means — including it would give `pages.login.title` and
    /// `Key::PagesLoginTitle` for no gain. Components named here drop out of
    /// the namespace, so that file still owns `login.*`.
    ///
    /// Two directories cannot end up claiming the same namespace; that is
    /// checked, because it would reintroduce exactly the silent overwrite the
    /// namespace rule exists to prevent.
    #[must_use]
    pub fn grouping_dirs<I, S>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.grouping_dirs = dirs.into_iter().map(Into::into).collect();
        self
    }

    /// How to report a locale that has drifted from the fallback. Defaults to
    /// [`Drift::Warn`].
    #[must_use]
    pub const fn drift(mut self, policy: Drift) -> Self {
        self.drift = policy;
        self
    }

    /// Whether each `Key` variant carries the fallback locale's copy as its doc
    /// comment. On by default, so hovering `Key::LoginTitle` shows what it
    /// actually says.
    #[must_use]
    pub const fn doc_comments(mut self, enabled: bool) -> Self {
        self.doc_comments = enabled;
        self
    }

    /// Whether to emit the `path` module — `split`, `locale_of`,
    /// `with_locale`, for locale-prefixed URLs. On by default.
    #[must_use]
    pub const fn path_helpers(mut self, enabled: bool) -> Self {
        self.path_helpers = enabled;
        self
    }

    /// Whether to emit `translate`, `try_translate`, `available_locales`,
    /// and `translate_with!`. On by default.
    ///
    /// These are what a dependent crate calls in place of `t!`, which it cannot
    /// reach across a crate boundary.
    #[must_use]
    pub const fn translate_wrappers(mut self, enabled: bool) -> Self {
        self.translate_wrappers = enabled;
        self
    }

    /// The crate the generated file will be `include!`d into.
    ///
    /// Only the doctest on `translate_with!` needs it, and it defaults to
    /// `CARGO_PKG_NAME`, which is right for every build script.
    #[must_use]
    pub fn crate_name(mut self, name: impl Into<String>) -> Self {
        self.crate_name = name.into().replace('-', "_");
        self
    }

    /// Reads the locale tree and returns the source, without writing or
    /// printing anything.
    ///
    /// # Errors
    ///
    /// Any of [`Error`]: an unreadable or malformed locale file, a namespace
    /// violation, a missing fallback locale, or drift under [`Drift::Deny`].
    pub fn generate(&self) -> Result<Generated, Error> {
        let files = locales::read_locale_files(&self.locales_dir, &self.grouping_dirs)?;

        check::check_namespaces(&files)?;
        check::check_key_prefixes(&files)?;

        let locales = locales::group_by_locale(&files);

        // Plural categories are checked against CLDR, not against the fallback, and
        // under every drift policy: a family `hr` covers badly is a wrong sentence for
        // the count two, not a missing translation. ADR 0018.
        check::plurals_match_cldr(&self.fallback, &locales)?;

        let canonical = locales
            .get(&self.fallback)
            .ok_or_else(|| Error::MissingFallback {
                locales_dir: self.locales_dir.clone(),
                locale: self.fallback.clone(),
            })?;

        let warnings = match self.drift {
            Drift::Ignore => Vec::new(),
            Drift::Warn => check::drift(&self.fallback, canonical, &locales),
            Drift::Deny => match check::drift(&self.fallback, canonical, &locales) {
                lines if lines.is_empty() => Vec::new(),
                lines => return Err(Error::Drift(lines)),
            },
        };

        let codes = locales.keys().cloned().collect::<Vec<_>>();

        let entries = resources::catalog_entries(&locales, &self.fallback);

        Ok(Generated {
            source: emit::generate(self, &entries, canonical, &codes),
            warnings,
        })
    }

    /// The generated source on its own, for a test that only cares what came
    /// out.
    ///
    /// # Errors
    ///
    /// As [`generate`](Self::generate).
    pub fn generate_to_string(&self) -> Result<String, Error> {
        self.generate().map(|generated| generated.source)
    }

    /// Writes `$OUT_DIR/i18n_generated.rs` and prints the cargo directives.
    ///
    /// This is the build-script entry point: it is the only method that reads
    /// `OUT_DIR` or writes to stdout.
    ///
    /// # Errors
    ///
    /// As [`generate`](Self::generate), plus [`Error::NoOutDir`] when called
    /// from outside a build script and [`Error::Io`] if the file cannot be
    /// written.
    pub fn emit(&self) -> Result<(), Error> {
        // Re-run when a translation is added, removed or renamed. Cargo scans a
        // directory path recursively, so per-page subdirectories are covered.
        println!("cargo::rerun-if-changed={}", self.locales_dir.display());
        println!("cargo::rerun-if-changed=build.rs");

        let generated = self.generate()?;

        for warning in &generated.warnings {
            println!("cargo::warning={warning}");
        }

        let out_dir = env::var_os("OUT_DIR").ok_or(Error::NoOutDir)?;
        let path = PathBuf::from(out_dir).join(OUTPUT_FILE);

        fs::write(&path, generated.source).map_err(|source| Error::Io {
            action: "writing",
            path,
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A throwaway locale tree under the system temp directory.
    ///
    /// The guards being tested are all about the *shape* of a tree — a key in
    /// the wrong directory, two directories colliding — which the real
    /// `i18n/locales` cannot express without breaking the build that ships it.
    /// Before these fixtures the only way to see a guard fire was to corrupt
    /// that tree by hand and read the cargo log.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);

            // Tests share a process and run in parallel, so the pid alone is
            // not enough to keep two fixtures apart.
            let unique = format!(
                "i18n-keygen-{}-{name}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );

            let root = env::temp_dir().join(unique).join("locales");
            fs::create_dir_all(&root).expect("creating the fixture root");

            Self { root }
        }

        fn file(self, relative: &str, contents: &str) -> Self {
            let path = self.root.join(relative);

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("creating a fixture directory");
            }
            fs::write(&path, contents).expect("writing a fixture file");

            self
        }

        fn generator(&self) -> Generator {
            Generator::new(&self.root).fallback("hr")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // The fixture root is `<unique>/locales`; take the whole thing.
            if let Some(parent) = self.root.parent() {
                let _ = fs::remove_dir_all(parent);
            }
        }
    }

    /// What a plural family becomes in Rust: one variant named for the family, and its
    /// members as strings in a catalog.
    ///
    /// There is no `Key::ApplesFew`. A variant per category would be a different enum for
    /// every locale that ships — Croatian has a `few`, English does not — and `Key` is one
    /// type for all of them (ADR 0018). So the enum carries the family, which is what a
    /// call site can name without knowing any language's rules, and `CATALOG` carries the
    /// strings, which is what a driver enumerating translations needs.
    #[test]
    fn a_plural_family_is_one_variant_and_several_catalog_strings() {
        let fixture = Fixture::new("plural-family")
            .file(
                "hr.toml",
                "apples.one = \"%{count} jabuka\"\napples.few = \"%{count} jabuke\"\n\
                     apples.other = \"%{count} jabuka\"\n",
            )
            .file(
                "en.toml",
                "apples.one = \"%{count} apple\"\napples.other = \"%{count} apples\"\n",
            );

        let generated = fixture.generator().generate().expect("generating");
        let source = generated.source;
        assert!(generated.warnings.is_empty(), "{:#?}", generated.warnings);

        assert!(source.contains("\n    Apples,\n"), "{source}");
        assert!(
            !source.contains("ApplesOne") && !source.contains("ApplesFew"),
            "{source}"
        );
        // The family, not a member: `as_str` answers `apples`, which is not a key, and
        // naming it renders it. `CATALOG` is what knows `apples.few` exists.
        assert!(source.contains("Self::Apples => \"apples\""), "{source}");
        assert!(
            source.contains("(\"apples.one\", \"apples\")")
                && source.contains("(\"apples.few\", \"apples\")")
                && source.contains("(\"apples.other\", \"apples\")"),
            "{source}"
        );
        assert!(source.contains("= &[\"apples\"];"), "{source}");
        // Stated on the variant, because a call site holding `Key::Apples` cannot tell
        // from the name that it is a family rather than a string.
        assert!(
            source.contains("is a plural family, not a string"),
            "{source}"
        );
        assert!(
            source.contains("plural_key(Key::Apples, count, locale)"),
            "{source}"
        );
    }

    /// `apples.two` compiles, ships, and is selected by nothing: CLDR gives Croatian
    /// `one`, `few` and `other` for cardinals, and no `n%10` rules written into the locale
    /// files change that. A category that is simply *missing* fails identically, so the
    /// check is against CLDR rather than against the fallback — which cannot be the
    /// reference for a language it does not speak.
    #[test]
    fn a_plural_category_a_locale_does_not_have_is_an_error() {
        let fixture = Fixture::new("plural-category")
            .file(
                "hr.toml",
                "apples.one = \"%{count} jabuka\"\napples.two = \"%{count} dvije jabuke\"\n\
                     apples.few = \"%{count} jabuke\"\napples.other = \"%{count} jabuka\"\n",
            )
            .file(
                "en.toml",
                "apples.one = \"%{count} apple\"\napples.other = \"%{count} apples\"\n",
            );

        let error = fixture
            .generator()
            .generate()
            .expect_err("`two` is not a Croatian cardinal category");

        assert!(matches!(error, Error::PluralCategories(_)), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("`hr` defines `apples.two`"), "{message}");
        assert!(message.contains("one, few, other"), "{message}");
        // English is not implicated: it defines neither category.
        assert!(!message.contains("`en` defines"), "{message}");
    }

    /// The shape of a real mistake: someone copies the fallback's two categories, sees it
    /// build, and the number two silently renders `other`.
    #[test]
    fn a_plural_category_a_locale_omits_is_an_error_too() {
        let fixture = Fixture::new("plural-category-missing").file(
            "hr.toml",
            "apples.one = \"%{count} jabuka\"\napples.other = \"%{count} jabuka\"\n",
        );

        let error = fixture
            .generator()
            .generate()
            .expect_err("Croatian needs `few` for the number two");

        assert!(matches!(error, Error::PluralCategories(_)), "{error:?}");
        let message = error.to_string();
        assert!(
            message.contains("`hr` defines `apples` without `apples.few`"),
            "{message}"
        );
        assert!(message.contains("one, few, other"), "{message}");
    }

    /// A family the fallback never declares is nobody's contract, so nothing is measured
    /// against CLDR for it — and it does not become a variant either. It stays what an
    /// unlanded translation always was: a warning.
    #[test]
    fn a_family_only_a_non_fallback_locale_declares_is_not_a_variant() {
        let fixture = Fixture::new("plural-invented")
            .file("hr.toml", "lakes = \"jezera\"\n")
            .file(
                "en.toml",
                "lakes = \"lakes\"\npears.one = \"%{count} pear\"\npears.other = \"%{count} pears\"\n",
            );

        let generated = fixture.generator().generate().expect("generating");

        assert!(!generated.source.contains("Pears"), "{}", generated.source);
        assert!(
            generated
                .warnings
                .iter()
                .any(|warning| warning.contains("pears.one")),
            "{:#?}",
            generated.warnings
        );
    }

    #[test]
    fn generates_the_keys_and_locales_it_found() {
        let fixture = Fixture::new("happy")
            .file("hr.toml", "lakes = \"jezera\"\n")
            .file("en.toml", "lakes = \"lakes\"\n")
            .file("pages/login/hr.toml", "[login]\ntitle = \"Prijava\"\n")
            .file("pages/login/en.toml", "[login]\ntitle = \"Log in\"\n");

        let generated = fixture
            .generator()
            .grouping_dirs(["pages"])
            .generate()
            .expect("generating");

        assert!(generated.warnings.is_empty(), "{:#?}", generated.warnings);

        assert!(generated.source.contains("    Lakes,"));
        assert!(generated.source.contains("    LoginTitle,"));
        assert!(
            generated
                .source
                .contains("pub const FALLBACK_LOCALE: &str = \"hr\";")
        );
        assert!(
            generated
                .source
                .contains("pub const FALLBACK: Self = Self::Hr;")
        );

        // `pages` is a filing decision, so it is not part of the key.
        assert!(!generated.source.contains("PagesLogin"));
    }

    #[test]
    fn doc_comments_carry_the_fallback_copy_and_can_be_turned_off() {
        let fixture = Fixture::new("docs").file("hr.toml", "lakes = \"jezera\"\n");

        let with = fixture
            .generator()
            .generate_to_string()
            .expect("generating");
        assert!(with.contains("/// \"jezera\""));

        let without = fixture
            .generator()
            .doc_comments(false)
            .generate_to_string()
            .expect("generating");
        assert!(!without.contains("jezera"));
        // The key itself still documents the variant.
        assert!(without.contains("/// `lakes`"));
    }

    #[test]
    fn optional_sections_can_be_left_out() {
        let fixture = Fixture::new("sections").file("hr.toml", "lakes = \"jezera\"\n");

        let all = fixture
            .generator()
            .generate_to_string()
            .expect("generating");
        assert!(all.contains("pub mod path {"));
        assert!(all.contains("pub fn translate("));

        let bare = fixture
            .generator()
            .path_helpers(false)
            .translate_wrappers(false)
            .generate_to_string()
            .expect("generating");
        assert!(!bare.contains("pub mod path {"));
        assert!(!bare.contains("pub fn translate("));
        // The non-optional half is still there.
        assert!(bare.contains("pub enum Key {"));
    }

    #[test]
    fn a_key_outside_its_directory_namespace_is_an_error() {
        let fixture = Fixture::new("namespace")
            .file("hr.toml", "lakes = \"jezera\"\n")
            .file(
                "login/hr.toml",
                // Ordered so `header` lands at the top level: a key written
                // after `[login]` would belong to that table and be fine.
                "header = \"Zaglavlje\"\n\n[login]\ntitle = \"Prijava\"\n",
            );

        let error = fixture
            .generator()
            .generate()
            .expect_err("an unprefixed key must not generate");

        let Error::Namespace(offenders) = &error else {
            panic!("expected a namespace error, got {error:?}");
        };

        assert_eq!(offenders.len(), 1, "{offenders:#?}");
        assert!(offenders[0].contains("login"), "{offenders:#?}");
        assert!(offenders[0].contains("`header`"), "{offenders:#?}");
        assert!(offenders[0].contains("`login.`"), "{offenders:#?}");

        // The message names the file, which is the only way to find the key.
        assert!(error.to_string().contains("hr.toml"));
    }

    /// The exact shape that reached the fishing app: a nav label in the common
    /// file, and a page directory of the same name. Each file is valid on its
    /// own; only the merge collides.
    #[test]
    fn a_common_key_colliding_with_a_page_namespace_is_an_error() {
        let fixture = Fixture::new("prefix")
            .file("hr.toml", "lakes = \"jezera\"\nsettings = \"postavke\"\n")
            .file(
                "pages/settings/hr.toml",
                "[settings]\ntitle = \"Postavke\"\n",
            );

        let error = fixture
            .generator()
            // Without this `pages` joins the namespace, and the directory-prefix
            // rule rejects the file first — for the wrong reason.
            .grouping_dirs(["pages"])
            .generate()
            .expect_err("a key that is also a namespace must not generate");

        let Error::KeyPrefix(offenders) = &error else {
            panic!("expected a key-prefix error, got {error:?}");
        };

        assert_eq!(offenders.len(), 1, "{offenders:#?}");
        assert!(offenders[0].contains("`settings`"), "{offenders:#?}");
        assert!(offenders[0].contains("`settings.title`"), "{offenders:#?}");

        // Both files, since the collision belongs to neither alone.
        assert!(error.to_string().contains("pages/settings"), "{error}");
    }

    /// Sorting alone would not settle this: `-` is `,` + 1 and sorts just
    /// before `.`, so a neighbouring key can look like a descendant.
    #[test]
    fn a_key_that_merely_sorts_nearby_is_not_a_prefix() {
        let fixture = Fixture::new("prefix-adjacent").file(
            "hr.toml",
            "settings = \"postavke\"\nsettings-old = \"stare\"\nsettingsx = \"x\"\n",
        );

        fixture
            .generator()
            .generate()
            .expect("keys that share a stem but not a dot boundary are fine");
    }

    #[test]
    fn two_directories_claiming_one_namespace_is_an_error() {
        let fixture = Fixture::new("clash")
            .file("hr.toml", "lakes = \"jezera\"\n")
            .file("login/hr.toml", "[login]\ntitle = \"Prijava\"\n")
            .file("pages/login/hr.toml", "[login]\nheader = \"Zaglavlje\"\n");

        let error = fixture
            .generator()
            .grouping_dirs(["pages"])
            .generate()
            .expect_err("two directories may not share a namespace");

        let Error::NamespaceClash(clashes) = &error else {
            panic!("expected a clash error, got {error:?}");
        };

        assert_eq!(clashes.len(), 1, "{clashes:#?}");
        assert!(clashes[0].contains("`login`"), "{clashes:#?}");

        // Both directories, because either one could be the one to move.
        let message = error.to_string();
        assert!(message.contains("locales/login"), "{message}");
        assert!(message.contains("locales/pages/login"), "{message}");
    }

    #[test]
    fn drift_is_reported_in_both_directions() {
        let fixture = Fixture::new("drift")
            .file("hr.toml", "lakes = \"jezera\"\nrivers = \"rijeke\"\n")
            .file("en.toml", "lakes = \"lakes\"\nseas = \"seas\"\n");

        let generated = fixture.generator().generate().expect("generating");

        assert_eq!(
            generated.warnings,
            [
                "locale `en` is missing the key `rivers`",
                "locale `en` has `seas`, which `hr` does not",
            ]
        );

        // The fallback is the key source, so `rivers` is a key even though
        // only one locale has it, and `seas` is not one at all — naming it
        // stays a compile error until `hr` grows the translation.
        assert!(generated.source.contains("    Rivers,"));
        assert!(!generated.source.contains("    Seas,"));
    }

    #[test]
    fn drift_can_be_denied_or_ignored() {
        let fixture = Fixture::new("drift-policy")
            .file("hr.toml", "lakes = \"jezera\"\nrivers = \"rijeke\"\n")
            .file("en.toml", "lakes = \"lakes\"\n");

        let error = fixture
            .generator()
            .drift(Drift::Deny)
            .generate()
            .expect_err("Deny must fail on drift");

        let Error::Drift(lines) = &error else {
            panic!("expected a drift error, got {error:?}");
        };
        assert_eq!(lines, &["locale `en` is missing the key `rivers`"]);

        let ignored = fixture
            .generator()
            .drift(Drift::Ignore)
            .generate()
            .expect("Ignore must not fail");
        assert!(ignored.warnings.is_empty(), "{:#?}", ignored.warnings);
    }

    #[test]
    fn a_locale_that_matches_the_fallback_does_not_drift() {
        let fixture = Fixture::new("no-drift")
            .file("hr.toml", "lakes = \"jezera\"\n")
            .file("en.toml", "lakes = \"lakes\"\n");

        let generated = fixture
            .generator()
            .drift(Drift::Deny)
            .generate()
            .expect("generating");

        assert!(generated.warnings.is_empty(), "{:#?}", generated.warnings);
    }

    #[test]
    fn a_missing_fallback_locale_is_an_error() {
        let fixture = Fixture::new("no-fallback").file("en.toml", "lakes = \"lakes\"\n");

        let error = fixture
            .generator()
            .generate()
            .expect_err("there is nothing to take the keys from");

        assert!(
            matches!(error, Error::MissingFallback { ref locale, .. } if locale == "hr"),
            "{error:?}"
        );
        assert!(error.to_string().contains("hr.toml"), "{error}");
    }

    #[test]
    fn a_missing_locales_directory_is_an_error() {
        let error = Generator::new(Path::new("definitely/not/here"))
            .fallback("hr")
            .generate()
            .expect_err("there is no tree to read");

        assert!(
            matches!(
                error,
                Error::Io {
                    action: "reading",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn malformed_toml_names_the_file() {
        let fixture = Fixture::new("bad-toml").file("hr.toml", "lakes = \n");

        let error = fixture
            .generator()
            .generate()
            .expect_err("invalid TOML must not generate");

        assert!(matches!(error, Error::Toml { .. }), "{error:?}");
        assert!(error.to_string().starts_with("parsing "), "{error}");
    }

    /// `_version` configures rust-i18n's file format; treating it as a message
    /// would put a `Key::Version` in the enum.
    #[test]
    fn the_version_marker_is_not_a_key() {
        let fixture = Fixture::new("version").file("hr.toml", "_version = 1\nlakes = \"jezera\"\n");

        let source = fixture
            .generator()
            .generate_to_string()
            .expect("generating");

        assert!(source.contains("    Lakes,"));
        assert!(!source.contains("Version,"));
    }

    /// Nested tables are how rust-i18n addresses `messages.hello`, and a
    /// non-string scalar is still a message.
    #[test]
    fn nested_tables_and_scalars_flatten_to_dotted_keys() {
        let fixture = Fixture::new("flatten")
            .file("hr.toml", "[messages]\nhello_world = \"Bok\"\ncount = 3\n");

        let source = fixture
            .generator()
            .generate_to_string()
            .expect("generating");

        assert!(source.contains("MessagesHelloWorld"));
        assert!(source.contains("=> \"messages.count\","));
    }
}
