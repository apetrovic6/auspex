//! Reading `locales/**/*.toml` the way rust-i18n reads it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// One `locales/**/<locale>.toml`.
pub struct LocaleFile {
    /// From the file stem — `login/hr.toml` is the `hr` locale. rust-i18n reads
    /// it the same way, ignoring the directory entirely.
    pub(crate) locale: String,
    /// The directory path below `locales/`, dotted: `Some("login")` for
    /// `locales/login/hr.toml`, `None` for `locales/hr.toml`. Every key in the
    /// file has to start with it.
    pub(crate) namespace: Option<String>,
    pub(crate) path: PathBuf,
    /// Dotted key to its text. The text is carried rather than discarded
    /// because the fallback locale's copy becomes each `Key` variant's doc
    /// comment, so hovering `Key::LoginTitle` shows what it actually says.
    pub(crate) messages: BTreeMap<String, String>,
}

/// Every `*.toml` under `locales/`, at any depth, matching rust-i18n's own
/// `locales/**/*.{yml,yaml,json,toml}` glob.
pub fn read_locale_files(dir: &Path, grouping_dirs: &[String]) -> Result<Vec<LocaleFile>, Error> {
    let mut files = Vec::new();
    collect(dir, dir, grouping_dirs, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn collect(
    root: &Path,
    dir: &Path,
    grouping_dirs: &[String],
    out: &mut Vec<LocaleFile>,
) -> Result<(), Error> {
    let entries = fs::read_dir(dir).map_err(|source| Error::Io {
        action: "reading",
        path: dir.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let path = entry
            .map_err(|source| Error::Io {
                action: "reading",
                path: dir.to_path_buf(),
                source,
            })?
            .path();

        if path.is_dir() {
            collect(root, &path, grouping_dirs, out)?;
            continue;
        }

        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }

        let locale = path
            .file_stem()
            .expect("a path ending in `.toml` has a file stem")
            .to_string_lossy()
            .into_owned();

        let namespace = path
            .parent()
            .and_then(|parent| parent.strip_prefix(root).ok())
            .map(|relative| {
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .filter(|component| !grouping_dirs.iter().any(|dir| dir == component))
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .filter(|namespace| !namespace.is_empty());

        let content = fs::read_to_string(&path).map_err(|source| Error::Io {
            action: "reading",
            path: path.clone(),
            source,
        })?;
        let parsed: toml::Table = content.parse().map_err(|source| Error::Toml {
            path: path.clone(),
            source,
        })?;

        let mut messages = BTreeMap::new();
        flatten(&toml::Value::Table(parsed), String::new(), &mut messages);

        out.push(LocaleFile {
            locale,
            namespace,
            path,
            messages,
        });
    }

    Ok(())
}

/// `{ locale => every message defined for it, across all files }`.
pub fn group_by_locale(files: &[LocaleFile]) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut locales: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

    for file in files {
        locales
            .entry(file.locale.clone())
            .or_default()
            .extend(file.messages.clone());
    }

    locales
}

/// Nested tables become dotted keys, which is how rust-i18n addresses them:
/// `[messages] hello = "…"` is looked up as `messages.hello`.
fn flatten(value: &toml::Value, prefix: String, out: &mut BTreeMap<String, String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                // `_version` configures the file format; it is not a message.
                if prefix.is_empty() && key == "_version" {
                    continue;
                }

                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(value, path, out);
            }
        }
        toml::Value::String(text) => {
            out.insert(prefix, text.clone());
        }
        // A number or a bool is still a message, just an unusual one.
        other => {
            out.insert(prefix, other.to_string());
        }
    }
}
