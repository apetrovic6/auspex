//! Converting the translation table into native resource formats.
//!
//! The same TOML tree that generates `Key` and `Locale` also becomes Android
//! `strings.xml` and an Apple String Catalog, so the Kotlin and Swift shells stop
//! carrying their own copies of the same words. This module owns every conversion
//! rule that implies; deciding *where* the files land is the driver's job (an
//! xtask subcommand), because that depends on the monorepo layout and this does not.
//!
//! The input is a plain `locale → key → template` map rather than a locale
//! directory on purpose. The driver builds it from the compiled `i18n` crate —
//! `Locale::ALL × Key::ALL` through `try_translate`, which is what makes a key a
//! locale defines-but-the-fallback-does-not impossible here and keeps the fallback
//! locale and grouping directories from being configured a second time. Keeping
//! this module pure makes every rule below unit-testable against synthetic maps,
//! which is the same reason `check.rs` is testable at all.
//!
//! # The rules, and the silent failure each one prevents
//!
//! Every rule in this module fails the conversion rather than warning, because
//! each one, if it were merely wrong, would fail at runtime in a way nobody
//! notices: a truncated string, a locale silently ignored by the platform, a
//! resource that loses a merge to a hand-written one.
//!
//! - **Names** ([`android`]): dotted keys become underscored resource names, and
//!   any collision — generated against generated, or generated against a
//!   committed name — is an error naming both. Android merges resource sets and
//!   the app's own value silently wins.
//! - **Placeholders**: `%{name}` becomes indexed (`%1$s`, `%1$@`), and the index
//!   order is taken from the **fallback** locale and reused everywhere, so `%1$`
//!   means the same argument in every language. A translation with a placeholder
//!   the fallback lacks is an error; omitting one is allowed.
//! - **Escaping**: Android needs both XML and its own escape layer, and an
//!   unescaped apostrophe truncates the string without any error.
//! - **Locale codes**: legacy codes are mapped (`he` → `iw`), and anything that
//!   is not a bare two- or three-letter code is an error rather than a directory
//!   Android will silently ignore.
//! - **Plurals**: `apples.one`/`apples.other` become one Android
//!   `<plurals>` and one String Catalog entry with `variations.plural`. rust-i18n
//!   pluralizes nothing — the dotted form is a naming convention and nothing selects a
//!   category from a number — so the Rust shells select it themselves
//!   (`i18n::plural_category`, ADR 0018) and the platform resources exist for the
//!   Kotlin and Swift halves of the same screen. A plural emitted as a flat string is
//!   wrong at runtime only, in every language with more than two forms.
//! - **Plural families are canonical per family, not per key**: Croatian
//!   `apples.few` is a key English does not have and must not need, while a family the
//!   fallback never declared is still a typo.
//! - **Per-app emission** ([`for_app`]): an app's resources carry its own namespace and
//!   the common keys, and nothing else. Unfiltered, every app's `strings.xml` and
//!   `.xcstrings` carries every app's words — invisible at one app and four keys, APK
//!   bloat at two, and an unshipped feature name inside a released binary at twenty,
//!   which `aapt dump` reads straight back out.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::Error;

/// `locale → key → raw template`.
///
/// Exactly what `try_translate` answers: a locale is absent for a key when it
/// does not define it, and present with the uninterpolated template otherwise
/// (`%{count}` survives verbatim).
pub type Table = BTreeMap<String, BTreeMap<String, String>>;

/// The name of every generated Android file.
///
/// Not `strings.xml`: the generated and the hand-written sets must stay
/// distinguishable in a diff, because Android merges them into one namespace
/// regardless (see [`android`]'s `committed_names`).
pub const ANDROID_FILE: &str = "strings_i18n.xml";

/// The CLDR plural categories, in CLDR's order — the last segment that makes a key
/// family a plural.
///
/// rust-i18n has no opinion about them (it pluralizes nothing: `t!("apples.one")` is a
/// flat lookup), so this list is what the `key.<category>` convention means, spelled the
/// way CLDR, Android's `quantity` attribute and Apple's `variations.plural` all spell it.
/// The subset a *given* locale uses is CLDR's answer, not this list's — checked at
/// generation time in `check::plurals_match_cldr`.
const PLURAL_CATEGORIES: [&str; 6] = ["zero", "one", "two", "few", "many", "other"];

/// `<family>` and `<category>` of a key spelled `<family>.<category>` where the last
/// segment is a plural category; `None` for anything else.
///
/// This does not decide whether `family` *is* a plural family — [`plural_families`] does,
/// and the difference matters: `page.one` alongside no sibling is one key named `one`.
pub(crate) fn plural_member(key: &str) -> Option<(&str, &str)> {
    let (family, category) = key.rsplit_once('.')?;
    PLURAL_CATEGORIES
        .contains(&category)
        .then_some((family, category))
}

/// The plural families `messages` declares, whether or not they are families yet.
pub(crate) fn declared_families(messages: &BTreeMap<String, String>) -> BTreeSet<&str> {
    messages
        .keys()
        .filter_map(|key| plural_member(key).map(|(family, _)| family))
        .collect()
}

/// The categories of `family` that `messages` defines, by name.
pub(crate) fn family_categories<'a>(
    messages: &'a BTreeMap<String, String>,
    family: &str,
) -> BTreeSet<&'a str> {
    messages
        .keys()
        .filter_map(|key| {
            plural_member(key)
                .filter(|(f, _)| *f == family)
                .map(|(_, category)| category)
        })
        .collect()
}

/// The keys of `family` that `messages` defines, sorted in CLDR's category order rather
/// than alphabetically, so a `<plurals>` block reads `one`, `few`, `other` and a diff of
/// two generations does not move its items around.
fn family_members<'a>(messages: &'a BTreeMap<String, String>, family: &str) -> Vec<&'a str> {
    let mut members: Vec<&str> = messages
        .keys()
        .filter(|key| plural_member(key).is_some_and(|(f, _)| f == family))
        .map(String::as_str)
        .collect();

    members.sort_by_key(|key| {
        plural_member(key)
            .and_then(|(_, category)| PLURAL_CATEGORIES.iter().position(|c| *c == category))
            .unwrap_or(usize::MAX)
    });

    members
}

/// One entry of the catalog, as both the resource emitters and the `Key` enum emit it.
///
/// A plural family is *one* entry — one `<plurals>` block, one String Catalog key, one
/// `Key` variant — whose members are the `<family>.<category>` strings the locales
/// define, which need not be the same list in every language. That asymmetry is the
/// whole reason this enum exists rather than a `Vec<String>`: `apples.few` has no
/// English counterpart to be emitted next to, and flattening the family into the key
/// list is the flat-string bug. It is shared by the two paths deliberately — ADR 0016
/// has `Key` and the resource files generated from the same rules, and a family that
/// meant one thing in the enum and another in `values-hr/` is how they would drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    String(String),
    Plural {
        family: String,
        /// Every member any locale defines, in CLDR category order.
        members: Vec<String>,
    },
}

impl Entry {
    /// What the entry is called: its own key, or the family a plural is named by.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::String(key) => key,
            Self::Plural { family, .. } => family,
        }
    }

    /// Whether this entry is a plural family rather than one string.
    #[must_use]
    pub const fn is_plural(&self) -> bool {
        matches!(self, Self::Plural { .. })
    }

    /// The `(catalog key, name)` pairs the `Key` enum and the driver enumerate: one per
    /// key, where a family contributes all of its members under the family's name.
    #[must_use]
    pub fn catalog(&self) -> Vec<(&str, &str)> {
        match self {
            Self::String(key) => vec![(key.as_str(), key.as_str())],
            Self::Plural { family, members } => members
                .iter()
                .map(|member| (member.as_str(), family.as_str()))
                .collect(),
        }
    }
}

/// Android still expects these legacy codes on older API levels.
const LEGACY_ANDROID_CODES: [(&str, &str); 3] = [("he", "iw"), ("id", "in"), ("yi", "ji")];

/// The Android resource name for a key, or `None` when the key cannot become one.
///
/// Dots become underscores — `counter.title` → `counter_title` — and everything
/// else has to already be legal (`[a-zA-Z_][a-zA-Z0-9_]*`); a `-` is *not* mapped,
/// because `settings-old` and `settings_old` would then collide silently, which is
/// the very failure this function exists to catch.
fn android_name(key: &str) -> Option<String> {
    let name = key.replace('.', "_");

    let mut chars = name.chars();
    // An empty key cannot become a name either.
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        return None;
    }

    Some(name)
}

/// The placeholder names of `template` in order of first occurrence.
///
/// `%{name}` is rust-i18n's syntax; `translate` hands back the uninterpolated
/// template, so these are still intact. Any other `%` is literal.
fn placeholders(template: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = template;

    while let Some(at) = rest.find('%') {
        let tail = &rest[at + 1..];
        if let Some(close) = tail
            .find('}')
            .filter(|close| *close > 0 && tail.starts_with('{'))
        {
            let name = &tail[1..close];
            if !name.contains('%') && !names.contains(&name.to_owned()) {
                names.push(name.to_owned());
            }
            rest = &tail[close + 1..];
            continue;
        }
        rest = tail;
    }

    names
}

/// One `%`-token of a template: a named placeholder or a literal percent.
///
/// `%` followed by something that is not a well-formed `%{name}` is treated as
/// literal, and so is `%{` never closed — both become `%%` on both platforms,
/// which is the only rendering that survives both format engines.
enum Percent<'a> {
    Named(&'a str),
    Literal,
}

fn next_percent(template: &str) -> Option<(Percent<'_>, usize)> {
    let at = template.find('%')?;
    let tail = &template[at + 1..];

    if let Some(close) = tail
        .find('}')
        .filter(|close| *close > 0 && tail.starts_with('{') && !tail[1..*close].contains('%'))
    {
        // `%{name}`: the `%` at `at`, the `{` at `at + 1`, and the `}` at
        // `at + close + 1` are all consumed.
        return Some((Percent::Named(&tail[1..close]), at + close + 2));
    }

    Some((Percent::Literal, at + 1))
}

/// The `template` with every `%{name}` replaced by the *fallback-ordered* index
/// for that name and every literal `%` doubled.
///
/// `order` is the placeholder list of the **fallback** locale's string for this
/// key, so argument 1 is argument 1 in every language even when the sentence is
/// reordered. A name that is not in `order` is reported.
///
/// `marker` turns an index into the platform's positional form — `%1$s` or
/// `%1$@`. The emitted form is inserted as-is; it contains no character either
/// format engine treats as data.
fn format_placeholders(
    key: &str,
    locale: &str,
    template: &str,
    order: &[String],
    marker: &dyn Fn(usize) -> String,
    problems: &mut Vec<String>,
) -> String {
    let mut out = String::new();
    let mut rest = template;

    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);

        let (token, consumed) = next_percent(&rest[at..]).unwrap_or((Percent::Literal, 1));
        match token {
            Percent::Named(name) => match order.iter().position(|known| known == name) {
                Some(index) => out.push_str(&marker(index + 1)),
                None => problems.push(format!(
                    "  locale `{locale}` has placeholder `%{{{name}}}` in `{key}`, \
                     which the fallback locale does not — the fallback defines the \
                     argument order, so this placeholder has no index to take"
                )),
            },
            Percent::Literal => out.push_str("%%"),
        }

        rest = &rest[at + consumed..];
    }

    out.push_str(rest);
    out
}

/// Android's escaping: the XML layer (`&amp;` — which at least fails loudly when
/// wrong) plus Android's own layer on top, where an unescaped `'` truncates the
/// string with no error, a leading `@` reads as a resource reference, and a raw
/// newline collapses.
///
/// Backslashes are escaped too, beyond the plan's table: a copy containing `\`
/// starts an escape Android will consume, and dropping an unknown backslash is
/// another silent misrender. This runs *after* placeholder insertion, and is
/// never applied to the emitted `%1$s` itself — those tokens are built here,
/// appended whole.
fn escape_android(text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    for (at, c) in text.char_indices() {
        match c {
            '\\' => out.push_str("\\\\"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => {}
            // Only at the very start of the value: `@string/x` and `?attr/y`
            // references are only recognized there.
            '@' if at == 0 => out.push_str("\\@"),
            '?' if at == 0 => out.push_str("\\?"),
            c => out.push(c),
        }
    }

    out
}

/// The `values/` qualifier for a locale code: the fallback gets none, legacy
/// codes are mapped, and anything that is not a bare two- or three-letter
/// lowercase code is an error naming the locale.
///
/// Script and region qualifiers (`sr-Latn`, `en_US`) need `b+` BCP-47 syntax or
/// legacy `-rRR` handling Android will not always complain about; the tree has
/// none today, and emitting a directory name Android silently ignores is the
/// failure mode worth being loud about.
fn android_qualifier(locale: &str, fallback: &str) -> Result<String, ()> {
    if locale == fallback {
        return Ok(String::new());
    }

    let code = LEGACY_ANDROID_CODES
        .iter()
        .find(|(from, _)| *from == locale)
        .map_or(locale, |(_, to)| *to);

    let chars: Vec<char> = code.chars().collect();
    if !(2..=3).contains(&chars.len()) || !chars.iter().all(char::is_ascii_lowercase) {
        return Err(());
    }

    Ok(format!("-{code}"))
}

/// Keys that look like rust-i18n plurals: `<family>.<category>` with at least two
/// sibling categories (one category alone is just a key named `one` or `other`).
///
/// This is a deliberate over-refusal: the shape is the only thing the flattened
/// key space leaves to go on, and guessing wrong ships a plural as a flat string,
/// which is wrong in every language with more than two forms and wrong only at
/// runtime.
pub(crate) fn plural_families<'a>(keys: impl Iterator<Item = &'a str>) -> BTreeSet<String> {
    let mut families: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

    for key in keys {
        if let Some((family, last)) = plural_member(key) {
            families.entry(family).or_default().insert(last);
        }
    }

    families
        .into_iter()
        .filter(|(_, categories)| categories.len() >= 2)
        .map(|(family, _)| family.to_owned())
        .collect()
}

/// The shared front half of both conversions: the fallback must be present, the
/// **families** of the fallback are canonical, and every plural family is complete
/// enough to be looked up.
///
/// Canonical-per-family is the subtle half, and it is a real change to a load-bearing
/// check (ADR 0018). Per key, the rule is "a locale may not define a key the fallback
/// lacks", and a *correct* plural table breaks it by construction: `apples.few` is a
/// Croatian category English has no word for, so no English `apples.few` exists to be
/// the canonical one. Weakening the rule is not the answer — it is what catches a
/// mistranslated key. So the fallback stays canonical **per family**: any category of a
/// family the fallback declares is allowed, in any locale, and a family the fallback
/// never declared is still refused, because then nothing in the base resource set
/// references it and every other locale silently renders its key.
fn validate(table: &Table, fallback: &str) -> Result<Vec<Entry>, Error> {
    if !table.contains_key(fallback) {
        return Err(Error::MissingFallback {
            locales_dir: PathBuf::from("the translation table"),
            locale: fallback.to_owned(),
        });
    }

    let canonical = &table[fallback];

    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for keys_in_locale in table.values() {
        keys.extend(keys_in_locale.keys().map(String::as_str));
    }

    // Families are decided over *every* locale, since the category that makes two keys
    // siblings may be one only Croatian defines.
    let families = plural_families(keys.iter().copied());
    let declared = declared_families(canonical);

    let is_family_member = |key: &str| -> bool {
        plural_member(key)
            .is_some_and(|(family, _)| families.contains(family) && declared.contains(family))
    };

    let mut offenders = Vec::new();

    for (locale, keys_in_locale) in table {
        if locale == fallback {
            continue;
        }
        for key in keys_in_locale.keys() {
            if canonical.contains_key(key) || is_family_member(key) {
                continue;
            }
            offenders.push(format!(
                "  locale `{locale}` defines `{key}`, which the fallback locale \
                 `{fallback}` does not"
            ));
        }
    }

    if !offenders.is_empty() {
        return Err(Error::CanonicalKeys(offenders));
    }

    // Android refuses to build a `<plurals>` without `other`, and Apple has nothing else
    // to select, so a family a locale defines must be lookable-up *in that locale*. The
    // fallback needs it too: `values/` is where the reference lives.
    let mut problems = Vec::new();

    for family in families
        .iter()
        .filter(|family| declared.contains(family.as_str()))
    {
        for (locale, keys_in_locale) in table {
            let members = family_members(keys_in_locale, family);
            if members.is_empty() {
                continue; // This locale defines none of it, and falls back like any key.
            }
            if !members
                .iter()
                .any(|key| plural_member(key).is_some_and(|(_, category)| category == "other"))
            {
                problems.push(format!(
                    "  locale `{locale}` defines `{family}` as {} but not `{family}.other`, \
                     which both platforms need as the category they fall back to",
                    members
                        .iter()
                        .map(|key| format!("`{key}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }

    if !problems.is_empty() {
        return Err(Error::Plurals(problems));
    }

    Ok(build_entries(table, fallback))
}

/// The catalog entries `table` should emit, under every rule [`validate`] checks.
///
/// This is what the resource emitters enumerate, and — in
/// [`catalog_entries`] — what the `Key` enum is generated from, so the two cannot
/// disagree about whether `apples` is one entry or three keys.
///
/// # Errors
///
/// [`Error::MissingFallback`] when the fallback locale is absent, [`Error::CanonicalKeys`]
/// when a locale defines a family the fallback does not, [`Error::Plurals`] when a family
/// has no `other` to fall back to.
pub fn entries(
    table: &BTreeMap<String, BTreeMap<String, String>>,
    fallback: &str,
) -> Result<Vec<Entry>, Error> {
    validate(table, fallback)
}

/// The same entries as [`entries`], with nothing refused.
///
/// The `Key` enum path. It cannot adopt [`validate`]'s hard rules: a locale defining a
/// key the fallback lacks is [`Drift::Warn`] territory there (the fallback's keys are the
/// ones that exist; the extra one is a warning, and naming it stays a compile error until
/// the translation lands), while the resource emitters must refuse it because a resource
/// nobody references is worse than a missing one. What both agree on is where a family
/// begins and ends, and that is the half worth sharing.
///
/// [`Drift::Warn`]: crate::Drift::Warn
#[must_use]
pub fn catalog_entries(
    table: &BTreeMap<String, BTreeMap<String, String>>,
    fallback: &str,
) -> Vec<Entry> {
    // `build_entries` indexes the fallback's messages, and a table without one has no
    // canonical key set to group anything under.
    if !table.contains_key(fallback) {
        return Vec::new();
    }

    build_entries(table, fallback)
}

/// The entries themselves, sorted by the name each is emitted under, so a family sits in
/// the key list where its members used to be rather than appended after all of them.
///
/// The keys are the fallback's, plus every category of every family the fallback
/// declared — and nothing else. That is the canonical-set rule of the whole generator
/// (a key only Croatian has is a drift warning, not a key, and naming it stays a compile
/// error until the translation lands) with the one exemption ADR 0018 makes for
/// categories, which no fallback can spell out for the other languages it ships.
fn build_entries(table: &BTreeMap<String, BTreeMap<String, String>>, fallback: &str) -> Vec<Entry> {
    let canonical = &table[fallback];

    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for keys_in_locale in table.values() {
        keys.extend(keys_in_locale.keys().map(String::as_str));
    }

    let families = plural_families(keys.iter().copied());
    let declared = declared_families(canonical);
    let is_family = |family: &str| families.contains(family) && declared.contains(family);

    let mut entries: Vec<Entry> = canonical
        .keys()
        .filter(|key| plural_member(key).is_none_or(|(family, _)| !is_family(family)))
        .map(|key| Entry::String(key.clone()))
        .collect();

    for family in families.iter().filter(|family| is_family(family)) {
        // Union across the locales, since a category only Croatian defines still has to
        // be emitted into `values-hr/` — dropping it there renders the English-looking
        // `other` for a count Croatian has its own words for.
        let mut members: BTreeSet<&str> = BTreeSet::new();
        for keys_in_locale in table.values() {
            members.extend(family_members(keys_in_locale, family));
        }
        let mut members: Vec<String> = members.into_iter().map(str::to_owned).collect();
        members.sort_by_key(|key| {
            plural_member(key)
                .and_then(|(_, category)| PLURAL_CATEGORIES.iter().position(|c| *c == category))
                .unwrap_or(usize::MAX)
        });

        entries.push(Entry::Plural {
            family: family.clone(),
            members,
        });
    }

    entries.sort_by(|a, b| a.name().cmp(b.name()));

    entries
}

/// The placeholder order one entry's strings are indexed against.
///
/// A plain string takes the fallback's order exactly as it always has. A plural family
/// needs *one* order shared by all its categories, because a caller passes one argument
/// list to whichever category a count selects: the order is the fallback's members in
/// CLDR order, with each name first seen kept once. Two categories that order their
/// arguments differently are therefore still correct — each gets the index of the
/// argument it names — which is the same guarantee the reordered Croatian sentence gets
/// across locales.
fn placeholder_order(table: &Table, fallback: &str, entry: &Entry) -> Vec<String> {
    match entry {
        Entry::String(key) => table[fallback]
            .get(key)
            .map_or_else(Vec::new, |template| placeholders(template)),
        Entry::Plural { members, .. } => {
            let mut order = Vec::new();
            for member in members {
                if let Some(template) = table[fallback].get(member) {
                    for name in placeholders(template) {
                        if !order.contains(&name) {
                            order.push(name);
                        }
                    }
                }
            }
            order
        }
    }
}

/// The part of the table one app may reference: its own namespace and the bare keys.
///
/// The partition is not new — it is `crates/i18n/build.rs`'s `.grouping_dirs(["pages",
/// "common"])` read back out of the key. That call strips a directory from the namespace,
/// and ADR 0014's one hard error (`check::check_namespaces`, a build error, not a warning)
/// then constrains what each file may define:
///
/// - `locales/common/en.toml` reduces to **no** namespace, and holds **bare** keys —
///   `increment`, `reset`; those belong to every app, which is what "common" means;
/// - `locales/pages/counter/en.toml` reduces to the namespace `counter`, and every key in
///   it must start `counter.` — so `counter.title` belongs to `counter` alone.
///
/// Which is to say the owner is readable from the key alone, with no second map to keep in
/// step: no dot, every app; `<ns>.<rest>`, that one app. `apps_have_locale_namespaces`
/// (xtask) is the invariant that ties `<ns>` to a real app directory for the apps that
/// link `i18n`.
///
/// Match the segment *with* its dot, never the bare prefix: an app named `count` must not
/// collect `counter.title`, and `key.starts_with("count")` says it may.
///
/// A plural member is judged by its family, with one exception that is a dot doing two
/// jobs: `apples.one` contains a dot, and the segment before it is a CLDR category rather
/// than a namespace, because the family itself is bare — so it belongs to every app, like
/// `increment`. A *namespaced* family (`counter.apples.one`) needs no special case: it
/// starts with the namespace it belongs to.
///
/// # What it drops, both halves
///
/// The namespace of *another* app is the point. Two more drops are consequences of the
/// rule rather than bugs in it, and are stated here instead of being found later:
///
/// - A namespace directory that names no app — `pages/login/`, for a login page inside
///   some other app — has no owner, so its keys reach no platform resources at all. That
///   is a product disagreeing with itself (`apps_have_locale_namespaces` gives every app
///   a namespace named after it; nothing gives a *page* one), and it fails quietly, on
///   Apple only, where a catalog miss renders the key itself.
/// - A dotted key in a root-level `locales/en.toml` has namespace `None`, so
///   `check_namespaces` does not constrain it, and `for_app` attributes it to whatever
///   app its first segment names. Nothing in this repository's tree does that; a product
///   that does should move the key into a namespace directory.
///
/// The `Table` comes back with the same locales it went in with, each possibly empty: a
/// locale with nothing left in it gets no file from [`android`] and no entries in the
/// catalog from [`apple`], which is the same absence both emitters already implement.
#[must_use]
pub fn for_app(table: &Table, app: &str) -> Table {
    let prefix = format!("{app}.");
    let families = plural_families(
        table
            .values()
            .flat_map(|messages| messages.keys().map(String::as_str)),
    );

    table
        .iter()
        .map(|(locale, messages)| {
            let owned = messages
                .iter()
                .filter(|(key, _)| {
                    !key.contains('.')
                        || key.starts_with(&prefix)
                        || plural_member(key).is_some_and(|(family, _)| {
                            families.contains(family) && !family.contains('.')
                        })
                })
                .map(|(key, template)| (key.clone(), template.clone()))
                .collect();

            (locale.clone(), owned)
        })
        .collect()
}

/// Android `strings.xml` files for a whole table.
///
/// Returns `(path, contents)` pairs with paths relative to the generated resource
/// root — `values/strings_i18n.xml`, `values-hr/strings_i18n.xml` — so this stays
/// free of the monorepo layout that decides where the root is; the driver joins
/// them. A locale with no translations at all gets no file.
///
/// `committed_names` is the set of resource names already defined by hand, which
/// the caller reads from the app's committed `values/*.xml`. The check is not
/// optional: library and app resources merge into one namespace and **the app's
/// own value wins**, so a generated `app_name` colliding with the hand-written
/// one would lose silently — the one collision an otherwise correct emitter does
/// not prevent by itself.
///
/// A plural family becomes `<plurals name="apples">` with one `<item quantity="one">` per
/// category that locale defines, and a locale missing a category omits that item rather
/// than inventing one. `quantity` takes the same six CLDR names the keys use, so the
/// category is copied rather than mapped.
///
/// # Errors
///
/// [`Error::MissingFallback`] when the fallback locale is not in the table,
/// [`Error::CanonicalKeys`] when a locale defines a key — or a whole plural family — the
/// fallback lacks, [`Error::Plurals`] when a plural family has no `other` for a locale to
/// fall back to, [`Error::AndroidNames`] on any name collision or un-mappable key,
/// [`Error::AndroidLocaleCodes`] on a locale code Android cannot be given a qualifier for,
/// [`Error::Placeholders`] when a translation uses a placeholder the fallback does not.
pub fn android(
    table: &Table,
    fallback: &str,
    committed_names: &BTreeSet<String>,
) -> Result<Vec<(PathBuf, String)>, Error> {
    let entries = validate(table, fallback)?;

    // Name collisions first: a name silently overwriting another name is the
    // same shape `check::check_key_prefixes` exists to prevent, arriving from
    // the direction it does not cover, so the message names both source keys.
    //
    // Two tables, because `R.string` and `R.plurals` are separate namespaces and a
    // generated `string` named `apples` next to a generated `plurals` named `apples` is
    // legal. It is also unreachable: `apples` and `apples.one` cannot both be keys
    // (`check::check_key_prefixes` refuses a key that starts another), so only one of the
    // two can be generated at all. What is *not* checked is a hand-written `<plurals>` in
    // the app's own `values/*.xml` — `committed_names` carries `R.string` names, since
    // that is the namespace the driver reads (see `i18n_resources::read_resource_names`).
    let mut offenders = Vec::new();
    let mut by_name: BTreeMap<String, &str> = BTreeMap::new();
    let mut by_plural_name: BTreeMap<String, &str> = BTreeMap::new();

    for entry in &entries {
        let (source, generated_names, name) = match entry {
            Entry::String(key) => (key.as_str(), &mut by_name, android_name(key)),
            Entry::Plural { family, .. } => {
                (family.as_str(), &mut by_plural_name, android_name(family))
            }
        };

        let Some(name) = name else {
            offenders.push(format!(
                "  key `{source}` cannot become an Android resource name \
                 (only letters, digits and `_`, not starting with a digit; \
                 dots are mapped to `_`)"
            ));
            continue;
        };

        if let Some(first) = generated_names.insert(name.clone(), source) {
            offenders.push(format!(
                "  keys `{first}` and `{source}` would both become `{name}`"
            ));
        }

        if committed_names.contains(&name) {
            offenders.push(format!(
                "  key `{source}` would become `{name}`, which is already defined by \
                 hand in the app's committed `values/*.xml` — the app's copy wins \
                 the merge, silently"
            ));
        }
    }

    if !offenders.is_empty() {
        return Err(Error::AndroidNames(offenders));
    }

    let mut qualifiers: BTreeMap<&str, String> = BTreeMap::new();
    let mut bad_codes = Vec::new();

    for locale in table.keys() {
        match android_qualifier(locale, fallback) {
            Ok(qualifier) => {
                qualifiers.insert(locale, qualifier);
            }
            Err(()) => bad_codes.push(format!(
                "  locale `{locale}` is not a bare two- or three-letter code; \
                 script and region qualifiers need `b+` BCP-47 directory syntax \
                 and legacy codes are mapped automatically — this one is neither"
            )),
        }
    }

    if !bad_codes.is_empty() {
        return Err(Error::AndroidLocaleCodes(bad_codes));
    }

    let mut problems = Vec::new();
    let mut files = Vec::new();

    for (locale, messages) in table {
        // Every locale in the table was qualified above, or a bad code already errored.
        let qualifier = qualifiers[locale.as_str()].as_str();
        let mut body = String::new();

        for entry in &entries {
            match entry {
                Entry::String(key) => {
                    let Some(template) = messages.get(key) else {
                        continue; // This locale does not define it: absent, not English.
                    };

                    let order = placeholder_order(table, fallback, entry);
                    let formatted = format_placeholders(
                        key,
                        locale,
                        template,
                        &order,
                        &|index| format!("%{index}$s"),
                        &mut problems,
                    );

                    // `android_name` succeeded for every key above, or the collision
                    // check already errored.
                    let name = android_name(key).unwrap_or_default();
                    // Writing into a `String` cannot fail.
                    let _ = writeln!(
                        body,
                        "    <string name=\"{name}\">{}</string>",
                        escape_android(&formatted)
                    );
                }
                Entry::Plural { family, members } => {
                    let order = placeholder_order(table, fallback, entry);
                    let mut items = String::new();

                    for member in members {
                        let Some(template) = messages.get(member) else {
                            continue; // Absent in this language, and CLDR may be right.
                        };
                        // `family_members` produced these, so each has a category.
                        let Some((_, category)) = plural_member(member) else {
                            continue;
                        };

                        let formatted = format_placeholders(
                            member,
                            locale,
                            template,
                            &order,
                            &|index| format!("%{index}$s"),
                            &mut problems,
                        );
                        // Writing into a `String` cannot fail.
                        let _ = writeln!(
                            items,
                            "        <item quantity=\"{category}\">{}</item>",
                            escape_android(&formatted)
                        );
                    }

                    if items.is_empty() {
                        continue; // This locale defines none of the family.
                    }

                    let name = android_name(family).unwrap_or_default();
                    // Writing into a `String` cannot fail.
                    let _ = write!(
                        body,
                        "    <plurals name=\"{name}\">\n{items}    </plurals>\n"
                    );
                }
            }
        }

        if body.is_empty() {
            continue;
        }

        let contents = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <!-- @generated by `cargo xtask i18n-resources` from crates/i18n/locales. \
             DO NOT EDIT. -->\n\
             <resources>\n{body}</resources>\n"
        );

        files.push((
            Path::new(&format!("values{qualifier}")).join(ANDROID_FILE),
            contents,
        ));
    }

    if !problems.is_empty() {
        return Err(Error::Placeholders(problems));
    }

    Ok(files)
}

/// One entry as `(catalog key, its localizations)`, the half of [`apple`] that has to
/// answer in two shapes.
///
/// A plain string is `localizations.<lang>.stringUnit`; a plural family is
/// `localizations.<lang>.variations.plural.<category>.stringUnit`, which is one level
/// deeper and the only place the two differ. Both are what Xcode 15+ writes; Apple
/// publishes no formal schema, so this is the de facto one and the unit test asserts it.
///
/// As in [`apple`], a locale that does not define a string is absent from the catalog
/// rather than filled with the fallback's copy.
fn localizations_for(
    table: &Table,
    fallback: &str,
    entry: &Entry,
    problems: &mut Vec<String>,
) -> (String, Map<String, Value>) {
    let unit = |formatted: String| {
        let mut unit = Map::new();
        unit.insert("state".to_owned(), Value::String("translated".to_owned()));
        unit.insert("value".to_owned(), Value::String(formatted));
        Value::Object(unit)
    };
    let marker = |index: usize| format!("%{index}$@");
    let order = placeholder_order(table, fallback, entry);

    let mut localizations = Map::new();

    match entry {
        Entry::String(key) => {
            for (locale, messages) in table {
                let Some(template) = messages.get(key) else {
                    continue; // Absent, not English — see the module docs.
                };

                let formatted =
                    format_placeholders(key, locale, template, &order, &marker, problems);

                let mut localization = Map::new();
                localization.insert("stringUnit".to_owned(), unit(formatted));
                localizations.insert(locale.clone(), Value::Object(localization));
            }

            (key.clone(), localizations)
        }
        Entry::Plural { family, members } => {
            for (locale, messages) in table {
                let mut variations = Map::new();

                for member in members {
                    let Some(template) = messages.get(member) else {
                        continue; // Absent in this language, and CLDR may be right.
                    };
                    // `members` came from `family_members`, so each has a category.
                    let Some((_, category)) = plural_member(member) else {
                        continue;
                    };

                    let formatted =
                        format_placeholders(member, locale, template, &order, &marker, problems);

                    let mut localization = Map::new();
                    localization.insert("stringUnit".to_owned(), unit(formatted));
                    variations.insert(category.to_owned(), Value::Object(localization));
                }

                if variations.is_empty() {
                    continue; // This locale defines none of the family.
                }

                let mut plural = Map::new();
                plural.insert("plural".to_owned(), Value::Object(variations));

                let mut localization = Map::new();
                localization.insert("variations".to_owned(), Value::Object(plural));
                localizations.insert(locale.clone(), Value::Object(localization));
            }

            (family.clone(), localizations)
        }
    }
}

/// The whole table as one Apple String Catalog (`.xcstrings`), ready to write.
///
/// Keys pass through untouched — String Catalog keys are arbitrary strings, so
/// `Key::as_str()` is the catalog key and no name mapping is needed — and the
/// catalog carries every locale, absent ones included, as the absence of that
/// language's entry. Standard JSON escaping is the whole string-escaping rule.
///
/// The shape is Xcode's own output format: `{"sourceLanguage": .., "strings":
/// {"<key>": {"localizations": {"<lang>": {"stringUnit": {"state": "translated",
/// "value": ..}}}}}, "version": "1.0"}`. Apple publishes no formal schema for
/// `.xcstrings`; this is the de facto one, reproduced from what Xcode 15+ writes.
///
/// # Errors
///
/// As [`android`], minus the name and locale-code rules Apple does not have
/// (there are no names to collide with and no qualified directory names), plus
/// nothing in place of them.
pub fn apple(table: &Table, fallback: &str) -> Result<String, Error> {
    let entries = validate(table, fallback)?;

    let mut problems = Vec::new();
    let mut strings = Map::new();

    for entry in &entries {
        let (name, localizations) = localizations_for(table, fallback, entry, &mut problems);

        let mut catalog_entry = Map::new();
        catalog_entry.insert("localizations".to_owned(), Value::Object(localizations));
        strings.insert(name, Value::Object(catalog_entry));
    }

    if !problems.is_empty() {
        return Err(Error::Placeholders(problems));
    }

    let mut catalog = Map::new();
    catalog.insert(
        "sourceLanguage".to_owned(),
        Value::String(fallback.to_owned()),
    );
    catalog.insert("strings".to_owned(), Value::Object(strings));
    catalog.insert("version".to_owned(), Value::String("1.0".to_owned()));

    // serde_json's default map is sorted, which is what Xcode writes and what a
    // diff of two generations wants.
    serde_json::to_string_pretty(&Value::Object(catalog))
        .map(|json| format!("{json}\n"))
        .map_err(|_| {
            // Unreachable: every value here is a plain string.
            Error::Placeholders(vec!["serializing the catalog failed".to_owned()])
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic table: `(locale, key, template)` triples, in any order.
    fn table(entries: &[(&str, &str, &str)]) -> Table {
        let mut out = Table::new();

        for (locale, key, template) in entries {
            out.entry((*locale).to_owned())
                .or_default()
                .insert((*key).to_owned(), (*template).to_owned());
        }

        out
    }

    fn committed(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn paths(files: &[(PathBuf, String)]) -> Vec<String> {
        files
            .iter()
            .map(|(path, _)| path.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    fn contents<'a>(files: &'a [(PathBuf, String)], path: &str) -> &'a str {
        let (_, body) = files
            .iter()
            .find(|(p, _)| p.to_string_lossy().ends_with(path))
            .unwrap_or_else(|| panic!("no {path} in {:?}", paths(files)));
        body
    }

    fn keys(messages: &BTreeMap<String, String>) -> Vec<&str> {
        messages.keys().map(String::as_str).collect()
    }

    /// The two-namespace fixture — and the reason it is a fixture rather than the tree.
    ///
    /// This repository has one app, and it owns the only namespace, so filtering `counter`
    /// selects every key there is: the change is invisible here by construction, and the
    /// case the filter exists for cannot occur. It occurs here instead.
    #[test]
    fn an_app_gets_its_own_namespace_and_the_common_keys_and_nothing_else() {
        let all = table(&[
            ("en", "increment", "Increment"),
            ("en", "reset", "Reset"),
            ("en", "billing.title", "Billing"),
            ("en", "ledger.title", "Ledger"),
            ("hr", "increment", "Povećaj"),
            ("hr", "billing.title", "Plaćanje"),
            ("hr", "ledger.title", "Knjigovodstvo"),
        ]);

        let billing = for_app(&all, "billing");
        assert_eq!(
            keys(&billing["en"]),
            ["billing.title", "increment", "reset"]
        );
        assert_eq!(keys(&billing["hr"]), ["billing.title", "increment"]);
        // Not merely absent from `en`: the other app's keys go from every locale.
        assert!(
            billing
                .values()
                .all(|messages| !messages.contains_key("ledger.title")),
            "{billing:#?}"
        );

        // Symmetrically, `ledger` keeps its own namespace and gains the same common keys.
        let ledger = for_app(&all, "ledger");
        assert_eq!(keys(&ledger["en"]), ["increment", "ledger.title", "reset"]);
        assert!(
            ledger
                .values()
                .all(|messages| !messages.contains_key("billing.title")),
            "{ledger:#?}"
        );
    }

    /// A fresh `just new` app has an empty namespace by design (1bd7bf5 measured why), so
    /// "no namespace of its own" is the ordinary case, not a degenerate one — and it is
    /// not "no strings".
    #[test]
    fn an_app_with_no_namespace_of_its_own_still_gets_the_common_keys() {
        let all = table(&[
            ("en", "increment", "Increment"),
            ("en", "ledger.title", "Ledger"),
        ]);

        assert_eq!(keys(&for_app(&all, "probe")["en"]), ["increment"]);
    }

    /// `starts_with(app)` instead of `starts_with(app + ".")` is the same filter to a
    /// reader and a different one to `count`, which would then collect `counter.title`.
    #[test]
    fn a_namespace_prefix_match_includes_the_dot() {
        let all = table(&[
            ("en", "increment", "Increment"),
            ("en", "counter.title", "Counter"),
        ]);

        assert_eq!(keys(&for_app(&all, "count")["en"]), ["increment"]);
        assert_eq!(
            keys(&for_app(&all, "counter")["en"]),
            ["counter.title", "increment"]
        );
    }

    /// This repository's own shape, written out as the property it has: one app owning the
    /// only namespace means filtering it is the identity. That is what makes the generated
    /// tree byte-identical before and after the filter, and therefore what hashing the
    /// example app's generated `res/` and catalog says. Deliberately not a test that the
    /// *real* table filters to itself: that would fail in a product the day it grew a
    /// second app with keys of its own, which is the moment this filter is meant to start
    /// working.
    #[test]
    fn filtering_a_single_namespace_table_is_the_identity() {
        let all = table(&[
            ("en", "increment", "Increment"),
            ("en", "counter.title", "Counter"),
            ("hr", "increment", "Povećaj"),
        ]);

        assert_eq!(for_app(&all, "counter"), all);
    }

    /// The filter wired to both emitters, which is how the drivers use it — with a witness
    /// on the same fixture, because "absent" asserted against a fixture nothing was ever
    /// trying to emit proves nothing: unfiltered, this exact table *does* put `ledger`
    /// into `billing`'s resources.
    #[test]
    fn a_filtered_table_feeds_both_emitters() {
        let all = table(&[
            ("en", "increment", "Increment"),
            ("en", "billing.title", "Billing"),
            ("en", "ledger.title", "Ledger"),
            ("hr", "increment", "Povećaj"),
            ("hr", "billing.title", "Plaćanje"),
            ("hr", "ledger.title", "Knjigovodstvo"),
        ]);
        // The witness first: this is the leak, emitted the way the driver emitted it
        // before this change.
        let unfiltered = android(&all, "en", &committed(&[])).expect("generating the old way");
        let leak = contents(&unfiltered, "values/strings_i18n.xml");
        assert!(
            leak.contains("<string name=\"ledger_title\">Ledger</string>")
                && leak.contains("<string name=\"billing_title\">Billing</string>"),
            "{leak}"
        );

        let billing = for_app(&all, "billing");

        let files = android(&billing, "en", &committed(&[])).expect("generating");
        assert_eq!(
            paths(&files),
            ["values/strings_i18n.xml", "values-hr/strings_i18n.xml"]
        );

        let base = contents(&files, "values/strings_i18n.xml");
        assert!(
            base.contains("<string name=\"increment\">Increment</string>")
                && base.contains("<string name=\"billing_title\">Billing</string>"),
            "{base}"
        );
        assert!(!base.contains("ledger"), "{base}");

        let hr = contents(&files, "values-hr/strings_i18n.xml");
        assert!(
            hr.contains("<string name=\"increment\">Povećaj</string>")
                && hr.contains("<string name=\"billing_title\">Plaćanje</string>"),
            "{hr}"
        );
        assert!(!hr.contains("Knjigovodstvo"), "{hr}");

        let catalog = apple(&billing, "en").expect("generating the catalog");
        assert!(catalog.contains("\"billing.title\""), "{catalog}");
        assert!(!catalog.contains("ledger.title"), "{catalog}");
    }

    #[test]
    fn android_emits_one_file_per_locale_with_underscored_names() {
        let files = android(
            &table(&[
                ("en", "counter.title", "Counter"),
                ("en", "reset", "Reset"),
                ("hr", "counter.title", "Brojač"),
                ("hr", "reset", "Poništi"),
            ]),
            "en",
            &committed(&["app_name"]),
        )
        .expect("generating Android resources");

        assert_eq!(
            paths(&files),
            ["values/strings_i18n.xml", "values-hr/strings_i18n.xml"]
        );

        let base = contents(&files, "values/strings_i18n.xml");
        assert!(
            base.contains("<string name=\"counter_title\">Counter</string>"),
            "{base}"
        );
        assert!(
            base.contains("<string name=\"reset\">Reset</string>"),
            "{base}"
        );

        let hr = contents(&files, "values-hr/strings_i18n.xml");
        assert!(
            hr.contains("<string name=\"reset\">Poništi</string>"),
            "{hr}"
        );
    }

    /// `translate` falls back and the plan is built on `try_translate` not
    /// writing the fallback into another locale's file — the table's absences
    /// must survive into absences in the file.
    #[test]
    fn a_key_a_locale_lacks_is_absent_not_english() {
        let files = android(
            &table(&[
                ("en", "reset", "Reset"),
                ("en", "increment", "Increment"),
                ("hr", "reset", "Poništi"),
            ]),
            "en",
            &committed(&[]),
        )
        .expect("generating Android resources");

        let hr = contents(&files, "values-hr/strings_i18n.xml");
        assert!(!hr.contains("Increment"), "{hr}");
        // ... while the base file still carries every key.
        assert!(contents(&files, "values/strings_i18n.xml").contains("Increment"));
    }

    /// A locale with nothing to say gets no file at all.
    #[test]
    fn a_locale_with_no_translations_gets_no_file() {
        let mut table = table(&[("en", "reset", "Reset")]);
        table.insert("de".to_owned(), BTreeMap::new());

        let files = android(&table, "en", &committed(&[])).expect("generating Android resources");

        assert_eq!(paths(&files), ["values/strings_i18n.xml"]);
    }

    #[test]
    fn two_keys_becoming_one_resource_name_is_an_error_naming_both() {
        let error = android(
            &table(&[
                ("en", "counter.title", "Counter"),
                ("en", "counter_title", "Other"),
            ]),
            "en",
            &committed(&[]),
        )
        .expect_err("last-writer-wins is exactly the silent overwrite to prevent");

        let Error::AndroidNames(offenders) = &error else {
            panic!("expected an Android names error, got {error:?}");
        };

        // Both source keys and the name they fight over.
        assert!(
            offenders[0].contains("`counter.title`") && offenders[0].contains("`counter_title`"),
            "{offenders:#?}"
        );
        assert!(offenders[0].contains("counter_title"), "{offenders:#?}");
    }

    #[test]
    fn a_generated_name_colliding_with_a_committed_name_is_an_error() {
        let error = android(
            &table(&[("en", "app.name", "Generated app")]),
            "en",
            &committed(&["app_name"]),
        )
        .expect_err("the app's own value wins the merge, silently");

        let Error::AndroidNames(offenders) = &error else {
            panic!("expected an Android names error, got {error:?}");
        };

        assert!(offenders[0].contains("`app.name`"), "{offenders:#?}");
        assert!(offenders[0].contains("app_name"), "{offenders:#?}");
    }

    #[test]
    fn a_key_that_cannot_become_a_resource_name_is_an_error() {
        // `-` is legal in rust-i18n keys and illegal in resource names; mapping
        // it would collide with `settings_old`, so it is refused instead.
        let error = android(
            &table(&[("en", "settings-old", "Old settings")]),
            "en",
            &committed(&[]),
        )
        .expect_err("an un-mappable key must not emit a broken name");

        assert!(matches!(error, Error::AndroidNames(_)), "{error:?}");
        assert!(error.to_string().contains("settings-old"), "{error}");
    }

    /// The index of an argument is its position in the *fallback* string, reused
    /// in every language — `%1$` must mean the same argument everywhere.
    #[test]
    fn placeholders_are_indexed_by_fallback_order_and_reused_everywhere() {
        let files = android(
            &table(&[
                ("en", "greeting", "%{name} has %{count} apples"),
                ("hr", "greeting", "%{count} jabuka za %{name}"),
            ]),
            "en",
            &committed(&[]),
        )
        .expect("generating Android resources");

        let base = contents(&files, "values/strings_i18n.xml");
        assert!(base.contains("%1$s has %2$s apples"), "{base}");

        let hr = contents(&files, "values-hr/strings_i18n.xml");
        // Same indices, reordered sentence — this is what a translator does first.
        assert!(hr.contains("%2$s jabuka za %1$s"), "{hr}");
    }

    #[test]
    fn a_placeholder_the_fallback_lacks_is_an_error_on_both_platforms() {
        let table = table(&[
            ("en", "greeting", "%{name}"),
            ("hr", "greeting", "%{name} i %{boja}"),
        ]);

        let error = android(&table, "en", &committed(&[])).expect_err("no index exists for `boja`");
        let Error::Placeholders(offenders) = &error else {
            panic!("expected a placeholders error, got {error:?}");
        };
        assert!(offenders[0].contains("%{boja}"), "{offenders:#?}");

        let error = apple(&table, "en").expect_err("same rule, same failure");
        assert!(matches!(error, Error::Placeholders(_)), "{error:?}");
    }

    /// Positional arguments may be skipped — a short translation that drops one.
    #[test]
    fn omitting_a_placeholder_is_allowed() {
        let files = android(
            &table(&[
                ("en", "line", "%{name}: %{count}"),
                ("hr", "line", "%{name}"),
            ]),
            "en",
            &committed(&[]),
        )
        .expect("skipping %2$s is legal");

        assert!(contents(&files, "values-hr/strings_i18n.xml").contains("%1$s"));
    }

    #[test]
    fn a_literal_percent_is_doubled() {
        let files = android(
            &table(&[("en", "progress", "100% done")]),
            "en",
            &committed(&[]),
        )
        .expect("generating Android resources");

        assert!(
            contents(&files, "values/strings_i18n.xml").contains("100%% done"),
            "the literal % must survive both format engines"
        );
    }

    /// Every row of the escape table. The apostrophe is the one that matters:
    /// its failure is a silently truncated string, not a build error.
    #[test]
    fn android_escaping_covers_the_whole_table() {
        let files = android(
            &table(&[
                ("en", "a", "Don't say \"hi\""),
                ("en", "b", "<b>&</b>"),
                ("en", "c", "@ref @string/x"),
                ("en", "d", "?attr first"),
                ("en", "e", "line\nindented\tend"),
                ("en", "f", "back\\slash"),
            ]),
            "en",
            &committed(&[]),
        )
        .expect("generating Android resources");

        let base = contents(&files, "values/strings_i18n.xml");
        assert!(base.contains("Don\\'t say \\\"hi\\\""), "{base}");
        assert!(base.contains("&lt;b&gt;&amp;&lt;/b&gt;"), "{base}");
        assert!(base.contains("\\@ref"), "{base}");
        assert!(base.contains("\\?attr"), "{base}");
        assert!(base.contains("line\\nindented\\tend"), "{base}");
        assert!(base.contains("back\\\\slash"), "{base}");
    }

    #[test]
    fn legacy_locale_codes_are_mapped_to_what_android_expects() {
        let files = android(
            &table(&[("en", "reset", "Reset"), ("he", "reset", "Nosef")]),
            "en",
            &committed(&[]),
        )
        .expect("generating Android resources");

        assert!(
            paths(&files).contains(&"values-iw/strings_i18n.xml".to_owned()),
            "{:?}",
            paths(&files)
        );
    }

    #[test]
    fn a_locale_code_android_cannot_be_given_a_directory_for_is_an_error() {
        // Script qualifiers need `b+sr+Latn` handling the tree has no need of
        // today; emitting `values-sr-Latn/` would be silently ignored.
        let error = android(
            &table(&[("en", "reset", "Reset"), ("sr-Latn", "reset", "Resetaj")]),
            "en",
            &committed(&[]),
        )
        .expect_err("a directory Android ignores must not be emitted");

        let Error::AndroidLocaleCodes(offenders) = &error else {
            panic!("expected a locale codes error, got {error:?}");
        };
        assert!(offenders[0].contains("sr-Latn"), "{offenders:#?}");
    }

    /// The reason a family is one entry rather than three keys.
    ///
    /// English gets two items and Croatian three, in CLDR's order, under one resource
    /// name. Emit them as `apples_one`/`apples_few`/`apples_other` strings instead and
    /// every platform stops selecting them: `getQuantityString` and the String Catalog
    /// both pick by *category*, and a flat string has no category to be picked by.
    #[test]
    fn a_plural_family_becomes_one_plurals_block_per_locale() {
        let files = android(
            &table(&[
                ("en", "increment", "Increment"),
                ("en", "apples.one", "%{count} apple"),
                ("en", "apples.other", "%{count} apples"),
                ("hr", "increment", "Povećaj"),
                ("hr", "apples.one", "%{count} jabuka"),
                ("hr", "apples.few", "%{count} jabuke"),
                ("hr", "apples.other", "%{count} jabuka"),
            ]),
            "en",
            &committed(&[]),
        )
        .expect("a plural family is in scope now");

        let base = contents(&files, "values/strings_i18n.xml");
        assert!(base.contains("<plurals name=\"apples\">"), "{base}");
        assert!(
            base.contains("<item quantity=\"one\">%1$s apple</item>"),
            "{base}"
        );
        assert!(
            base.contains("<item quantity=\"other\">%1$s apples</item>"),
            "{base}"
        );
        // English has no `few`, so English does not get one.
        assert!(!base.contains("few"), "{base}");

        let hr = contents(&files, "values-hr/strings_i18n.xml");
        assert!(
            hr.contains("<item quantity=\"few\">%1$s jabuke</item>"),
            "{hr}"
        );
        // Not smuggled back in as flat strings alongside the block.
        assert!(
            !hr.contains("apples_one") && !hr.contains("apples_few"),
            "{hr}"
        );
    }

    #[test]
    fn a_plural_family_is_one_catalog_key_with_plural_variations() {
        let json = apple(
            &table(&[
                ("en", "apples.one", "%{count} apple"),
                ("en", "apples.other", "%{count} apples"),
                ("hr", "apples.one", "%{count} jabuka"),
                ("hr", "apples.few", "%{count} jabuke"),
                ("hr", "apples.other", "%{count} jabuka"),
            ]),
            "en",
        )
        .expect("generating the catalog");

        let catalog: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        assert_eq!(
            catalog["strings"]["apples"]["localizations"]["hr"]["variations"]["plural"]["few"]["stringUnit"]
                ["value"],
            "%1$@ jabuke"
        );
        assert_eq!(
            catalog["strings"]["apples"]["localizations"]["en"]["variations"]["plural"]["one"]["stringUnit"]
                ["state"],
            "translated"
        );

        // One key. A member spelled out next to it would be the flat-string bug wearing
        // JSON braces, and Xcode would offer both to the translator.
        assert!(catalog["strings"].get("apples.few").is_none());
        assert!(catalog["strings"].get("apples_one").is_none());
    }

    /// `other` is not one category among several: it is what a platform selects when
    /// nothing else does, and what Android refuses to build a `<plurals>` without.
    #[test]
    fn a_plural_family_without_other_is_refused() {
        let table = table(&[
            ("en", "apples.one", "%{count} apple"),
            ("en", "apples.other", "%{count} apples"),
            // Croatian covers one and few — as far as it goes, correctly — and stops.
            ("hr", "apples.one", "%{count} jabuka"),
            ("hr", "apples.few", "%{count} jabuke"),
        ]);

        let error = android(&table, "en", &committed(&[]))
            .expect_err("a family a locale cannot look up is not a family");
        assert!(matches!(error, Error::Plurals(_)), "{error:?}");
        assert!(error.to_string().contains("`hr`"), "{error}");
        assert!(error.to_string().contains("apples.other"), "{error}");

        let error = apple(&table, "en").expect_err("same rule, same failure");
        assert!(matches!(error, Error::Plurals(_)), "{error:?}");
    }

    /// Canonical per family, and no further: `hr` may invent a *category* of a family
    /// English has, because English cannot have every category, but it may not invent a
    /// family. Nothing in the base resource set references that one, which is what the
    /// canonical rule exists to prevent.
    #[test]
    fn a_family_the_fallback_does_not_declare_is_refused() {
        let error = android(
            &table(&[
                ("en", "increment", "Increment"),
                ("hr", "apples.one", "%{count} jabuka"),
                ("hr", "apples.other", "%{count} jabuka"),
            ]),
            "en",
            &committed(&[]),
        )
        .expect_err("a family nobody in the base set declares");

        assert!(matches!(error, Error::CanonicalKeys(_)), "{error:?}");
        assert!(error.to_string().contains("apples.other"), "{error}");
    }

    /// The over-refusal has a floor: one key named `one` is just a key.
    #[test]
    fn a_lone_category_named_key_is_not_a_plural() {
        android(
            &table(&[("en", "page.one", "First")]),
            "en",
            &committed(&[]),
        )
        .expect("a single `.one` key is not a plural family");
    }

    #[test]
    fn a_key_not_in_the_fallback_is_an_error() {
        let error = android(
            &table(&[("en", "reset", "Reset"), ("hr", "extra", "Nepoznato")]),
            "en",
            &committed(&[]),
        )
        .expect_err("the fallback is the canonical key set");

        assert!(matches!(error, Error::CanonicalKeys(_)), "{error:?}");
        assert!(error.to_string().contains("`extra`"), "{error}");
    }

    #[test]
    fn a_table_without_the_fallback_locale_is_an_error() {
        let error = apple(&table(&[("hr", "reset", "Poništi")]), "en")
            .expect_err("there is no canonical key set");
        assert!(
            matches!(error, Error::MissingFallback { ref locale, .. } if locale == "en"),
            "{error:?}"
        );
    }

    /// The shape Xcode itself writes — verified against several references
    /// before this emitter was written; Apple publishes no formal schema.
    #[test]
    fn apple_catalog_is_the_xcode_string_catalog_shape() {
        let json = apple(
            &table(&[
                ("en", "counter.title", "Counter"),
                ("en", "reset", "Reset"),
                ("hr", "counter.title", "Brojač"),
            ]),
            "en",
        )
        .expect("serializing");

        let catalog: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        assert_eq!(catalog["sourceLanguage"], "en");
        assert_eq!(catalog["version"], "1.0");

        // Dotted keys pass through untouched — no mapping, no collisions.
        assert_eq!(
            catalog["strings"]["counter.title"]["localizations"]["en"]["stringUnit"]["value"],
            "Counter"
        );
        assert_eq!(
            catalog["strings"]["counter.title"]["localizations"]["hr"]["stringUnit"]["state"],
            "translated"
        );

        // Absent, not English: `reset` has no Croatian entry at all.
        assert!(
            catalog["strings"]["reset"]["localizations"]
                .get("hr")
                .is_none()
        );
        assert!(
            catalog["strings"]["reset"]["localizations"]
                .get("en")
                .is_some()
        );
    }

    #[test]
    fn apple_placeholders_are_positional_percent_at() {
        let json = apple(
            &table(&[
                ("en", "greeting", "%{name} has %{count} apples"),
                ("hr", "greeting", "%{count} jabuka za %{name}"),
            ]),
            "en",
        )
        .expect("generating the catalog");

        let catalog: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(
            catalog["strings"]["greeting"]["localizations"]["en"]["stringUnit"]["value"],
            "%1$@ has %2$@ apples"
        );
        assert_eq!(
            catalog["strings"]["greeting"]["localizations"]["hr"]["stringUnit"]["value"],
            "%2$@ jabuka za %1$@"
        );

        let json = apple(&table(&[("en", "progress", "100% done")]), "en").expect("generating");
        assert!(json.contains("100%% done"), "{json}");
    }

    /// JSON escaping is the whole Apple escaping rule; apostrophes in particular
    /// pass through, which is exactly what they must not do on Android.
    #[test]
    fn apple_escaping_is_json_escaping() {
        let json = apple(&table(&[("en", "a", "Don't say \"hi\"\n")]), "en")
            .expect("generating the catalog");

        assert!(json.contains("Don't say \\\"hi\\\"\\n"), "{json}");
    }
}
