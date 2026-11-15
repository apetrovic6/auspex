//! Layered application configuration.
//!
//! Every service loads its configuration the same way, so that "where does this value
//! come from?" has one answer across the monorepo. Sources are layered, later winning
//! over earlier:
//!
//! 1. `config/<name>.toml` — committed defaults, safe to read in a PR
//! 2. `config/<name>.<profile>.toml` — per-environment overrides, optional
//! 3. `APP__*` environment variables — deployment and secrets
//!
//! The environment layer is last because that is where a deployment and its secrets live;
//! nested keys use a double underscore, so `APP__DATABASE__URL` sets `database.url`.
//! A single underscore would be ambiguous for keys that themselves contain one.
//!
//! # Secrets from files
//!
//! A variable whose name ends in `_FILE` names a file to read instead of carrying the value:
//! `APP__DATABASE__PASSWORD_FILE=/run/secrets/password` sets `database.password` from that
//! file. This is the shape a container orchestrator, a sops decrypt and a systemd credential
//! all hand a process, and none of them wants the secret to appear in the environment it
//! hands to every child.
//!
//! One trailing line ending is trimmed, because `printf 'hunter2\\n' > password` and a
//! Kubernetes secret are both spelled with one and neither means it to be part of the value.
//! The `_FILE` variable is removed before the layering, so `APP__X_FILE` never reaches a
//! `x.file` key; setting both `APP__X` and `APP__X_FILE` is an error rather than a
//! precedence rule, because a deployment that does both believes two different values and
//! nothing downstream can tell which one it got.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use config::{Config, Environment, File, FileFormat, Map};
use serde::de::DeserializeOwned;

/// Prefix for configuration environment variables.
pub const ENV_PREFIX: &str = "APP__";

/// Separator for nested keys within an environment variable name.
pub const ENV_SEPARATOR: &str = "__";

/// Environment variable naming the active profile.
pub const PROFILE_VAR: &str = "APP_PROFILE";

/// Profile used when [`PROFILE_VAR`] is unset.
pub const DEFAULT_PROFILE: &str = "local";

/// Directory searched for configuration files when none is given.
pub const DEFAULT_DIR: &str = "config";

/// Environment variable overriding the configuration directory.
///
/// The files belong to a service (`services/api/config/`) but the process's working
/// directory is wherever it was launched from, which for `cargo run -p api` is the
/// workspace root. Rather than guess, the launcher says: the Justfile recipe and the
/// devenv process both set this.
pub const DIR_VAR: &str = "APP_CONFIG_DIR";

/// Suffix that redirects a variable's value to a file (see the module documentation).
///
/// **Reserved.** A variable ending in this is never passed to the layering under its own name:
/// `APP__DATABASE__PASSWORD_FILE` sets `database.password` and disappears, so a *configuration*
/// field whose own name ends in `_file` would be unreachable through the environment (and a
/// variable for it would be read as an indirection by mistake). `cargo xtask check` refuses a
/// `config/*.toml` key ending in `_file`, which is the file-layer half of the same rule.
pub const FILE_SUFFIX: &str = "_FILE";

/// [`ENV_PREFIX`] with the separator taken back off.
///
/// figment wanted the prefix and the separator in one literal (`Env::prefixed("APP__")`);
/// the `config` crate wants them apart (`with_prefix("APP")` plus
/// `prefix_separator("__")`). [`ENV_PREFIX`] stays the public spelling, because that is what
/// every deployment and every README says, and the builder gets this one. A test asserts
/// the two agree — two literals that must match is the shape this repository closes with a
/// check rather than a comment.
const PREFIX: &str = "APP";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A layer could not be read, or the merged result did not match the target type.
    #[error("could not load configuration {name:?} (profile {profile:?})")]
    Load {
        name: String,
        profile: String,
        /// Boxed, and it has to be. `ConfigError` is 96 bytes (measured); inline, this
        /// variant is 152, and every `Result<T, Error>` in this crate's API then crosses
        /// `clippy::result_large_err`'s 128-byte threshold — which `-D warnings` turns into
        /// a failed build. Boxed, `Error` is 56.
        #[source]
        source: Box<config::ConfigError>,
    },
    /// A variable and its `_FILE` twin are both set.
    ///
    /// Not a precedence rule: two values for one key means the deployment disagrees with
    /// itself, and whichever one won the other would look, from outside, like it had been
    /// applied.
    #[error(
        "{variable} and {file_variable} are both set — set the value itself or the file it \
         names, never both"
    )]
    BothSet {
        variable: String,
        file_variable: String,
    },
    /// A `_FILE` variable names a file that could not be read, or whose contents are not text.
    #[error("could not read {file_variable} (setting {variable}) from {path}: {source}")]
    File {
        variable: String,
        file_variable: String,
        path: PathBuf,
        /// Boxed for the same reason as [`Error::Load`]'s source: `io::Error` is small, but
        /// the box keeps this variant's size from depending on it and keeps the enum far
        /// below `clippy::result_large_err`'s threshold.
        #[source]
        source: Box<std::io::Error>,
    },
    /// The merged configuration is well-formed but says something contradictory.
    ///
    /// Raised by a caller that validates a section after loading it (`services/api` does for
    /// `database`), so the rule lives beside the fields it is about rather than in this
    /// crate, which knows nothing about a database.
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// The active profile, from [`PROFILE_VAR`], or [`DEFAULT_PROFILE`].
#[must_use]
pub fn profile() -> String {
    profile_from(std::env::var(PROFILE_VAR).ok())
}

/// The configuration directory, from [`DIR_VAR`], or [`DEFAULT_DIR`].
#[must_use]
pub fn dir() -> PathBuf {
    dir_from(std::env::var_os(DIR_VAR))
}

/// The decision behind [`profile`], separated from the lookup so it can be tested.
///
/// Reading an environment variable is not the part that can be wrong; choosing what to do
/// when it is absent, empty or set is. `unsafe_code = "forbid"` plus edition 2024 means a
/// test cannot set a variable (`std::env::set_var` became `unsafe`, and `forbid` cannot be
/// relaxed by an inner `allow`), so the choice has to be reachable without one.
///
/// Empty counts as unset. `APP_PROFILE=""` is what a launcher emits for
/// `APP_PROFILE=$PROFILE` when `PROFILE` was never set, and a profile named `""` would send
/// the loader looking for `api..toml` — a file nobody can have meant.
fn profile_from(value: Option<String>) -> String {
    value
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned())
}

/// The decision behind [`dir`], for the same reason as [`profile_from`].
fn dir_from(value: Option<OsString>) -> PathBuf {
    value
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_DIR), PathBuf::from)
}

/// Load configuration named `name` from [`DEFAULT_DIR`], using the active [`profile`].
///
/// # Errors
///
/// Returns [`Error::Load`] if a present file is malformed, or if the merged configuration
/// does not deserialize into `T` — most often a missing required field.
pub fn load<T: DeserializeOwned>(name: &str) -> Result<T, Error> {
    load_from(&dir(), name, &profile())
}

/// Load configuration named `name` from `dir` for an explicit `profile`.
///
/// Both files are optional: a service can be configured entirely from the environment,
/// which is the normal case in a container. Only a file that exists *and* is malformed is
/// an error.
///
/// # Errors
///
/// Returns [`Error::Load`] if a present file is malformed, or if the merged configuration
/// does not deserialize into `T`.
pub fn load_from<T: DeserializeOwned>(dir: &Path, name: &str, profile: &str) -> Result<T, Error> {
    merge(dir, name, profile, None)
}

/// [`load_from`], with the environment supplied rather than read from the process.
///
/// The hook a test needs, and the one an embedder with its own environment source would want:
/// the layering and the `_FILE` resolution are exactly what a caller must not reimplement, and
/// a test cannot set a process variable at all (see the module documentation — `set_var` is
/// `unsafe` in edition 2024 and this workspace forbids unsafe code).
///
/// Keys are full variable names, `APP__` included, which is what `Environment::source` expects.
/// They go through the same `_FILE` resolution as the process environment.
///
/// # Errors
///
/// Same as [`load_from`], plus [`Error::BothSet`] and [`Error::File`] for the `_FILE` half.
pub fn load_from_env<T: DeserializeOwned>(
    dir: &Path,
    name: &str,
    profile: &str,
    env: impl IntoIterator<Item = (String, String)>,
) -> Result<T, Error> {
    let mut map = Map::new();
    for (key, value) in env {
        map.insert(key, value);
    }
    merge(dir, name, profile, Some(map))
}

/// The layering, with the environment injectable.
///
/// `env` is `None` in production, which makes [`Environment`] read the process environment
/// (`None => env::vars_os()` in the upstream source). Tests pass `Some(map)` instead — see
/// the test module for why that is not a convenience but the only way this can be tested at
/// all.
fn merge<T: DeserializeOwned>(
    dir: &Path,
    name: &str,
    profile: &str,
    env: Option<Map<String, String>>,
) -> Result<T, Error> {
    let base: PathBuf = dir.join(format!("{name}.toml"));
    let overlay: PathBuf = dir.join(format!("{name}.{profile}.toml"));

    // The environment becomes data here, `_FILE` indirection resolved, and only then becomes
    // a source. Both halves have to happen before the builder sees it: the `config` crate has
    // no notion of a file-backed variable, and a `_FILE` that reached the layering would
    // otherwise be a key of its own.
    let mut env = match env {
        Some(env) => env,
        None => process_env()?,
    };
    resolve_file_variables(&mut env)?;

    Config::builder()
        // `required(false)`: both files are optional, exactly as before. A service
        // configured entirely from the environment is the normal case in a container, and
        // only a file that exists *and* is malformed is an error.
        .add_source(File::from(base).format(FileFormat::Toml).required(false))
        .add_source(File::from(overlay).format(FileFormat::Toml).required(false))
        .add_source(
            Environment::with_prefix(PREFIX)
                .prefix_separator(ENV_SEPARATOR)
                .separator(ENV_SEPARATOR)
                // Deliberately *not* `.try_parsing(true)`. It looks like the safe default
                // and is the opposite: `Value::into_*` already parses the string in
                // `APP__PORT=9999` into a `u16`, so eager parsing buys nothing here, and
                // what it adds is a round trip through `i64` / `f64` / `bool` for every
                // variable — which rewrites any value whose spelling is not the number's
                // (`APP__DATABASE__PASSWORD=007` comes back as `7`). A test pins it.
                //
                // Also deliberately *not* `.ignore_empty(true)`, so an empty value overrides
                // rather than falling through to the file below it. A test pins that.
                .source(Some(env)),
        )
        .build()
        .and_then(Config::try_deserialize)
        .map_err(|source| Error::Load {
            name: name.to_owned(),
            profile: profile.to_owned(),
            source: Box::new(source),
        })
}

/// This process's `APP__*` variables, as the environment layer.
///
/// Read with `vars_os` and converted explicitly: `vars()` panics on a variable that is not
/// UTF-8, and a panic in configuration loading names neither the variable nor the fact that
/// the environment is at fault. A non-UTF-8 *value* is an error because silently dropping it
/// would turn a malformed secret into "missing secret", which is a different bug in a
/// different place. A non-UTF-8 *key* cannot be one of ours (every `APP__*` name is ASCII) and
/// is skipped.
fn process_env() -> Result<Map<String, String>, Error> {
    let mut env = Map::new();
    for (key, value) in std::env::vars_os() {
        let Some(key) = key.to_str() else {
            continue;
        };
        if !key.starts_with(ENV_PREFIX) {
            continue;
        }
        let value = value.into_string().map_err(|_| {
            Error::Invalid(format!(
                "{key} is not valid UTF-8, so it cannot be a setting"
            ))
        })?;
        env.insert(key.to_owned(), value);
    }
    Ok(env)
}

/// Replace every `KEY_FILE` with the contents of the file it names.
///
/// In place, before the layering, so the file's value is exactly as strong as an environment
/// variable and beats every file layer, which is what a deployment expects of it.
fn resolve_file_variables(env: &mut Map<String, String>) -> Result<(), Error> {
    let file_variables: Vec<String> = env
        .keys()
        .filter(|key| key.ends_with(FILE_SUFFIX))
        .cloned()
        .collect();

    for file_variable in file_variables {
        // The key list was cloned above; a `None` here would mean the map changed underneath
        // it, which nothing in this function does.
        let Some(contents) = env.remove(&file_variable) else {
            continue;
        };
        let variable = file_variable
            .strip_suffix(FILE_SUFFIX)
            .unwrap_or_default()
            .to_owned();

        if env.contains_key(&variable) {
            return Err(Error::BothSet {
                variable,
                file_variable,
            });
        }

        let path = PathBuf::from(&contents);
        let text = std::fs::read_to_string(&path).map_err(|source| Error::File {
            variable: variable.clone(),
            file_variable: file_variable.clone(),
            path: path.clone(),
            source: Box::new(source),
        })?;
        env.insert(variable, trim_line_ending(&text));
    }
    Ok(())
}

/// The value of a file-backed secret, without the line ending a shell or a secret store adds.
///
/// One line ending, not `trim_end()`: a secret may legitimately end in a space, and a value
/// that lost it would fail to authenticate for a reason nothing reports.
fn trim_line_ending(text: &str) -> String {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Database {
        url: String,
        #[serde(default)]
        pool_size: u32,
        #[serde(default)]
        password: String,
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Settings {
        port: u16,
        database: Database,
    }

    const BASE: &str = "port = 5000\n[database]\nurl = \"postgres://base\"\n";

    /// A directory the loader can read, deleted when the test ends.
    ///
    /// Returned alongside the `TempDir` that owns it: dropping that ends the directory, so
    /// every test has to hold on to both.
    fn dir_with(files: &[(&str, &str)]) -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("create a temp dir");
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).expect("write a config file");
        }
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    /// The environment layer as data.
    ///
    /// `std::env::set_var` is `unsafe` in edition 2024 and the workspace forbids unsafe
    /// code, which is what figment's `Jail` was doing for us from inside its own crate.
    /// `Environment::source(Some(map))` is the upstream-blessed alternative — it exists, per
    /// its own doc comment, for "when you want to test your own code using this source,
    /// without the need to change the actual system environment variables". These tests
    /// therefore mutate nothing, race nothing, and do not depend on nextest's
    /// process-per-test for correctness.
    fn env(pairs: &[(&str, &str)]) -> Map<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// The layering, called the only way a test can call it.
    ///
    /// [`merge`]'s `None` means "read the process environment", which is precisely what a
    /// test must not do — it is the state the rest of the suite is written to avoid.
    fn load_with<T: DeserializeOwned>(
        dir: &Path,
        name: &str,
        profile: &str,
        env: Map<String, String>,
    ) -> Result<T, Error> {
        merge(dir, name, profile, Some(env))
    }

    /// The `Error::Load` source, as text — where the interesting half of a failure lives.
    fn cause<T>(result: &Result<T, Error>) -> String {
        match result {
            Err(Error::Load { source, .. }) => source.to_string(),
            _ => panic!("expected Error::Load"),
        }
    }

    #[test]
    fn reads_the_base_file() {
        let (_dir, path) = dir_with(&[("api.toml", BASE)]);
        let settings: Settings = load_with(&path, "api", "local", Map::new()).unwrap();
        assert_eq!(settings.port, 5000);
        assert_eq!(settings.database.url, "postgres://base");
        assert_eq!(settings.database.pool_size, 0, "serde's default applies");
    }

    #[test]
    fn profile_file_overrides_the_base_file() {
        let (_dir, path) = dir_with(&[("api.toml", BASE), ("api.prod.toml", "port = 8080\n")]);
        let settings: Settings = load_with(&path, "api", "prod", Map::new()).unwrap();
        assert_eq!(
            settings.port, 8080,
            "profile layer should win over the base"
        );
        assert_eq!(
            settings.database.url, "postgres://base",
            "keys the profile does not mention should survive"
        );
    }

    #[test]
    fn environment_wins_over_every_file_and_nests_on_double_underscore() {
        let (_dir, path) = dir_with(&[("api.toml", BASE), ("api.prod.toml", "port = 8080\n")]);
        let settings: Settings = load_with(
            &path,
            "api",
            "prod",
            env(&[
                ("APP__PORT", "9999"),
                ("APP__DATABASE__URL", "postgres://from-env"),
            ]),
        )
        .unwrap();
        assert_eq!(settings.port, 9999, "env should win over both files");
        assert_eq!(settings.database.url, "postgres://from-env");
    }

    #[test]
    fn missing_files_are_fine_when_the_environment_is_complete() {
        let (_dir, path) = dir_with(&[]);
        let settings: Settings = load_with(
            &path,
            "api",
            "local",
            env(&[
                ("APP__PORT", "1234"),
                ("APP__DATABASE__URL", "postgres://only-env"),
            ]),
        )
        .unwrap();
        assert_eq!(settings.port, 1234);
        assert_eq!(settings.database.url, "postgres://only-env");
    }

    #[test]
    fn a_missing_required_field_is_an_error_not_a_default() {
        let (_dir, path) = dir_with(&[("api.toml", "port = 5000\n")]);
        let result: Result<Settings, Error> = load_with(&path, "api", "local", Map::new());
        assert!(result.is_err(), "database.url is required and absent");
        assert!(
            cause(&result).contains("database"),
            "the error should name the missing field, got: {}",
            cause(&result)
        );
    }

    #[test]
    fn a_single_underscore_inside_a_key_does_not_nest() {
        let (_dir, path) = dir_with(&[("api.toml", BASE)]);
        let settings: Settings = load_with(
            &path,
            "api",
            "local",
            env(&[("APP__DATABASE__POOL_SIZE", "7")]),
        )
        .unwrap();
        assert_eq!(
            settings.database.pool_size, 7,
            "`__` nests; a lone `_` is part of the key, which is why `__` was chosen"
        );
    }

    #[test]
    fn a_present_but_malformed_file_is_an_error_naming_the_file() {
        let (_dir, path) = dir_with(&[("api.toml", "port = \n")]);
        let result: Result<Settings, Error> = load_with(&path, "api", "local", Map::new());
        assert!(
            result.is_err(),
            "a file that exists and does not parse is an error"
        );
        let cause = cause(&result);
        assert!(
            cause.contains("api.toml"),
            "the error should name the offending file, got: {cause}"
        );
    }

    #[test]
    fn an_empty_environment_value_overrides_rather_than_falling_through() {
        // Pinned, because neither engine documents it and `ignore_empty(true)` is one
        // method call away from the opposite behaviour. An empty `APP__PORT=` shadows the
        // file's `port = 5000` with an empty string, which does not deserialize into u16:
        // empty means "set to nothing", not "unset".
        let (_dir, path) = dir_with(&[("api.toml", BASE)]);
        let result: Result<Settings, Error> =
            load_with(&path, "api", "local", env(&[("APP__PORT", "")]));
        assert!(result.is_err(), "an empty value is not a number");
        assert!(
            cause(&result).contains("port"),
            "the error should name the field the empty value landed on, got: {}",
            cause(&result)
        );

        // The same empty value on a `String` field is accepted, which is the half that
        // makes this a decision rather than a parse failure.
        let (_dir, path) = dir_with(&[("api.toml", BASE)]);
        let settings: Settings =
            load_with(&path, "api", "local", env(&[("APP__DATABASE__URL", "")])).unwrap();
        assert_eq!(settings.database.url, "", "empty overrides the file layer");
    }

    #[test]
    fn a_file_variable_sets_the_value_from_the_file() {
        let (dir, path) = dir_with(&[("api.toml", BASE)]);
        let secret = dir.path().join("password");
        // A trailing space before the newline, on purpose: only the line ending is trimmed,
        // and a secret that lost a meaningful trailing space would fail to authenticate for a
        // reason nothing would report.
        std::fs::write(&secret, "hunter2 \n").expect("write a secret file");

        let settings: Settings = load_with(
            &path,
            "api",
            "local",
            env(&[(
                "APP__DATABASE__PASSWORD_FILE",
                secret.to_str().expect("a UTF-8 path"),
            )]),
        )
        .unwrap();

        assert_eq!(
            settings.database.password, "hunter2 ",
            "the file's value replaces the variable, and only one line ending is trimmed"
        );
    }

    #[test]
    fn a_file_variable_is_read_before_the_layering_so_it_beats_the_files() {
        // The value is as strong as any other environment variable: it is resolved into the
        // environment layer, not layered underneath it.
        let (dir, path) = dir_with(&[("api.toml", BASE)]);
        let secret = dir.path().join("url");
        std::fs::write(&secret, "postgres://from-file\n").expect("write a secret file");

        let settings: Settings = load_with(
            &path,
            "api",
            "local",
            env(&[(
                "APP__DATABASE__URL_FILE",
                secret.to_str().expect("a UTF-8 path"),
            )]),
        )
        .unwrap();

        assert_eq!(settings.database.url, "postgres://from-file");
    }

    #[test]
    fn a_file_variable_and_its_twin_are_an_error_rather_than_a_precedence_rule() {
        let (dir, path) = dir_with(&[("api.toml", BASE)]);
        let secret = dir.path().join("password");
        std::fs::write(&secret, "from-file\n").expect("write a secret file");

        let result: Result<Settings, Error> = load_with(
            &path,
            "api",
            "local",
            env(&[
                ("APP__DATABASE__PASSWORD", "from-env"),
                (
                    "APP__DATABASE__PASSWORD_FILE",
                    secret.to_str().expect("a UTF-8 path"),
                ),
            ]),
        );

        let message = result
            .as_ref()
            .err()
            .map(ToString::to_string)
            .expect("two values for one key must be an error");
        assert!(
            message.contains("APP__DATABASE__PASSWORD") && message.contains("_FILE"),
            "the error must name both variables, got: {message}"
        );
    }

    #[test]
    fn an_unreadable_secret_file_names_the_path_and_the_variable() {
        let (_dir, path) = dir_with(&[("api.toml", BASE)]);
        let missing = path.join("there-is-no-such-secret");

        let result: Result<Settings, Error> = load_with(
            &path,
            "api",
            "local",
            env(&[(
                "APP__DATABASE__PASSWORD_FILE",
                missing.to_str().expect("a UTF-8 path"),
            )]),
        );

        let message = result
            .as_ref()
            .err()
            .map(ToString::to_string)
            .expect("an unreadable file must be an error");
        assert!(
            message.contains("there-is-no-such-secret")
                && message.contains("APP__DATABASE__PASSWORD_FILE"),
            "the error must name the path and the variable, got: {message}"
        );
    }

    #[test]
    fn a_file_variable_does_not_become_a_key_of_its_own() {
        // `APP__DATABASE__PASSWORD_FILE` must set `database.password` and nothing else. If it
        // survived the resolution, the environment layer would also carry
        // `database.password_file`, which a strict settings type would reject.
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Strict {
            port: u16,
            database: StrictDatabase,
        }

        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct StrictDatabase {
            url: String,
            password: String,
        }

        let (dir, path) = dir_with(&[(
            "api.toml",
            "port = 5000\n[database]\nurl = \"postgres://base\"\npassword = \"\"\n",
        )]);
        let secret = dir.path().join("password");
        std::fs::write(&secret, "hunter2\n").expect("write a secret file");

        let settings: Strict = load_with(
            &path,
            "api",
            "local",
            env(&[(
                "APP__DATABASE__PASSWORD_FILE",
                secret.to_str().expect("a UTF-8 path"),
            )]),
        )
        .expect("the _FILE variable must not leak through as a key");

        assert_eq!(settings.database.password, "hunter2");
        assert_eq!(settings.port, 5000);
        assert_eq!(settings.database.url, "postgres://base");
    }

    #[test]
    fn a_numeric_looking_secret_keeps_its_spelling() {
        // `.try_parsing(true)` is one method call away and looks like the safe default. It
        // is the opposite, and it buys nothing: `Value::into_uint` / `into_int` /
        // `into_float` already parse a string, which is how `APP__PORT=9999` reaches a
        // `u16` without it. What eager parsing adds is a round trip through `i64` / `f64`
        // / `bool` for *every* variable, so a value whose spelling is not the number's
        // comes back rewritten — `007` as `7`, `1.50` as `1.5`, `True` as `true`. A
        // password is the case that matters, because nothing downstream can tell it was
        // ever different.
        let (_dir, path) = dir_with(&[("api.toml", BASE)]);
        let settings: Settings = load_with(
            &path,
            "api",
            "local",
            env(&[("APP__DATABASE__PASSWORD", "007")]),
        )
        .unwrap();
        assert_eq!(settings.database.password, "007");
    }

    #[test]
    fn profile_falls_back_to_the_default_when_unset_or_empty() {
        assert_eq!(profile_from(Some("staging".to_owned())), "staging");
        assert_eq!(profile_from(None), DEFAULT_PROFILE);
        assert_eq!(
            profile_from(Some(String::new())),
            DEFAULT_PROFILE,
            "an empty APP_PROFILE is a launcher's unset variable, not a profile name"
        );
    }

    #[test]
    fn dir_falls_back_to_the_default_when_unset_or_empty() {
        assert_eq!(
            dir_from(Some(OsString::from("/etc/api"))),
            PathBuf::from("/etc/api")
        );
        assert_eq!(dir_from(None), PathBuf::from(DEFAULT_DIR));
        assert_eq!(
            dir_from(Some(OsString::new())),
            PathBuf::from(DEFAULT_DIR),
            "an empty APP_CONFIG_DIR would otherwise search the working directory"
        );
    }

    #[test]
    fn the_public_prefix_is_the_private_one_plus_the_separator() {
        assert_eq!(
            ENV_PREFIX,
            format!("{PREFIX}{ENV_SEPARATOR}"),
            "ENV_PREFIX is what the documentation and every deployment spell; PREFIX is what \
             the builder needs. They cannot disagree."
        );
    }
}
