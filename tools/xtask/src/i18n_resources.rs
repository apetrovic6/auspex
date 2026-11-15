//! `cargo xtask i18n-resources <app> <platform>` — native localization resources.
//!
//! The conversion rules live in `i18n_keygen::resources`; what lives here is the
//! part that knows the monorepo layout: where an app's platform project is, where
//! its generated output goes, and which hand-written resource names the generated
//! ones must not collide with. That split is the usual division of labour —
//! `i18n-keygen` owns the rules, xtask does the layout-aware driving.
//!
//! # Why the table is read from the compiled crate, not re-parsed from disk
//!
//! The alternative is for the driver to run its own `Generator` over `locales/`,
//! which means writing `.fallback("en")` and `.grouping_dirs(["pages", "common"])`
//! a **second** time. This repository already pays for that mistake once — the
//! fallback locale is written twice (`i18n!` and `.fallback(..)`) and needs
//! `the_i18n_macro_and_the_generator_agree_on_the_fallback` to tie them, because
//! nothing at compile time does. Reading `Locale::ALL × Key::CATALOG` from the
//! compiled crate makes that third writer impossible rather than checkable.
//!
//! Iterating with [`i18n::try_translate`] rather than [`i18n::translate`] is
//! load-bearing for the same reason it is in `i18n_keygen::resources`: a locale
//! that does not define a key must be *absent* from its resource file, because
//! `translate` falls back and would write the English string into `values-hr/`,
//! which disables the platform's own fallback while rendering identically.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use i18n_keygen::resources;

use crate::platform::Platform;
use crate::repo;

/// Generate `app`'s resources for `platform`, in place under its `generated/`.
///
/// `app` selects the output directory *and* the key set: the translations are one crate
/// for the whole repository, so an unfiltered table writes every app's words into every
/// app's binary. What one app may carry is derived from the key itself in
/// `i18n_keygen::resources::for_app`, from the same grouping-directory rule the namespace
/// check enforces — see `crates/i18n/build.rs` and ADR 0014.
pub fn generate(app: &str, platform: Platform) -> Result<()> {
    let table = resources::for_app(&translation_table(), app);
    let fallback = i18n::FALLBACK_LOCALE;

    match platform {
        Platform::Android => {
            let platform_dir = repo::root()
                .join("apps")
                .join(app)
                .join("platforms")
                .join(platform.dir());

            let files = resources::android(
                &table,
                fallback,
                &committed_resource_names(&platform_dir.join("app/src/main/res"))?,
            )
            .with_context(|| format!("generating Android resources for `{app}`"))?;

            let out_root = platform_dir.join("generated/res");
            // Stale output goes first: a key deleted from the locale tree must not
            // survive in a file nobody regenerates by hand. `generated/` is build
            // output, so there is nothing here to lose.
            if out_root.exists() {
                fs::remove_dir_all(&out_root)
                    .with_context(|| format!("clearing {}", out_root.display()))?;
            }

            for (path, contents) in &files {
                let out = out_root.join(path);
                fs::create_dir_all(out.parent().unwrap_or(out.as_path()))
                    .context("creating the output directory")?;
                fs::write(&out, contents).with_context(|| format!("writing {}", out.display()))?;
            }

            println!(
                "  ✓ i18n-resources: {} Android file(s) for `{app}`",
                files.len()
            );
        }
        Platform::Apple => {
            let json = resources::apple(&table, fallback)
                .with_context(|| format!("generating the String Catalog for `{app}`"))?;

            let out = repo::root()
                .join("apps")
                .join(app)
                .join("platforms")
                .join(platform.dir())
                .join("generated")
                .join("Localizable.xcstrings");

            fs::create_dir_all(out.parent().unwrap_or(out.as_path()))
                .context("creating the output directory")?;
            fs::write(&out, &json).with_context(|| format!("writing {}", out.display()))?;

            println!("  ✓ i18n-resources: {} for `{app}`", out.display());
        }
    }

    Ok(())
}

/// `locale → key → template`, straight from the compiled crate.
///
/// `try_translate` and nothing else — see the module docs. A key rust-i18n
/// resolves from a *fallback* table is not how this works: `try_translate` asks
/// the locale's own table, which is exactly the question the emitters ask.
///
/// Enumerated over `Key::CATALOG`, not `Key::ALL`, and the difference is the whole of
/// ADR 0018: a plural family is one variant (`Key::Apples`) but several strings
/// (`apples.one`, `apples.few`). Iterating variants looks up `"apples"`, which no locale
/// defines, so every plural family would drop out of the table here and the emitter would
/// produce an app with no plurals at all — and no error, because "no keys" is a perfectly
/// good table.
pub fn translation_table() -> resources::Table {
    let mut table = BTreeMap::new();

    for locale in i18n::Locale::ALL {
        let messages = i18n::Key::CATALOG
            .iter()
            .filter_map(|(key, _)| {
                i18n::try_translate(key, locale).map(|template| ((*key).to_owned(), template))
            })
            .collect::<BTreeMap<_, _>>();

        table.insert(locale.as_str().to_owned(), messages);
    }

    table
}

/// The resource names already defined by hand, read from the app's committed
/// `res/values*/ *.xml`.
///
/// These are what a generated name must not collide with: library and app
/// resources merge into one namespace and **the app's own value wins**, so a
/// generated `app_name` losing to the hand-written one is silent. Parsing is
/// deliberately dumb — scan for `name="` after `<string`/`<plurals`/`<string-array`
/// — because these files are small and hand-written, and a full XML parser is a
/// dependency this one scan does not justify.
pub fn committed_resource_names(res_dir: &Path) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();

    let entries = match fs::read_dir(res_dir) {
        Ok(entries) => entries,
        // No committed resources at all is a perfectly fine app.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(names),
        Err(source) => {
            return Err(
                anyhow::Error::new(source).context(format!("reading {}", res_dir.display()))
            );
        }
    };

    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", res_dir.display()))?;
        let path = entry.path();

        // Only the values directories contribute names; `drawable/` and friends
        // cannot collide with a string resource.
        let is_values = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("values"));

        if !is_values {
            continue;
        }

        let xml_files =
            fs::read_dir(&path).with_context(|| format!("reading {}", path.display()))?;
        for xml in xml_files {
            let xml = xml.with_context(|| format!("reading {}", path.display()))?;
            if xml.path().extension().is_some_and(|ext| ext == "xml") {
                read_resource_names(&xml.path(), &mut names)?;
            }
        }
    }

    Ok(names)
}

fn read_resource_names(path: &Path, names: &mut BTreeSet<String>) -> Result<()> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    for tag in extract_tags(&contents) {
        // Only `<string>` tags contribute `R.string` entries — `<plurals>` and
        // `<string-array>` get their own `R` namespaces and cannot collide with
        // a generated string.
        if tag.split_whitespace().next() == Some("string")
            && let Some(name) = attribute(&tag, "name")
        {
            names.insert(name);
        }
    }

    Ok(())
}

/// The `<tag ...>` openings of an XML document, without its contents.
fn extract_tags(xml: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut rest = xml;

    while let Some(open) = rest.find('<') {
        let tail = &rest[open + 1..];
        let Some(close) = tail.find('>') else { break };
        tags.push(tail[..close].to_owned());
        rest = &tail[close + 1..];
    }

    tags
}

/// `attr="value"` from a tag opening, if present.
fn attribute(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let at = tag.find(&needle)? + needle.len();
    let value = tag[at..].split('"').next()?;
    (!value.is_empty()).then(|| value.to_owned())
}

// The layout this driver assumes is checked by invariants, so a small amount of
// it is worth pinning here too, against the emitter's own output.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_covers_every_locale_and_key_of_the_compiled_crate() {
        let table = translation_table();

        assert_eq!(table.len(), i18n::Locale::ALL.len());
        for locale in i18n::Locale::ALL {
            // Every key the compiled crate knows, and nothing it does not — spelled
            // against `CATALOG` because the plural members are strings rather than
            // variants, which is the very thing this function is here to get right.
            assert!(
                table[locale.as_str()]
                    .keys()
                    .all(|key| i18n::Key::CATALOG.iter().any(|(known, _)| known == key)),
                "{locale}: a key that no `Key` names"
            );
        }
        // The Croatian half is real but partial: absence must survive, or the
        // driver is quietly re-implementing `translate` after all.
        //
        // It is now a partial half in a way counting entries cannot express — Croatian
        // defines a plural category English does not have, and `apples.few` is not
        // missing from English — so the assertion is per key.
        let hr = &table["hr"];
        let en = &table[i18n::FALLBACK_LOCALE];
        assert!(hr.contains_key("apples.few"), "{hr:#?}");
        assert!(!en.contains_key("apples.few"), "{en:#?}");
        assert!(
            hr.keys().filter(|key| !en.contains_key(*key)).count() == 1,
            "hr gained more than its `few`: {hr:#?}"
        );
    }

    #[test]
    fn committed_names_are_read_from_the_example_app_s_own_res_tree() {
        // Discovered, not named: a product may have dropped the example app (`cli new
        // --drop-example`) or renamed it, and the skeleton a product scaffolds an app from
        // is a legitimate tree too. The check this test stands for walks every app, not one.
        let res = repo::expand(&repo::root(), "apps/*/platforms/android/app/src/main/res")
            .into_iter()
            .find(|path| path.is_dir());
        let Some(res) = res else {
            // A product with no Android app has no committed names to read. The invariant
            // that consumes them (`generated_resources_are_wired`) skips the same way.
            return;
        };
        let names = committed_resource_names(&res).expect("reading committed resources");

        // The one name the plan says is there. If this ever changes the fixture
        // in `collision_checks_run_against_committed_names` below needs to know.
        assert!(names.contains("app_name"), "{names:?}");
    }

    /// The shape that would otherwise lose silently: a key whose generated name
    /// is already a hand-written one.
    #[test]
    fn collision_checks_run_against_committed_names() {
        let mut table = BTreeMap::new();
        table.insert(
            "en".to_owned(),
            BTreeMap::from([("app.name".to_owned(), "Generated".to_owned())]),
        );

        let error = resources::android(&table, "en", &BTreeSet::from(["app_name".to_owned()]))
            .expect_err("app_name is taken by the app's own strings.xml");
        assert!(
            matches!(error, i18n_keygen::Error::AndroidNames(_)),
            "{error:?}"
        );
    }
}
