//! Settings for this service.
//!
//! Loading is [`config`]'s job — committed TOML, then a per-profile overlay, then `APP__`
//! environment variables. This module only describes the shape.

use std::num::NonZeroU32;

use serde::Deserialize;

/// Name of this service's configuration file (`<name>.toml`).
pub const CONFIG_NAME: &str = "api";

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub application: ApplicationSettings,
    pub database: DatabaseSettings,
    #[serde(default)]
    pub telemetry: telemetry::Config,
    /// Whether the schema is offered to anyone who can reach the port.
    ///
    /// Absent means off: a config predating this key must not be exposed by it.
    #[serde(default)]
    pub reflection: ReflectionSettings,
    /// The built single-page application this origin serves, if any.
    #[serde(default)]
    pub web: WebSettings,
}

/// A built single-page application, served from disk beside the API.
///
/// A service that has a browser front end serves it from the same origin as its RPCs, which
/// is why there is no separate web server: the client is same-origin with the API it calls,
/// and a deployment is one process and one port.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WebSettings {
    /// Directory holding the built bundle (`index.html` and its assets).
    ///
    /// Empty means this service serves no static files, which is the default: a service with
    /// no front end must not answer its root with a 404 that looks like a missing route.
    /// The image sets this from the directory it bakes in, and a deployment may point it
    /// anywhere on disk (`APP__WEB__DIR`).
    #[serde(default)]
    pub dir: String,
    /// The file served for a path that is not a file.
    ///
    /// A client-side router owns every path under its base, and the browser got there from a
    /// link in the bundle, not from a request the server ever saw. Answering those with
    /// `index.html` is what lets a reload at `/counter` work; answering with 404 makes the
    /// front end fail on exactly the paths a user shares.
    #[serde(default = "default_index")]
    pub index: String,
}

/// `index.html` — what every SPA build names its entry point.
fn default_index() -> String {
    "index.html".to_owned()
}

impl WebSettings {
    /// Whether a directory was configured at all.
    #[must_use]
    pub const fn is_served(&self) -> bool {
        !self.dir.is_empty()
    }
}

/// Whether to serve gRPC server reflection (`docs/adr/0019`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReflectionSettings {
    /// Off unless a profile says otherwise; `api.local.toml` is the layer that does.
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseSettings {
    /// A complete connection URL, and the alternative to the structured fields below.
    ///
    /// This is the shape a managed database hands its consumer: `CloudNativePG`'s
    /// `<cluster>-app` secret carries the whole URL under `uri`, and a deployment mounts it
    /// or names it from `APP__DATABASE__URL_FILE` rather than reassembling it from parts that
    /// the operator may have changed.
    ///
    /// Set this OR the structured fields, never both — [`DatabaseSettings::validate`] refuses
    /// a configuration that does both, because the two shapes disagree about which host,
    /// role and password is in use and silently picking one would look like success.
    #[serde(default)]
    pub url: String,
    /// Structured connection fields, for a deployment where each part is set separately.
    ///
    /// Defaulted rather than required, so a URL-only configuration deserializes; absence is
    /// caught by [`DatabaseSettings::validate`], which names every field it needed and did
    /// not get. Empty means unset, which is what `APP__DATABASE__HOST=""` would otherwise
    /// try to dial.
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    /// Empty in the committed file on purpose — a password belongs in the environment
    /// (`APP__DATABASE__PASSWORD`, or `APP__DATABASE__PASSWORD_FILE` for a mounted secret),
    /// never in a file a pull request can read.
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub database_name: String,
    /// Postgres counts every pooled connection against its own `max_connections`, so this
    /// is a share of a global budget rather than a local tuning knob.
    pub max_connections: NonZeroU32,
}

impl DatabaseSettings {
    /// Refuse a configuration that names two databases.
    ///
    /// The rule is one shape or the other: a `url` excludes `host`, `port`, `username`,
    /// `password` and `database_name`, and a configuration with no `url` requires all of
    /// them. Called from [`load`], so a bad configuration fails before a pool exists rather
    /// than at the first connection attempt — which in a rolling deploy is after the process
    /// has reported itself listening.
    ///
    /// # Errors
    ///
    /// Returns [`settings::Error::Invalid`] naming the fields that are missing, or the
    /// fields that contradict `url`.
    pub fn validate(&self) -> Result<(), settings::Error> {
        let structured: Vec<&str> = [
            ("host", !self.host.is_empty()),
            ("port", self.port != 0),
            ("username", !self.username.is_empty()),
            ("database_name", !self.database_name.is_empty()),
        ]
        .into_iter()
        .filter_map(|(name, set)| set.then_some(name))
        .collect();

        if self.url.is_empty() {
            let missing: Vec<&str> = ["host", "port", "username", "database_name"]
                .into_iter()
                .filter(|name| !structured.contains(name))
                .collect();
            if !missing.is_empty() {
                return Err(settings::Error::Invalid(format!(
                    "database.url is unset and the structured fields are incomplete — \
                     set database.url, or set {}",
                    missing.join(", ")
                )));
            }
            return Ok(());
        }

        let mut contradicting = structured;
        if !self.password.is_empty() {
            contradicting.push("password");
        }
        if contradicting.is_empty() {
            Ok(())
        } else {
            Err(settings::Error::Invalid(format!(
                "database.url is set together with {} — a URL excludes the structured \
                 fields, and setting both means two deployments' worth of configuration in \
                 one process",
                contradicting.join(", ")
            )))
        }
    }

    /// Connection URL for this database, after [`Self::validate`].
    #[must_use]
    pub fn connection_url(&self) -> String {
        if !self.url.is_empty() {
            return self.url.clone();
        }
        format!(
            "postgres://{}:{}@{}:{}/{}",
            self.username, self.password, self.host, self.port, self.database_name
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplicationSettings {
    pub host: String,
    /// Port to bind. Zero asks the OS for a free one, which is how the integration tests
    /// run a real server without racing each other for a fixed port — read the actual
    /// port back from [`crate::startup::App::port`].
    pub port: u16,
}

/// Load settings for this service.
///
/// # Errors
///
/// Returns an error if a configuration file is malformed, a required key is missing from
/// every layer, or the `database` section contradicts itself ([`DatabaseSettings::validate`]).
pub fn load() -> Result<Settings, settings::Error> {
    let settings: Settings = settings::load(CONFIG_NAME)?;
    settings.database.validate()?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> NonZeroU32 {
        NonZeroU32::new(10).expect("10 is non-zero")
    }

    fn structured() -> DatabaseSettings {
        DatabaseSettings {
            url: String::new(),
            host: "127.0.0.1".to_owned(),
            port: 5432,
            username: "counter".to_owned(),
            password: "hunter2".to_owned(),
            database_name: "counter".to_owned(),
            max_connections: pool(),
        }
    }

    fn from_url(url: &str) -> DatabaseSettings {
        DatabaseSettings {
            url: url.to_owned(),
            host: String::new(),
            port: 0,
            username: String::new(),
            password: String::new(),
            database_name: String::new(),
            max_connections: pool(),
        }
    }

    #[test]
    fn the_structured_shape_assembles_its_url() {
        let settings = structured();
        settings.validate().expect("the committed shape is valid");
        assert_eq!(
            settings.connection_url(),
            "postgres://counter:hunter2@127.0.0.1:5432/counter"
        );
    }

    #[test]
    fn the_url_shape_is_used_verbatim() {
        // The peer-auth shape a NixOS host hands its service: a Unix socket, no password.
        let settings = from_url("postgres:///counter?host=/run/postgresql");
        settings
            .validate()
            .expect("a URL-only configuration is valid");
        assert_eq!(
            settings.connection_url(),
            "postgres:///counter?host=/run/postgresql"
        );
    }

    #[test]
    fn a_url_plus_structured_fields_is_an_error_naming_them() {
        let settings = DatabaseSettings {
            url: "postgres:///counter?host=/run/postgresql".to_owned(),
            ..structured()
        };

        let message = settings
            .validate()
            .expect_err("both shapes at once must be refused")
            .to_string();

        for field in ["host", "port", "username", "password", "database_name"] {
            assert!(
                message.contains(field),
                "the error should name {field}, got: {message}"
            );
        }
    }

    #[test]
    fn a_configuration_with_no_url_needs_every_structured_field() {
        let settings = DatabaseSettings {
            host: "127.0.0.1".to_owned(),
            port: 5432,
            ..from_url("")
        };

        let message = settings
            .validate()
            .expect_err("a partial structured shape must be refused")
            .to_string();

        assert!(
            message.contains("username") && message.contains("database_name"),
            "the error should name what is missing, got: {message}"
        );
        assert!(
            !message.contains("host,"),
            "host was set and should not be reported missing, got: {message}"
        );
    }

    #[test]
    fn an_empty_environment_value_cannot_stand_in_for_a_structured_field() {
        // `APP__DATABASE__HOST=""` overrides the file with an empty value, which is a
        // decision (settings' own test pins it). Here it has to fail loudly rather than
        // produce `postgres://user:@:5432/db`, which parses and dials nowhere.
        let settings = DatabaseSettings {
            host: String::new(),
            ..structured()
        };

        assert!(settings.validate().is_err(), "an empty host is not a host");
    }
}

#[cfg(test)]
mod committed_tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    /// The configuration directory as committed — not a fixture.
    ///
    /// The bug this module exists to catch was a *file* problem: the base `api.toml` named a
    /// database location, so every deployment that supplied a URL (`CloudNativePG`'s `uri`, a
    /// peer-auth socket) was refused at startup. A test that builds `DatabaseSettings` by hand
    /// cannot see that, because the contradiction is between the committed defaults and the
    /// deployment's environment. So these load the real directory.
    fn committed() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config")
    }

    /// The profile a deployment runs: its own overlay, so the committed `local` one (which
    /// names the development database) does not apply.
    const DEPLOYMENT: &str = "production";

    #[test]
    fn the_committed_base_file_accepts_a_url_from_the_environment() {
        let settings: Settings = settings::load_from_env(
            &committed(),
            CONFIG_NAME,
            DEPLOYMENT,
            [(
                "APP__DATABASE__URL".to_owned(),
                "postgres:///counter?host=/run/postgresql".to_owned(),
            )],
        )
        .expect("a deployment supplies its own database location");

        settings
            .database
            .validate()
            .expect("a URL is a complete location");
        assert_eq!(
            settings.database.connection_url(),
            "postgres:///counter?host=/run/postgresql"
        );
        assert_eq!(
            settings.database.max_connections.get(),
            10,
            "the pool size is the base file's, because it is a property of the process"
        );
    }

    #[test]
    fn the_committed_base_file_accepts_a_url_from_a_file() {
        // The shape the image is deployed with: a mounted secret, named by APP__…_FILE. The
        // image bakes only the base file, so this is the whole configuration a CNPG or
        // systemd-credential deployment has.
        let dir = TempDir::new().expect("a temp dir");
        let path = dir.path().join("database-url");
        std::fs::write(&path, "postgres:///counter?host=/run/postgresql\n")
            .expect("write the URL file");

        let settings: Settings = settings::load_from_env(
            &committed(),
            CONFIG_NAME,
            DEPLOYMENT,
            [(
                "APP__DATABASE__URL_FILE".to_owned(),
                path.to_string_lossy().into_owned(),
            )],
        )
        .expect("a URL in a file is a complete location");

        settings
            .database
            .validate()
            .expect("a URL is a complete location");
        assert_eq!(
            settings.database.connection_url(),
            "postgres:///counter?host=/run/postgresql",
            "the file's trailing newline is trimmed, not part of the URL"
        );
    }

    #[test]
    fn the_local_profile_still_names_a_structured_database() {
        // devenv's view: the base file plus `api.local.toml`, which is where the development
        // location lives now. If the split broke this, the dev shell would start with no
        // database at all — the same bug in the other direction.
        let settings: Settings = settings::load_from_env(&committed(), CONFIG_NAME, "local", [])
            .expect("the dev shell's configuration loads");

        settings
            .database
            .validate()
            .expect("the overlay carries the whole structured shape");
        assert_eq!(
            settings.database.connection_url(),
            "postgres://counter:@127.0.0.1:5432/counter"
        );
    }
}
