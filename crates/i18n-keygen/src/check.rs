//! The rules a locale tree has to obey before anything is generated from it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use icu_plurals::{PluralCategory, PluralRules};

use crate::error::Error;
use crate::locales::LocaleFile;
use crate::resources;

/// Where the other locales disagree with the canonical one, one line each.
///
/// The fallback locale is canonical: its keys are the ones that exist. Both
/// directions are reported — a key it is missing is a translation that will
/// never be reached, which is as much a mistake as one that falls back.
///
/// A plural family is not a set of keys and is not compared that way (ADR 0018):
/// `apples.few` in Croatian is not drift when English has no such category, and saying so
/// on every build would train everyone to ignore the warning. The categories a family
/// must have are not compared to the fallback at all but to CLDR, in
/// [`plurals_match_cldr`], where they are a hard error rather than a line here.
pub fn drift(
    fallback: &str,
    canonical: &BTreeMap<String, String>,
    locales: &BTreeMap<String, BTreeMap<String, String>>,
) -> Vec<String> {
    let mut lines = Vec::new();

    let families = resources::plural_families(
        locales
            .values()
            .flat_map(|messages| messages.keys().map(String::as_str)),
    );
    let canonical_families = resources::declared_families(canonical);
    // A family only becomes a family if the fallback declares it: otherwise its keys are
    // ordinary keys and the ordinary rule below is the right one to apply.
    let is_family_member = |key: &str| -> bool {
        resources::plural_member(key).is_some_and(|(family, _)| {
            families.contains(family) && canonical_families.contains(family)
        })
    };

    for (locale, messages) in locales {
        if locale == fallback {
            continue;
        }

        // Both directions exempt plural categories, and for the same reason: a category is
        // not a translation a locale is owed or allowed to invent, it is a slot that
        // locale's own rules do or do not have. `plurals_match_cldr` decides that, against
        // CLDR, and warns with a hard error rather than a drift line.
        for missing in canonical
            .keys()
            .filter(|key| !messages.contains_key(*key) && !is_family_member(key))
        {
            lines.push(format!("locale `{locale}` is missing the key `{missing}`"));
        }
        for extra in messages
            .keys()
            .filter(|key| !canonical.contains_key(*key) && !is_family_member(key))
        {
            lines.push(format!(
                "locale `{locale}` has `{extra}`, which `{fallback}` does not"
            ));
        }
    }

    lines
}

/// A plural family must use exactly the categories CLDR gives each locale that defines it.
///
/// Both directions, and both are wrong at runtime only. The six CLDR categories exist;
/// a language selects some of them. `apples.two` in Croatian is accepted by every tool in
/// the chain — `aapt` takes `<item quantity="two">`, Xcode takes the variation, this
/// generator would have emitted it happily — and selected by none, because Croatian's
/// cardinal rules never return `two`: a string no count can reach, which is not visible
/// as dead code from the file it is written in. The other direction is a family `hr`
/// defines without `few`, where Croatian selects `few` for two: `plural_key` names
/// `apples.few`, nothing defines it, and `translate` renders the key. The per-key rules
/// cannot see either of them, because in both cases one of the two files has no such key
/// for the comparison to line up against.
///
/// A family the fallback does not declare is skipped: those keys stay flat keys, and
/// `check_key_prefixes` already owns whatever they are.
///
/// A locale ICU cannot parse is skipped rather than failed: `locales/` names its
/// directories, and nothing in this repository checks those names are BCP 47, so a code
/// with no CLDR answer has no answer to be wrong against.
pub fn plurals_match_cldr(
    fallback: &str,
    locales: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<(), Error> {
    let mut offenders = Vec::new();

    let families = resources::plural_families(
        locales
            .values()
            .flat_map(|messages| messages.keys().map(String::as_str)),
    );
    // A table with no fallback locale is refused by the generator before this runs; the
    // empty map is here so this check has no failure mode of its own.
    let no_keys = BTreeMap::new();
    let canonical_families =
        resources::declared_families(locales.get(fallback).unwrap_or(&no_keys));

    for (locale, messages) in locales {
        let Some(categories) = cldr_categories(locale) else {
            continue;
        };

        for family in resources::declared_families(messages) {
            if !(families.contains(family) && canonical_families.contains(family)) {
                continue;
            }
            let defined = resources::family_categories(messages, family);

            for extra in defined
                .iter()
                .filter(|category| !categories.contains(category))
            {
                offenders.push(format!(
                    "  locale `{locale}` defines `{family}.{extra}`, which its plural rules \
                     never select — `{locale}` has {categories}",
                    categories = list(&categories)
                ));
            }

            for missing in categories
                .iter()
                .filter(|category| !defined.contains(*category))
            {
                offenders.push(format!(
                    "  locale `{locale}` defines `{family}` without `{family}.{missing}`, \
                     which its plural rules do select: a count in that category renders the \
                     `other` form — `{locale}` has {categories}",
                    categories = list(&categories)
                ));
            }
        }
    }

    if offenders.is_empty() {
        Ok(())
    } else {
        Err(Error::PluralCategories(offenders))
    }
}

/// The categories CLDR gives `locale` for cardinal numbers, spelled as the locale files
/// spell them.
/// The cardinal categories CLDR gives `locale`, in CLDR's own order, so a message about
/// them reads as the rulebook reads rather than alphabetically.
fn cldr_categories(locale: &str) -> Option<Vec<&'static str>> {
    let rules =
        PluralRules::try_new_cardinal(locale.parse::<icu_locale_core::Locale>().ok()?.into())
            .ok()?;

    Some(rules.categories().map(cldr_name).collect())
}

/// The name a locale file and a platform resource use for a CLDR category.
const fn cldr_name(category: PluralCategory) -> &'static str {
    match category {
        PluralCategory::Zero => "zero",
        PluralCategory::One => "one",
        PluralCategory::Two => "two",
        PluralCategory::Few => "few",
        PluralCategory::Many => "many",
        PluralCategory::Other => "other",
    }
}

/// A set as `` `a`, `b` ``, in the order the set holds it.
fn list(names: &[&str]) -> String {
    names.to_vec().join(", ")
}

/// A file under `locales/<name>/` may only define keys prefixed `<name>.`, and
/// no two directories may claim the same `<name>`.
pub fn check_namespaces(files: &[LocaleFile]) -> Result<(), Error> {
    check_namespaces_unique(files)?;

    let mut offenders = Vec::new();

    for file in files {
        let Some(namespace) = &file.namespace else {
            continue;
        };

        let prefix = format!("{namespace}.");
        for key in file.messages.keys().filter(|key| !key.starts_with(&prefix)) {
            offenders.push(format!(
                "  {} defines `{key}`, which is not under `{prefix}`",
                file.path.display()
            ));
        }
    }

    if offenders.is_empty() {
        Ok(())
    } else {
        Err(Error::Namespace(offenders))
    }
}

/// No key may also be the start of another: `settings` and `settings.title`
/// cannot both exist.
///
/// The namespace rules above constrain what a file inside `locales/<name>/`
/// may define, but say nothing about the common file at the root. A bare
/// `settings` there collides with the whole `settings.*` namespace a page
/// directory owns — one is a string, the other a table, and rust-i18n keeps a
/// single flat map per locale, so merging drops one of them by glob order with
/// no diagnostic. That is precisely the silent overwrite the namespace rules
/// exist to prevent, reached from the one direction they do not cover.
///
/// Checked per locale over every file at once, because the collision only
/// exists once a locale's files are merged — each file alone is valid TOML.
pub fn check_key_prefixes(files: &[LocaleFile]) -> Result<(), Error> {
    let mut by_locale: BTreeMap<&str, BTreeMap<&str, &Path>> = BTreeMap::new();

    for file in files {
        let keys = by_locale.entry(file.locale.as_str()).or_default();

        for key in file.messages.keys() {
            keys.insert(key.as_str(), file.path.as_path());
        }
    }

    let mut offenders = Vec::new();

    for (locale, keys) in &by_locale {
        for (key, path) in keys {
            // `/` is `.` + 1, so this half-open range is exactly the keys that
            // continue past a dot — `settings.title` but not `settings-title`,
            // which merely sorts nearby.
            let (first, past_last) = (format!("{key}."), format!("{key}/"));

            for (descendant, descendant_path) in keys.range(first.as_str()..past_last.as_str()) {
                offenders.push(format!(
                    "  `{locale}`: `{key}` in {} is also the start of `{descendant}` in {}",
                    path.display(),
                    descendant_path.display()
                ));
            }
        }
    }

    if offenders.is_empty() {
        Ok(())
    } else {
        Err(Error::KeyPrefix(offenders))
    }
}

/// Each namespace must come from exactly one directory.
///
/// Stripping the grouping directories is what makes this reachable:
/// `locales/login/` and `locales/pages/login/` both reduce to `login`, so
/// without this they would share a key space and overwrite each other in glob
/// order — the very thing the namespace rule prevents everywhere else.
fn check_namespaces_unique(files: &[LocaleFile]) -> Result<(), Error> {
    let mut directories: BTreeMap<&str, BTreeSet<PathBuf>> = BTreeMap::new();

    for file in files {
        let Some(namespace) = &file.namespace else {
            continue;
        };

        if let Some(parent) = file.path.parent() {
            directories
                .entry(namespace.as_str())
                .or_default()
                .insert(parent.to_path_buf());
        }
    }

    let clashes = directories
        .iter()
        .filter(|(_, dirs)| dirs.len() > 1)
        .map(|(namespace, dirs)| {
            let dirs = dirs
                .iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");

            format!("  `{namespace}` is claimed by {dirs}")
        })
        .collect::<Vec<_>>();

    if clashes.is_empty() {
        Ok(())
    } else {
        Err(Error::NamespaceClash(clashes))
    }
}
