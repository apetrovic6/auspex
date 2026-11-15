//! Why generation failed.

use std::path::PathBuf;
use std::{fmt, io};

/// Everything that can stop a generation run.
///
/// The build script this replaced used `panic!` and `assert!`, whose messages
/// were written to be read in a `cargo build` log. They are kept verbatim as
/// the [`Display`](fmt::Display) text, so `.expect("generating i18n keys")`
/// still prints what it always did. The variants exist for the other caller —
/// a test, which wants to know *which* rule was broken without matching on
/// prose.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A locale directory or file could not be read, or the generated file
    /// could not be written.
    Io {
        /// `reading` or `writing`, so the message reads as a sentence.
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// A locale file is not valid TOML.
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// No file for the fallback locale, which is where the keys come from.
    MissingFallback {
        locales_dir: PathBuf,
        locale: String,
    },
    /// Keys defined outside the namespace their directory claims. One string
    /// per offending key.
    Namespace(Vec<String>),
    /// Two directories reducing to the same namespace. One string per clash.
    NamespaceClash(Vec<String>),
    /// A key that is also the start of another key — `settings` alongside
    /// `settings.title`. One string per pair.
    KeyPrefix(Vec<String>),
    /// Drift between the fallback locale and another, under
    /// [`Drift::Deny`](crate::Drift::Deny). One string per divergent key.
    Drift(Vec<String>),
    /// A locale defines a key — or a whole plural family — the fallback locale does
    /// not, in a [`crate::resources`] table. One string per offending key. A category
    /// of a family the fallback *does* declare is not an offender; see the rule in
    /// [`crate::resources`]'s module docs.
    CanonicalKeys(Vec<String>),
    /// A plural family a locale defines but cannot look up, in a
    /// [`crate::resources`] table: it has no `other` for that locale. One string per
    /// offending locale and family.
    Plurals(Vec<String>),
    /// A locale uses a plural category its CLDR cardinal rules never select, or is
    /// missing one it needs. One string per offender. Checked at generation time,
    /// where the locale files are, rather than at emission.
    PluralCategories(Vec<String>),
    /// A key that cannot become an Android resource name, or two that would
    /// become the same one — generated against generated, or generated against
    /// a committed hand-written name. One string per offender.
    AndroidNames(Vec<String>),
    /// A locale code Android cannot be given a qualified `values-*` directory
    /// for. One string per offending locale.
    AndroidLocaleCodes(Vec<String>),
    /// A translation uses a placeholder the fallback locale's string for the
    /// same key does not. One string per offending placeholder.
    Placeholders(Vec<String>),
    /// [`Generator::emit`](crate::Generator::emit) was called outside a build
    /// script, so there is nowhere to put the file.
    NoOutDir,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                source,
            } => write!(f, "{action} {}: {source}", path.display()),
            Self::Toml { path, source } => write!(f, "parsing {}: {source}", path.display()),
            Self::MissingFallback {
                locales_dir,
                locale,
            } => write!(
                f,
                "no {}/**/{locale}.toml — the fallback locale is the key source",
                locales_dir.display()
            ),
            Self::Namespace(offenders) => write!(
                f,
                "locale keys must be namespaced by their directory:\n{}\n\n\
                 rust-i18n flattens every file into one key space, so an unprefixed key \
                 here can silently overwrite another page's.",
                offenders.join("\n")
            ),
            Self::NamespaceClash(clashes) => write!(
                f,
                "two directories resolve to the same key namespace:\n{}\n\n\
                 they would share one key space, so one file's keys would silently \
                 overwrite the other's.",
                clashes.join("\n")
            ),
            Self::KeyPrefix(offenders) => write!(
                f,
                "a key cannot also be the start of another:\n{}\n\n\
                 one is a string and the other a table, so they cannot both exist in \
                 the flat map rust-i18n keeps per locale. Merging the files overwrites \
                 one with the other, and which one survives is glob order.",
                offenders.join("\n")
            ),
            Self::Drift(lines) => f.write_str(&refusal(
                "locales have drifted from the fallback",
                &lines
                    .iter()
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>(),
                "`Drift::Warn` reports these as cargo warnings instead.",
            )),
            Self::CanonicalKeys(offenders) => f.write_str(&refusal(
                "a locale defines a key the fallback does not",
                offenders,
                "the fallback locale is the canonical key set; its resources define \
                 what can be referenced at all, so a translation of a key that does \
                 not exist there has nothing to translate. A plural family is \
                 canonical as a family: `apples.few` may exist in Croatian without \
                 existing in English, because English has no such category, but a \
                 family the fallback never declares is still a key nobody emits.",
            )),
            Self::Plurals(families) => f.write_str(&refusal(
                "a locale defines a plural family it cannot look up",
                families,
                "every platform selects a category at runtime — Android's `<plurals>`, \
                 Apple's `variations.plural`, `i18n::plural_category` in a Rust shell \
                 — and each of them needs `other` as the category it falls back to. \
                 A family without it is an Android build failure, or a resource that \
                 cannot be referenced.",
            )),
            Self::PluralCategories(offenders) => f.write_str(&refusal(
                "plural categories disagree with CLDR",
                offenders,
                "the categories a language has are CLDR's answer for it, not the six \
                 that exist: Croatian selects `one`, `few` and `other`, English only \
                 `one` and `other`. A category a locale never selects is accepted by \
                 every platform and rendered by none; a category it does select that \
                 nobody wrote renders the `other` form instead. Both are silent \
                 misrenders, which this generator refuses rather than warns about.",
            )),
            Self::AndroidNames(offenders) => f.write_str(&refusal(
                "Android resource names must not collide",
                offenders,
                "library and app resources merge into one namespace and the app's \
                 own value wins, so a collision that is not stopped here resolves \
                 silently — last writer, or hand-written over generated.",
            )),
            Self::AndroidLocaleCodes(offenders) => write!(
                f,
                "locale codes Android cannot be given a qualified directory for:\n{}",
                offenders.join("\n")
            ),
            Self::Placeholders(offenders) => write!(
                f,
                "placeholders disagree with the fallback locale:\n{}\n\n\
                 positional arguments are numbered by their order of first occurrence \
                 in the fallback string; a placeholder the fallback lacks has no \
                 number, and inventing one would render arguments in a different \
                 order per language.",
                offenders.join("\n")
            ),
            Self::NoOutDir => f.write_str(
                "OUT_DIR is unset — `emit` writes where cargo tells it to, so it only \
                 works from a build script; `generate` returns the source instead.",
            ),
        }
    }
}

/// A refusal in this crate's voice: a headline, the offenders underneath, and what goes
/// wrong if a build lets it through.
///
/// The plural pair are the reason it exists: they were one message until ADR 0018 —
/// "plurals are not supported" — which is how a feature gets deleted rather than fixed, so
/// each of the two now says which failure it is and what it costs. The arms shaped like
/// them were written through it afterwards, which keeps `fmt` inside clippy's length limit
/// without shortening a sentence to fit a lint; the arms whose shape is not this one are
/// untouched, wording and all.
fn refusal(headline: &str, offenders: &[String], why: &str) -> String {
    format!("{headline}:\n{}\n\n{why}", offenders.join("\n"))
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Toml { source, .. } => Some(source),
            _ => None,
        }
    }
}
