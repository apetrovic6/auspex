//! `cargo xtask add-auth-oidc <service>`.
//!
//! The template ships an identity *seam* and a verifier that authenticates nobody
//! (`docs/adr/0012`), because which credential a product accepts is the most product-specific
//! decision in the tree. This is the command that makes it, for one service, when a product
//! decides its credential is an OIDC provider's access token (`docs/adr/0027`). Nothing changes
//! in a product until it is run — there is no `[auth]` section, no dependency and no crate
//! before it, which is the point: a configuration section that no code reads is furniture, and
//! this one is read by exactly the code this command installs.
//!
//! # What it writes
//!
//! - **`crates/oidc`**, once. Written from the sources embedded below (`fixtures/oidc/`) because
//!   a product has no `templates/` to read them from, and left alone when it is already there, so
//!   a second service is only wired. The template carries the same crate for real, where
//!   `ci-test` compiles and runs it; `the_embedded_crate_is_the_crate` keeps the two identical.
//! - **In the workspace manifest:** any missing OIDC-only dependencies, enclosed by
//!   `# --- added by cargo xtask add-auth-oidc ---` and its `end added by` counterpart.
//!   These product-side ownership markers survive template stripping and let future updates
//!   distinguish generated entries from dependencies the product already chose.
//! - **In the service:** an `[auth]` settings section whose `validate()` runs before anything
//!   else starts, the `oidc` dependency, the verifier in place of `identity::NoIdentities`, the
//!   audience in `config/<name>.toml`, and a test file that drives `Whoami` through the real
//!   router, the real verifier and a provider in the test process.
//!
//! # What it does not do
//!
//! It does not touch readiness. A provider outage must not take every replica out of rotation,
//! public methods included — the same reasoning that keeps `/healthz` off the database. The
//! closing output says so, the way `add-database` names the probe.

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};

use super::{Service, Writes, read_for_edit, report, rustedit, write_planned};
use crate::scaffold::Sentinels;

/// The crate's files as embedded, as `(path inside crates/oidc, contents)`.
///
/// A table rather than a directory walk: `include_str!` is the only way to carry a file inside
/// the binary, and a product runs this from a binary built where `fixtures/` is not on the path
/// a walk would look at.
const OIDC_FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        include_str!("../../fixtures/oidc/Cargo.toml.in"),
    ),
    ("src/lib.rs", include_str!("../../fixtures/oidc/src/lib.rs")),
    (
        "src/http.rs",
        include_str!("../../fixtures/oidc/src/http.rs"),
    ),
    (
        "src/keys.rs",
        include_str!("../../fixtures/oidc/src/keys.rs"),
    ),
    (
        "src/tests.rs",
        include_str!("../../fixtures/oidc/src/tests.rs"),
    ),
    (
        "src/testing/mod.rs",
        include_str!("../../fixtures/oidc/src/testing/mod.rs"),
    ),
    (
        "src/testing/test-only-signing-key-a.pem",
        include_str!("../../fixtures/oidc/src/testing/test-only-signing-key-a.pem"),
    ),
    (
        "src/testing/test-only-signing-key-a.pub.pem",
        include_str!("../../fixtures/oidc/src/testing/test-only-signing-key-a.pub.pem"),
    ),
    (
        "src/testing/test-only-signing-key-b.pem",
        include_str!("../../fixtures/oidc/src/testing/test-only-signing-key-b.pem"),
    ),
    (
        "src/testing/test-only-tls-ca.pem",
        include_str!("../../fixtures/oidc/src/testing/test-only-tls-ca.pem"),
    ),
    (
        "src/testing/test-only-tls-leaf.pem",
        include_str!("../../fixtures/oidc/src/testing/test-only-tls-leaf.pem"),
    ),
    (
        "src/testing/test-only-tls-leaf-key.pem",
        include_str!("../../fixtures/oidc/src/testing/test-only-tls-leaf-key.pem"),
    ),
    (
        "src/testing/test-only-ec-key.pem",
        include_str!("../../fixtures/oidc/src/testing/test-only-ec-key.pem"),
    ),
    (
        "src/testing/test-only-ed25519-key.pem",
        include_str!("../../fixtures/oidc/src/testing/test-only-ed25519-key.pem"),
    ),
    (
        "src/bff/mod.rs",
        include_str!("../../fixtures/oidc/src/bff/mod.rs"),
    ),
    (
        "src/bff/cookies.rs",
        include_str!("../../fixtures/oidc/src/bff/cookies.rs"),
    ),
    (
        "src/bff/csrf.rs",
        include_str!("../../fixtures/oidc/src/bff/csrf.rs"),
    ),
    (
        "src/bff/routes.rs",
        include_str!("../../fixtures/oidc/src/bff/routes.rs"),
    ),
    (
        "src/bff/tests.rs",
        include_str!("../../fixtures/oidc/src/bff/tests.rs"),
    ),
];

/// The service test, with `__name__` / `__snake__` / `__pascal__` sentinels and an `@DB@` marker
/// for the one thing that differs between a stateless service and one `add-database` converted.
const SERVICE_TEST: &str = include_str!("../../fixtures/oidc/service-test.rs.in");

/// Additional behavioural checks installed only by `--web`.
const WEB_SERVICE_TEST: &str = include_str!("../../fixtures/oidc/web-service-test.rs.in");

/// What the command was asked for beyond the service's name.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// Written into `config/<name>.local.toml`, so `devenv up` has an issuer. Never into the
    /// committed base file: that one is baked into the image, and an issuer is a deployment's.
    pub issuer: Option<String>,
    /// Overrides the default, `versions.toml`'s `[publish].project`.
    pub audience: Option<String>,
    /// Install the BFF (Backend For Frontend) for browser login through the service.
    /// Embeds the `bff` module in `crates/oidc`, wires `/auth/*` routes into the service,
    /// and adds cookie encryption key config.
    pub web: bool,
}

/// Install OIDC verification into a service.
///
/// # Errors
///
/// Returns an error if the service already has it, if a file it would write exists, if an
/// anchored edit does not match, or if the repository lacks something the crate needs
/// (workspace dependency entries, the `crates/*` member glob) — all before anything is written.
pub fn add_auth_oidc(name: &str, options: &Options) -> Result<()> {
    let service = Service::open(name)?;
    let outcome = convert(&service, options)?;
    let amended: Vec<&std::path::Path> = outcome
        .amended
        .iter()
        .map(std::path::PathBuf::as_path)
        .collect();

    report(&service, &outcome.written, &amended)?;

    if outcome.crate_written {
        println!("\n  crates/oidc was not in this repository; it is now.");
    } else {
        println!("\n  crates/oidc was already here and was left exactly as it was.");
    }

    println!("\n  Readiness is untouched, on purpose. `/readyz` does not consult the identity");
    println!("  provider: an outage there must not take every replica out of rotation, public");
    println!("  methods included, for the reason `/healthz` stays off the database. A request");
    println!(
        "  that needs a key it cannot fetch answers `internal`, and the replica keeps serving."
    );

    println!("\nNext:");
    println!(
        "  # The issuer is the one setting this command could not choose. The service refuses"
    );
    println!("  # to start without it, rather than starting and denying everyone.");
    println!("  export APP__AUTH__ISSUER=https://id.example.com   # or APP__AUTH__ISSUER_FILE");
    println!("  cargo test -p {}", service.name);
    println!(
        "  # `audience` is in services/{}/config/{}.toml: the OIDC client id the provider",
        service.name, service.name
    );
    println!("  # puts in a token's `aud`. Check it matches the client you registered.");
    Ok(())
}

/// What a conversion produced, so the reporting can stay outside it.
#[derive(Debug)]
pub(super) struct Outcome {
    written: Vec<PathBuf>,
    amended: Vec<PathBuf>,
    /// Whether `crates/oidc` was written by this run.
    crate_written: bool,
}

/// The conversion itself, and the half a test can drive. See `database::convert` for why it takes
/// a `Service` rather than a name.
///
/// # Errors
///
/// Same as [`add_auth_oidc`].
pub(super) fn convert(service: &Service, options: &Options) -> Result<Outcome> {
    let config = service.src("config.rs");
    let lib = service.src("lib.rs");
    let main = service.src("main.rs");
    let manifest = service.dir.join("Cargo.toml");
    let settings_toml = service.dir.join(format!("config/{}.toml", service.name));
    let local_toml = service
        .dir
        .join(format!("config/{}.local.toml", service.name));
    let tests = service.dir.join(format!("tests/{}", service.name));
    let tests_main = tests.join("main.rs");
    let tests_oidc = tests.join("oidc.rs");
    let startup = service.src("startup.rs");

    let config_text = read_for_edit(service, &config)?;
    let manifest_text = read_for_edit(service, &manifest)?;

    // First, and with the reason: running this twice, or on a service somebody wired by hand, must
    // end in a sentence that says what is already there, not in a failed anchor three edits in.
    refuse_if_installed(service, &config_text, &manifest_text)?;

    // Also before anything is written: the generated test drives the one protected method
    // that ends in `/Whoami`, and its name — read off `tests/<name>/auth.rs`, not guessed
    // from the service's name — is what the test's imports and raw path have to agree with.
    // A service whose contract is not named after it (`services/api`) is exactly the case
    // the guess gets wrong, so this read happens, and may refuse, before the first write.
    let contract = contract(service)?;

    let audience = resolve_audience(service, options)?;
    if let Some(issuer) = &options.issuer {
        check_issuer(issuer)?;
    }

    let crate_dir = service.root.join("crates/oidc");
    let crate_present = crate_dir.join("Cargo.toml").is_file();
    let mut writes = Writes::default();
    let mut root_manifest = None;
    if !crate_present {
        root_manifest = plan_workspace(service)?;
        for (path, contents) in OIDC_FILES {
            writes.add(crate_dir.join(path), (*contents).to_owned());
        }
    }
    writes.add(tests_oidc, test_file(service, &contract, options.web)?);
    writes.add(service.src("auth.rs"), auth_rs());
    let lib_text = read_for_edit(service, &lib)?;

    let settings_text = read_for_edit(service, &settings_toml)?;
    let local_text = read_for_edit(service, &local_toml)?;
    let tests_main_text = read_for_edit(service, &tests_main)?;
    let main_text = read_for_edit(service, &main)?;
    let startup_text = read_for_edit(service, &startup)?;

    let mut edited = vec![
        (
            lib.clone(),
            plan_lib(&lib_text).with_context(|| format!("editing {}", service.rel(&lib)))?,
        ),
        (
            config.clone(),
            plan_config(&config_text, &service.name, options.web)
                .with_context(|| format!("editing {}", service.rel(&config)))?,
        ),
        (
            main.clone(),
            plan_main(&main_text, &service.name, options.web)
                .with_context(|| format!("editing {}", service.rel(&main)))?,
        ),
        (
            manifest.clone(),
            plan_manifest(&manifest_text, options.web)
                .with_context(|| format!("editing {}", service.rel(&manifest)))?,
        ),
        (
            settings_toml.clone(),
            plan_settings_toml(&settings_text, &audience, options.web)
                .with_context(|| format!("editing {}", service.rel(&settings_toml)))?,
        ),
        (
            tests_main.clone(),
            plan_tests_main(&tests_main_text)
                .with_context(|| format!("editing {}", service.rel(&tests_main)))?,
        ),
    ];
    let mut amended = vec![lib, config, main, manifest, settings_toml, tests_main];
    if let Some((path, text)) = root_manifest {
        edited.push((path.clone(), text));
        amended.push(path);
    }
    if options.web {
        edited.push((
            startup.clone(),
            plan_startup(&startup_text)
                .with_context(|| format!("editing {}", service.rel(&startup)))?,
        ));
        amended.push(startup);
    }
    if let Some(issuer) = &options.issuer {
        edited.push((
            local_toml.clone(),
            plan_local_settings_toml(&local_text, issuer)
                .with_context(|| format!("editing {}", service.rel(&local_toml)))?,
        ));
        amended.push(local_toml);
    }

    let written = writes.commit(service)?;
    write_planned(service, &edited)?;

    Ok(Outcome {
        written,
        amended,
        crate_written: !crate_present,
    })
}

/// Refuse a service that already has OIDC auth, naming what shows it.
fn refuse_if_installed(service: &Service, config: &str, manifest: &str) -> Result<()> {
    let manifest_doc: toml_edit::DocumentMut = manifest.parse().with_context(|| {
        format!(
            "{} does not parse as TOML",
            service.rel(&service.dir.join("Cargo.toml"))
        )
    })?;
    let depends = manifest_doc
        .get("dependencies")
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|table| table.contains_key("oidc"));

    let evidence = if config.contains("AuthSettings") {
        Some(format!(
            "{} declares `AuthSettings`",
            service.rel(&service.src("config.rs"))
        ))
    } else if depends {
        Some(format!(
            "{} depends on `oidc`",
            service.rel(&service.dir.join("Cargo.toml"))
        ))
    } else {
        None
    };
    match evidence {
        Some(evidence) => bail!(
            "service {:?} already has authentication installed — {evidence}. Nothing to do; \
             if the wiring was done by hand and is wrong, that is a hand edit, because this \
             command installs one shape and does not repair another",
            service.name
        ),
        None => Ok(()),
    }
}

/// The audience: the flag, else the product's own name from `versions.toml`.
///
/// `[publish].project` because it is the one name the product already has that is not the
/// template's (`cli new` rewrites it), and an audience is exactly a name the product gives
/// itself to the provider. It is a default for the committed file, not a rule: the OIDC client
/// that tokens are minted for may be called anything, and the closing output says to check.
fn resolve_audience(service: &Service, options: &Options) -> Result<String> {
    if let Some(audience) = &options.audience {
        if audience.trim().is_empty() {
            bail!(
                "--audience is empty — an empty audience would accept a token for nobody in particular"
            );
        }
        return Ok(audience.clone());
    }
    let path = service.root.join("versions.toml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {} for [publish].project", service.rel(&path)))?;
    let document: toml::Value = toml::from_str(&text)
        .with_context(|| format!("{} does not parse as TOML", service.rel(&path)))?;
    let project = document
        .get("publish")
        .and_then(|publish| publish.get("project"))
        .and_then(toml::Value::as_str)
        .filter(|project| !project.is_empty());
    project.map(ToOwned::to_owned).with_context(|| {
        format!(
            "{} has no [publish].project to default the audience from — pass \
             `--audience <the OIDC client id>`",
            service.rel(&path)
        )
    })
}

/// The same rule `oidc::Config::validate` applies at startup, applied at the command line, so a
/// typo is refused where it is typed and not at the first `devenv up`.
///
/// Duplicated rather than shared because this binary cannot depend on a crate it is about to
/// write; the startup check is the authoritative one, and this one is only ever stricter or
/// equal to it by construction of the two tests that pin them to the same table.
fn check_issuer(issuer: &str) -> Result<()> {
    let rest = issuer
        .strip_prefix("https://")
        .map(|rest| (true, rest))
        .or_else(|| issuer.strip_prefix("http://").map(|rest| (false, rest)));
    let Some((secure, rest)) = rest else {
        bail!("--issuer {issuer:?} is not an https:// URL");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        bail!("--issuer {issuer:?} has no host, or carries credentials");
    }
    if rest.contains('?') || rest.contains('#') {
        bail!("--issuer {issuer:?} has a query or fragment, which an issuer identifier does not");
    }
    let host = authority.strip_prefix('[').map_or_else(
        || authority.split(':').next().unwrap_or_default(),
        |bracketed| bracketed.split(']').next().unwrap_or_default(),
    );
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !secure && !loopback {
        bail!(
            "--issuer {issuer:?} is plain http:// — accepted only for localhost, 127.0.0.0/8 \
             and ::1, because keys fetched in the clear can be replaced by anyone on the path"
        );
    }
    Ok(())
}

/// The workspace entries the crate's manifest inherits, with the reasons for each choice.
///
/// Embedded like the crate and kept identical to the block in the template's own `Cargo.toml`
/// (`the_workspace_entries_are_the_ones_the_command_writes`), because there are two copies and
/// nothing else says so.
const WORKSPACE_DEPENDENCIES: &str =
    include_str!("../../fixtures/oidc/workspace-dependencies.toml");

/// What the crate being written needs from the repository around it, and the edit that supplies
/// the part a product does not have.
///
/// `crates/*` must be a member glob (refused otherwise: the crate would be written and never
/// built), and every `workspace = true` dependency of the embedded manifest must have an entry in
/// the root `[workspace.dependencies]`. The third-party ones the crate alone uses are
/// **written** here, because `cli new` strips them from a product — unused workspace
/// dependencies make cargo warn on every invocation — and anything else the manifest inherits is a
/// refusal naming it: those are shared entries a product removed on purpose or by accident, and
/// guessing a version for them is not this command's call.
///
/// Returns the root manifest's new text, or `None` when it already has everything. Checked and
/// planned up front, because the alternative is a crate on disk that cargo then refuses to load,
/// and a workspace that does not load takes `xtask` with it, so no check in the repository can say
/// why.
fn plan_workspace(service: &Service) -> Result<Option<(PathBuf, String)>> {
    let path = service.root.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} for its workspace dependencies",
            service.rel(&path)
        )
    })?;
    let root: toml::Value = toml::from_str(&text)
        .with_context(|| format!("{} does not parse as TOML", service.rel(&path)))?;

    let members = root
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array);
    if !members.is_some_and(|members| {
        members
            .iter()
            .any(|member| member.as_str() == Some("crates/*"))
    }) {
        bail!(
            "{} does not list `crates/*` as a workspace member, so crates/oidc would be \
             written and never built",
            service.rel(&path)
        );
    }

    let available: Vec<&str> = root
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table)
        .map(|table| table.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let provided: toml::Table = toml::from_str(WORKSPACE_DEPENDENCIES)
        .context("the embedded workspace entries are not TOML")?;

    let missing: Vec<String> = oidc_workspace_dependencies()?
        .into_iter()
        .filter(|name| !available.contains(&name.as_str()) && !provided.contains_key(name))
        .collect();
    if !missing.is_empty() {
        bail!(
            "{} has no [workspace.dependencies] entry for {} — crates/oidc inherits its \
             versions from there, and this command does not choose them. Add them to the \
             workspace and run this again",
            service.rel(&path),
            missing.join(", ")
        );
    }

    let absent: Vec<&str> = provided
        .keys()
        .map(String::as_str)
        .filter(|name| !available.contains(name))
        .collect();
    if absent.is_empty() {
        return Ok(None);
    }

    // The whole block, comment and all, when none of it is there; only the missing lines when a
    // product already carries some (it has chosen its own versions for those, and a second copy of
    // the explanation would be a second place to keep right).
    let block = if absent.len() == provided.len() {
        WORKSPACE_DEPENDENCIES.to_owned()
    } else {
        let mut lines: String = WORKSPACE_DEPENDENCIES
            .lines()
            .filter(|line| {
                absent
                    .iter()
                    .any(|name| line.starts_with(&format!("{name} = ")))
            })
            .collect::<Vec<_>>()
            .join("\n");
        lines.push('\n');
        lines
    };
    let block = format!(
        "# --- added by cargo xtask add-auth-oidc ---\n\
         {block}\
         # --- end added by cargo xtask add-auth-oidc ---\n"
    );

    // Before the next table header, which is where the dependencies end. Text, not `toml_edit`,
    // because a multi-line comment block is what this inserts and the document model has no
    // place to put one.
    let mut lines: Vec<&str> = text.lines().collect();
    let header = lines
        .iter()
        .position(|line| line.trim() == "[workspace.dependencies]")
        .with_context(|| {
            format!(
                "{} has no [workspace.dependencies] table",
                service.rel(&path)
            )
        })?;
    let end = lines[header + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .map_or(lines.len(), |offset| header + 1 + offset);
    // Back over the blank lines that separate the table from the next one.
    let mut at = end;
    while at > header + 1 && lines[at - 1].trim().is_empty() {
        at -= 1;
    }
    let block_lines: Vec<&str> = block.lines().collect();
    lines.splice(at..at, block_lines);
    let mut planned = lines.join("\n");
    planned.push('\n');

    toml::from_str::<toml::Value>(&planned).with_context(|| {
        format!(
            "the entries written into {} do not parse",
            service.rel(&path)
        )
    })?;
    Ok(Some((path, planned)))
}

/// The names the embedded manifest takes from `[workspace.dependencies]`.
fn oidc_workspace_dependencies() -> Result<Vec<String>> {
    let manifest = OIDC_FILES
        .iter()
        .find_map(|(path, contents)| (*path == "Cargo.toml").then_some(*contents))
        .context("the embedded crate has no manifest")?;
    let document: toml_edit::DocumentMut = manifest.parse()?;
    let mut names = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(table) = document
            .get(section)
            .and_then(toml_edit::Item::as_table_like)
        else {
            continue;
        };
        for (name, item) in table.iter() {
            let inherited = item
                .as_table_like()
                .and_then(|entry| entry.get("workspace"))
                .and_then(toml_edit::Item::as_bool)
                .unwrap_or(false);
            if inherited {
                names.push(name.to_owned());
            }
        }
    }
    Ok(names)
}

// -------------------------------------------------------------------------------------------------
// The edits
// -------------------------------------------------------------------------------------------------

/// `src/config.rs`: the `auth` section, its type, and the validation that runs at load.
#[allow(clippy::too_many_lines)]
fn plan_config(source: &str, service: &str, web: bool) -> Result<String> {
    let auth = format!(
        r#"/// How callers prove who they are (`docs/adr/0027`).
///
/// Written by `cargo xtask add-auth-oidc`. Before it, this service verified nobody
/// (`identity::NoIdentities`), and there was deliberately no section to configure.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthSettings {{
    /// The OIDC provider's issuer, e.g. `https://id.example.com`: where its discovery document
    /// lives and what a token's `iss` must equal, as an exact string.
    ///
    /// Empty in the committed file on purpose. It names a deployment, so it arrives as
    /// `APP__AUTH__ISSUER` (or `APP__AUTH__ISSUER_FILE`) — and a service that starts without one
    /// must **stop**, not start and refuse every credential: an unconfigured verifier that denies
    /// everyone looks, from outside, like a provider outage nobody can find. [`Self::validate`]
    /// is what makes it stop, from `load`, before a pool or a socket exists.
    ///
    /// Defaulted rather than required, so a settings literal that predates the section still
    /// deserializes and is then refused by name, not by a serde "missing field".
    #[serde(default)]
    pub issuer: String,
    /// What a token's `aud` must contain: the OIDC client id the provider mints tokens for.
    /// Committed in `config/{service}.toml`, defaulting to the product's name.
    #[serde(default)]
    pub audience: String,
    /// The signing algorithms this verifier accepts. Defaults to `["RS256"]` when absent,
    /// whether the struct is deserialised (serde `default_algorithms`) or constructed by hand
    /// (the manual [`Default`] impl, so `..Default::default()` in a test does not produce an
    /// empty list). Chosen from a fixed allow-list: RS256, RS384, RS512, PS256, PS384, PS512,
    /// ES256, ES384, `EdDSA`. HS* and `none` are never accepted (an HMAC key would be the
    /// public JWK, which is a classic confusion attack).
    #[serde(default = "default_algorithms")]
    pub algorithms: Vec<String>,
{web_fields}}}

{web_functions}
fn default_algorithms() -> Vec<String> {{
    vec!["RS256".to_owned()]
}}

impl Default for AuthSettings {{
    fn default() -> Self {{
        Self {{
            issuer: String::new(),
            audience: String::new(),
            algorithms: default_algorithms(),
{web_defaults}        }}
    }}
}}

impl AuthSettings {{
    /// Refuse a configuration that cannot verify anyone.
    ///
    /// # Errors
    ///
    /// Returns [`settings::Error::Invalid`] naming the setting and how to supply it, for a
    /// missing issuer or audience, or an issuer that is not `https://` (plain `http://` is
    /// accepted only for loopback — keys fetched in the clear can be replaced by anyone on the
    /// path).
    pub fn validate(&self) -> Result<(), settings::Error> {{
        if self.issuer.trim().is_empty() {{
            return Err(settings::Error::Invalid(
                "auth.issuer is unset — set APP__AUTH__ISSUER (or APP__AUTH__ISSUER_FILE) to the \
                 OIDC provider's issuer URL, e.g. https://id.example.com. This service verifies \
                 credentials against that provider and refuses to start without one, rather \
                 than start and deny every caller"
                    .to_owned(),
            ));
        }}
        if self.audience.trim().is_empty() {{
            return Err(settings::Error::Invalid(
                "auth.audience is unset — it belongs in config/{service}.toml under [auth], as \
                 the OIDC client id the provider puts in a token's `aud`"
                    .to_owned(),
            ));
        }}
        // Validate algorithms and their issuer URL together, not inside `verifier_config`.
        // `oidc::AlgorithmSet::from_strs` returns a structured error for empty, unknown or
        // duplicate entries, each naming the offending value. Every code path into the
        // verifier goes through this method, so `verifier_config` can safely expect.
        let _ = oidc::AlgorithmSet::from_strs(&self.algorithms)
            .map_err(|error| settings::Error::Invalid(format!("auth.algorithms: {{error}}")))?;
{web_validate}        self.verifier_config()
            .validate()
            .map_err(|error| settings::Error::Invalid(format!("auth: {{error}}")))
    }}


    /// The verifier's configuration, after [`Self::validate`].
    ///
    /// # Panics
    ///
    /// Panics if the algorithms list is invalid, which [`Self::validate`] already refused —
    /// the `expect` here is a second line of defence for a caller that built the settings
    /// some other way.
    #[must_use]
    pub fn verifier_config(&self) -> oidc::Config {{
        oidc::Config {{
            issuer: self.issuer.clone(),
            audience: self.audience.clone(),
            algorithms: oidc::AlgorithmSet::from_strs(&self.algorithms)
                .expect("algorithms validated at load"),
        }}
    }}
}}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplicationSettings {{"#,
        web_fields = if web {
            r#"    /// Public browser origin, e.g. `https://app.example.com`, without a path.
    /// Required by `add-auth-oidc --web`; used for redirects and CSRF checks.
    #[serde(default)]
    pub public_url: String,
    /// `OAuth2` scopes requested at login (e.g. `openid profile email`).
    /// Configured by `add-auth-oidc --web`.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// Optional `audience` parameter sent to the authorization endpoint.
    /// Configured by `add-auth-oidc --web`.
    #[serde(default)]
    pub login_audience: Option<String>,
    /// Secret cookie encryption key: standard padded base64 of 32 random bytes.
    /// Generate with `openssl rand -base64 32`; supply `APP__AUTH__COOKIE_KEY` or
    /// `APP__AUTH__COOKIE_KEY_FILE` pointing to a deployment secret containing that text.
    #[serde(default)]
    pub cookie_key: oidc::bff::cookies::CookieKeySetting,
"#
        } else {
            ""
        },
        web_defaults = if web {
            r"            public_url: String::new(),
            scopes: default_scopes(),
            login_audience: None,
            cookie_key: oidc::bff::cookies::CookieKeySetting::default(),
"
        } else {
            ""
        },
        web_validate = if web {
            r#"        if self.public_url.trim().is_empty() {
            return Err(settings::Error::Invalid(
                "auth.public_url is unset — set it to the public https origin this service \
                 is served at, e.g. https://app.example.com".to_owned(),
            ));
        }
        oidc::bff::parse_public_url(&self.public_url)
            .map_err(|error| settings::Error::Invalid(format!("auth.public_url: {error}")))?;
        if self.scopes.is_empty()
            || !self.scopes.iter().any(|scope| scope == "openid")
            || self.scopes.iter().any(|scope| {
                scope.is_empty() || scope.chars().any(char::is_whitespace)
            })
        {
            return Err(settings::Error::Invalid(
                "auth.scopes must contain openid and only nonempty scopes without whitespace"
                    .to_owned(),
            ));
        }
        if self.login_audience.as_ref().is_some_and(|value| value.trim().is_empty()) {
            return Err(settings::Error::Invalid(
                "auth.login_audience must be nonempty when set".to_owned(),
            ));
        }
        self.cookie_key.decode()
            .map_err(|error| settings::Error::Invalid(format!("auth.cookie_key: {error}")))?;
"#
        } else {
            ""
        },
        web_functions = if web {
            r#"fn default_scopes() -> Vec<String> {
    vec!["openid".to_owned(), "profile".to_owned(), "email".to_owned()]
}
"#
        } else {
            ""
        },
    );

    // Two shapes of `load`, because `add-database` rewrote it: it validates the database after
    // loading, and this goes beside that. Neither shape matching is a refusal that quotes the
    // anchor, like every other edit here.
    let load = if source.contains("    settings.database.validate()?;") {
        rustedit::Edit {
            what: "validate the auth section beside the database one",
            find: "    settings.database.validate()?;",
            replace: "    settings.database.validate()?;\n    settings.auth.validate()?;"
                .to_owned(),
        }
    } else {
        rustedit::Edit {
            what: "validate the auth section after loading it",
            find: "pub fn load() -> Result<Settings, settings::Error> {\n    settings::load(CONFIG_NAME)\n}",
            replace: "pub fn load() -> Result<Settings, settings::Error> {\n    let settings: Settings = settings::load(CONFIG_NAME)?;\n    settings.auth.validate()?;\n    Ok(settings)\n}".to_owned(),
        }
    };

    rustedit::apply(
        source,
        &[
            rustedit::Edit {
                what: "add `auth` to Settings",
                find: "    pub application: ApplicationSettings,",
                replace: "    pub application: ApplicationSettings,\n    /// Who may call this service. Absent is refused at load, not treated as \"nobody\".\n    #[serde(default)]\n    pub auth: AuthSettings,".to_owned(),
            },
            rustedit::Edit {
                what: "add the AuthSettings type",
                find: "#[derive(Debug, Clone, Deserialize)]\npub struct ApplicationSettings {",
                replace: auth,
            },
            load,
        ],
    )
}

/// `src/main.rs`: the verifier, where `identity::NoIdentities` was.
fn plan_main(source: &str, name: &str, web: bool) -> Result<String> {
    let snake = name.replace('-', "_");
    let source = rustedit::apply(
        source,
        &[
            rustedit::Edit {
                what: "replace the shipped verifier with the OIDC one",
                find: "            let verifier = Arc::new(identity::NoIdentities);",
                replace: format!(
                    "            // Installed by `cargo xtask add-auth-oidc` (`docs/adr/0027`) in place of\n\
                     \x20           // `identity::NoIdentities`, so the comment above is the history of this line and\n\
                     \x20           // not what it does now. `auth::verifier` is a function in the library so that\n\
                     \x20           // `tests/{name}/oidc.rs` builds its router through the very call the service\n\
                     \x20           // makes: a line in this binary is one no test can reach. It fetches nothing\n\
                     \x20           // here — keys are loaded on the first credential, which is what lets a replica\n\
                     \x20           // boot through a provider outage and serve its public methods — and\n\
                     \x20           // `config::load` already refused a missing issuer.\n\
                     \x20           let verifier = {snake}::auth::verifier(&settings.auth)\n\
                     \x20               .context(\"could not build the OIDC verifier\")?;",
                ),
            },
            rustedit::Edit {
                what: "drop the `Arc` import the verifier no longer needs",
                find: "use std::sync::Arc;\n\n",
                replace: String::new(),
            },
        ],
    )?;
    if web {
        rustedit::apply(
            &source,
            &[rustedit::Edit {
                what: "build and merge the BFF router after App::build",
                find: "            app.run_until_stopped().await.context(\"server failed\")?;",
                replace: "            // Config load requires and validates the public origin and cookie key.\n\
                     \x20           // Mount the BFF unconditionally, alongside the app's SPA fallback.\n\
                     \x20           let key = settings.auth.cookie_key.decode()\n\
                     \x20               .expect(\"cookie_key validated at load\");\n\
                     \x20           let bff = oidc::bff::bff_router(oidc::bff::BffSetup {\n\
                     \x20               issuer: settings.auth.issuer.clone(),\n\
                     \x20               client_id: settings.auth.audience.clone(),\n\
                     \x20               public_url: oidc::bff::parse_public_url(&settings.auth.public_url)\n\
                     \x20                   .expect(\"public_url validated at load\"),\n\
                     \x20               scopes: settings.auth.scopes.clone(),\n\
                     \x20               login_audience: settings.auth.login_audience.clone(),\n\
                     \x20               cookie_key: key,\n\
                     \x20           });\n\
                     \x20           app.with_routes(bff).run_until_stopped().await.context(\"server failed\")?;".to_string(),
            }],
        )
    } else {
        Ok(source)
    }
}

/// `src/auth.rs`: the verifier this service installs, as a function a test can call.
fn auth_rs() -> String {
    "//! The verifier this service installs, in one place so a test can reach it.\n\
     //!\n\
     //! Written by `cargo xtask add-auth-oidc` (`docs/adr/0027`). `main.rs` is a binary, and\n\
     //! nothing under `tests/` can run it: a line there that was wrong, or was later edited back to\n\
     //! `identity::NoIdentities`, would pass every test while authenticating nobody. This is that\n\
     //! line as a function. `main.rs` calls it, and `tests/*/oidc.rs` builds its router through it,\n\
     //! so the suite drives the verifier the service actually runs.\n\
     \n\
     use std::sync::Arc;\n\
     \n\
     use identity::VerifyCredential;\n\
     \n\
     use crate::config::AuthSettings;\n\
     \n\
     /// The credential verifier for this service's `[auth]` settings.\n\
     ///\n\
     /// Fetches nothing: the provider's keys are loaded when the first credential needs them, so a\n\
     /// replica that starts during a provider outage starts.\n\
     ///\n\
     /// # Errors\n\
     ///\n\
     /// Returns the verifier's configuration error when the issuer or audience is unusable —\n\
     /// which `config::load` has already refused, so this is a second line of defence for a caller\n\
     /// that built the settings some other way.\n\
     pub fn verifier(settings: &AuthSettings) -> Result<Arc<dyn VerifyCredential>, oidc::ConfigError> {\n\
     \x20   Ok(Arc::new(oidc::Verifier::new(&settings.verifier_config())?))\n\
     }\n"
        .to_owned()
}

/// `src/lib.rs`: declare the module.
fn plan_lib(source: &str) -> Result<String> {
    rustedit::insert_mod(source, "pub mod auth;", "pub mod config;")
}

/// `src/startup.rs`: let the binary mount additional routes without exposing router internals.
fn plan_startup(source: &str) -> Result<String> {
    rustedit::apply(
        source,
        &[rustedit::Edit {
            what: "add a public consuming router integration method",
            find: "    /// Serve until the process is asked to stop.",
            replace: "    /// Mount additional routes, retaining the application's SPA fallback.\n\
                 \x20   #[must_use]\n\
                 \x20   pub fn with_routes(mut self, routes: axum::Router) -> Self {\n\
                 \x20       self.router = routes.merge(self.router);\n\
                 \x20       self\n\
                 \x20   }\n\n\
                 \x20   /// Serve until the process is asked to stop."
                .to_owned(),
        }],
    )
}

/// `Cargo.toml`: `oidc` as a dependency, and with its test support as a dev-dependency.
///
/// Path dependencies, as the other internal crates are, and not `workspace = true`: the workspace
/// table is third-party only (`Cargo.toml` says so).
fn plan_manifest(source: &str, web: bool) -> Result<String> {
    let mut document: toml_edit::DocumentMut = source.parse()?;

    let mut dependency = toml_edit::InlineTable::new();
    dependency.insert("path", "../../crates/oidc".into());
    if web {
        let mut features = toml_edit::Array::new();
        features.push("bff");
        dependency.insert("features", toml_edit::Value::Array(features));
    }
    let dependencies = document
        .get_mut("dependencies")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| anyhow::anyhow!("the manifest has no [dependencies] table"))?;
    dependencies.insert(
        "oidc",
        toml_edit::Item::Value(toml_edit::Value::InlineTable(dependency)),
    );
    if let Some(mut key) = dependencies.key_mut("oidc") {
        key.leaf_decor_mut()
            .set_prefix("# Verifies OIDC access tokens; wired in `main.rs` (docs/adr/0027).\n");
    }

    let mut test_support = toml_edit::InlineTable::new();
    test_support.insert("path", "../../crates/oidc".into());
    let mut features = toml_edit::Array::new();
    features.push("testing");
    test_support.insert("features", toml_edit::Value::Array(features));
    if document.get("dev-dependencies").is_none() {
        document["dev-dependencies"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let dev = document
        .get_mut("dev-dependencies")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| anyhow::anyhow!("[dev-dependencies] is not a table"))?;
    dev.insert(
        "oidc",
        toml_edit::Item::Value(toml_edit::Value::InlineTable(test_support)),
    );
    if let Some(mut key) = dev.key_mut("oidc") {
        key.leaf_decor_mut().set_prefix(
            "# An identity provider that lives in the test process, and the PUBLIC keys it signs with\n\
             # (`oidc::testing`). Dev-only: no binary carries them.\n",
        );
    }
    if web && !dev.contains_key("tempfile") {
        dev.insert("tempfile", toml_edit::value("3"));
    }

    Ok(document.to_string())
}

/// `config/<name>.toml`: the audience, and a note on where the issuer comes from.
///
/// Refused when the file already has an `auth` table: appending a second one gives a file that
/// does not parse, and a table somebody wrote is not this command's to edit.
fn plan_settings_toml(source: &str, audience: &str, web: bool) -> Result<String> {
    refuse_existing_auth_table(source)?;
    let audience = toml_string(audience);
    let web_settings = if web {
        "\n# OAuth2 scopes requested when a browser logs in. openid and profile are needed\n\
         # for the display name; offline_access keeps the session alive past the browser\n\
         # session (the refresh token is in an HttpOnly cookie).\n\
         scopes = [\"openid\", \"profile\", \"email\"]\n\
         # Optional `audience` parameter sent to the authorization endpoint.\n\
         # login_audience = \"https://api.example.com\"\n\
         # Required public browser origin, without a path, query or fragment.\n\
         # http is allowed only for localhost or 127.0.0.1 development origins.\n\
         # public_url = \"https://app.example.com\"\n\
         # Secret cookie encryption key: standard padded base64 of 32 random bytes.\n\
         # Generate with: openssl rand -base64 32\n\
         # Never commit the key. Supply APP__AUTH__COOKIE_KEY, or set\n\
         # APP__AUTH__COOKIE_KEY_FILE=/run/secrets/cookie-key to read its base64 text.\n"
    } else {
        ""
    };
    Ok(format!(
        "{}\n\n[auth]\n\
         # What a token's `aud` must contain: the OIDC client id the provider mints tokens for.\n\
         # Defaults to the product's name (versions.toml [publish].project); change it to match the\n\
         # client you registered.\n\
         #\n\
         # No `issuer` here, deliberately. It names a deployment, this file is baked into the\n\
         # image, and a service that starts without one refuses to (`AuthSettings::validate`),\n\
         # where a committed issuer would be one more value every environment inherits by accident.\n\
         # It arrives as APP__AUTH__ISSUER or APP__AUTH__ISSUER_FILE; `config/<name>.local.toml`\n\
         # carries the development one (`add-auth-oidc --issuer`).\n\
         audience = {audience}\n\
         {web_settings}",
        source.trim_end(),
    ))
}

/// `config/<name>.local.toml`: the development issuer, for `devenv up`.
fn plan_local_settings_toml(source: &str, issuer: &str) -> Result<String> {
    refuse_existing_auth_table(source)?;
    let issuer = toml_string(issuer);
    Ok(format!(
        "{}\n\n[auth]\n\
         # The provider the development stack verifies against. Not baked into the image: a\n\
         # deployment names its own issuer through APP__AUTH__ISSUER.\n\
         issuer = {issuer}\n",
        source.trim_end(),
    ))
}

/// `text` as a TOML basic string, quoted and escaped the way `toml_edit` writes one.
fn toml_string(text: &str) -> String {
    toml_edit::value(text).to_string().trim().to_owned()
}

fn refuse_existing_auth_table(source: &str) -> Result<()> {
    let document: toml_edit::DocumentMut = source
        .parse()
        .context("it does not parse as TOML, so there is nothing to append to")?;
    if document.get("auth").is_some() {
        bail!(
            "this file already has an `auth` table this command did not write. Add `audience` \
             (and, locally, `issuer`) to it yourself, or delete the table and run this again"
        );
    }
    Ok(())
}

/// `tests/<name>/main.rs`: declare the new test module.
fn plan_tests_main(source: &str) -> Result<String> {
    rustedit::insert_mod(source, "mod oidc;", "mod auth;")
}

/// The Connect contract the service's `Whoami` RPC lives in, as the generated test spells it.
struct Contract {
    /// The proto package as a Rust module path under `buffa` / `connect`: `counter::v1`.
    package: String,
    /// The generated client type: `CounterServiceClient`.
    client: String,
    /// The raw method path without `/Whoami`: `counter.v1.CounterService`.
    path: String,
}

impl Contract {
    /// One classification-table entry, `counter.v1.CounterService/Whoami`, split into the
    /// three spellings the test needs.
    fn from_whoami(entry: &str) -> Result<Self> {
        let path = entry
            .strip_suffix("/Whoami")
            .ok_or_else(|| anyhow::anyhow!("{entry:?} does not end in `/Whoami`"))?;
        let mut parts = path.split('.');
        let service = parts
            .next_back()
            .ok_or_else(|| anyhow::anyhow!("{path:?} has no service name"))?;
        let package = parts.collect::<Vec<_>>().join(".");
        if package.is_empty() {
            bail!("{entry:?} names no proto package before the service name");
        }
        Ok(Self {
            package: package.replace('.', "::"),
            client: format!("{service}Client"),
            path: path.to_owned(),
        })
    }
}

/// The one protected method that returns an identity, read off the classification table.
///
/// `tests/<service>/auth.rs` is the only place a service writes down what it routes that is
/// not the contract itself (`docs/adr/0012`): `PROTECTED` names each protected method the way
/// `Router::methods()` answers to it, and the generated test drives the one ending in
/// `/Whoami`. Reading it here rather than guessing from the service's name is the whole fix:
/// a service may serve a contract that is not named after it (`services/api` serves
/// `counter.v1.CounterService`), and the name-guess produced a test that did not compile.
///
/// # Errors
///
/// Returns an error when the file declares no `PROTECTED` list, when no entry in it ends in
/// `/Whoami`, or when more than one does — all before anything is written.
fn contract(service: &Service) -> Result<Contract> {
    let auth = service.auth_test();
    let source = read_for_edit(service, &auth)?;
    let protected = rustedit::read_const_list(&source, "PROTECTED")
        .with_context(|| format!("reading the PROTECTED list in {}", service.rel(&auth)))?;
    let whoami: Vec<&str> = protected
        .iter()
        .map(String::as_str)
        .filter(|method| method.ends_with("/Whoami"))
        .collect();
    match whoami.as_slice() {
        [] => bail!(
            "{} has no `.../Whoami` entry in its PROTECTED list, so there is nothing to tell \
             the generated test which contract to drive through the router. It expects exactly \
             one protected method spelled `<package>.<Service>/Whoami` — the method that \
             returns an identity. Add it (see `docs/adr/0012`), or say what replaced `Whoami` \
             and update the generated test to match.",
            service.rel(&auth)
        ),
        [one] => Contract::from_whoami(one).with_context(|| {
            format!(
                "reading the PROTECTED list in {} — the entry {one:?}",
                service.rel(&auth)
            )
        }),
        many => bail!(
            "{} names {} `.../Whoami` entries in its PROTECTED list ({}) — the generated test \
             drives exactly one. Keep the method that returns an identity and move the others \
             out of PROTECTED.",
            service.rel(&auth),
            many.len(),
            many.join(", ")
        ),
    }
}

/// `tests/<name>/oidc.rs`: `Whoami` through the real router and the real verifier.
fn test_file(service: &Service, contract: &Contract, web: bool) -> Result<String> {
    let database = if service.has_database() {
        "crate::helpers::dead_pool(), "
    } else {
        ""
    };
    let mut test = SERVICE_TEST.to_owned();
    if web {
        test = rustedit::apply(
            &test,
            &[
                rustedit::Edit {
                    what: "supply web settings to the verifier test",
                    find: "        ..Default::default()",
                    replace: "        ..web_defaults()".to_owned(),
                },
                rustedit::Edit {
                    what: "supply web settings to the algorithm tests",
                    find: "        algorithms,\n    }",
                    replace: "        algorithms,\n        ..web_defaults()\n    }".to_owned(),
                },
                rustedit::Edit {
                    what: "load web settings with a test-only cookie key",
                    find: r#"    settings::load_from_env(
        &committed(),
        CONFIG_NAME,
        "production",
        environment
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
    )
    .expect("the committed configuration loads")"#,
                    replace: "    load_web(environment)".to_owned(),
                },
            ],
        )?;
        test.push_str(WEB_SERVICE_TEST);
    }
    Ok(Sentinels::new(&service.name)
        .apply(&test)
        .replace("@PKG@", &contract.package)
        .replace("@CLIENT@", &contract.client)
        .replace("@PATH@", &contract.path)
        .replace("@DB@", database))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::fixtures::{scaffolded_service, service_with_database, tree};

    /// A repository root that has what the crate needs from it: the workspace entries, the glob,
    /// and a `versions.toml`. The temporary tree `scaffolded_service` builds has none of these, so
    /// this adds them — copied from the real files, so a change to the template's own entries is
    /// what the test reads.
    fn give_the_workspace(root: &std::path::Path) {
        let real = crate::repo::root();
        // What `cli new` leaves: the template's manifest without the block it strips.
        let manifest = std::fs::read_to_string(real.join("Cargo.toml")).expect("read Cargo.toml");
        std::fs::write(root.join("Cargo.toml"), strip_template_only(&manifest))
            .expect("write Cargo.toml");
        std::fs::copy(real.join("versions.toml"), root.join("versions.toml"))
            .expect("copy versions.toml");
    }

    /// `tools/cli`'s `strip_marked`, for the one marker pair; the CLI is not a dependency of this
    /// crate, and the round trip that proves the two agree is `check-archetypes`.
    fn strip_template_only(text: &str) -> String {
        let mut out = Vec::new();
        let mut inside = false;
        for line in text.lines() {
            if line.starts_with("# --- template-only") {
                inside = true;
            } else if line.starts_with("# --- end template-only") {
                inside = false;
            } else if !inside {
                out.push(line);
            }
        }
        out.join("\n") + "\n"
    }

    fn skip_without_template() -> bool {
        !crate::repo::root().join("templates/service").is_dir()
    }

    /// The service skeleton has a local overlay and a tests main, as `just add service` renders.
    fn converted(root: &std::path::Path, options: &Options) -> (Service, Outcome) {
        let service = scaffolded_service(root);
        give_the_workspace(root);
        let outcome = convert(&service, options).expect("the skeleton must convert");
        (service, outcome)
    }

    #[test]
    fn a_scaffolded_service_converts() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (service, outcome) = converted(dir.path(), &Options::default());

        assert!(outcome.crate_written);
        assert!(service.root.join("crates/oidc/Cargo.toml").is_file());
        assert!(service.root.join("crates/oidc/src/lib.rs").is_file());

        let config = std::fs::read_to_string(service.src("config.rs")).expect("config.rs");
        assert!(config.contains("pub auth: AuthSettings,"), "{config}");
        assert!(config.contains("settings.auth.validate()?;"), "{config}");
        // P1: the generated AuthSettings has a manual Default impl that includes
        // algorithms = ["RS256"], not a derived one that would give algorithms = [].
        assert!(
            config.contains("impl Default for AuthSettings"),
            "Default must be manual so algorithms defaults to [RS256]: {config}"
        );
        assert!(
            !config.contains("#[derive(..Default"),
            "Default must not be derived: {config}"
        );

        let main = std::fs::read_to_string(service.src("main.rs")).expect("main.rs");
        assert!(
            main.contains("demo::auth::verifier(&settings.auth)"),
            "{main}"
        );
        assert!(
            !main.contains("use std::sync::Arc;"),
            "the import the call leaves unused must go: {main}"
        );
        let lib = std::fs::read_to_string(service.src("lib.rs")).expect("lib.rs");
        assert!(lib.contains("pub mod auth;"), "{lib}");
        let auth = std::fs::read_to_string(service.src("auth.rs")).expect("auth.rs");
        assert!(auth.contains("oidc::Verifier::new"), "{auth}");
        assert!(!main.contains("Arc::new(identity::NoIdentities)"), "{main}");

        let manifest = std::fs::read_to_string(service.dir.join("Cargo.toml")).expect("manifest");
        assert!(
            manifest.contains("oidc = { path = \"../../crates/oidc\" }"),
            "{manifest}"
        );
        assert!(manifest.contains("features = [\"testing\"]"), "{manifest}");

        let settings =
            std::fs::read_to_string(service.dir.join("config/demo.toml")).expect("settings");
        assert!(settings.contains("[auth]"), "{settings}");
        assert!(
            settings.contains("audience = \"rust-app-template\""),
            "{settings}"
        );
        assert!(
            !settings.contains("issuer ="),
            "the committed file must not name an issuer: {settings}"
        );

        let tests_main =
            std::fs::read_to_string(service.dir.join("tests/demo/main.rs")).expect("main");
        assert!(tests_main.contains("mod oidc;"), "{tests_main}");
        assert!(service.dir.join("tests/demo/oidc.rs").is_file());
    }

    #[test]
    fn the_audience_defaults_to_the_publish_project() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        let versions = std::fs::read_to_string(dir.path().join("versions.toml")).expect("versions");
        std::fs::write(
            dir.path().join("versions.toml"),
            versions.replace(
                "project = \"rust-app-template\"",
                "project = \"acme-ledger\"",
            ),
        )
        .expect("rewrite versions.toml");

        convert(&service, &Options::default()).expect("converts");

        let settings =
            std::fs::read_to_string(service.dir.join("config/demo.toml")).expect("settings");
        assert!(
            settings.contains("audience = \"acme-ledger\""),
            "{settings}"
        );
    }

    #[test]
    fn an_explicit_audience_wins() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (service, _) = converted(
            dir.path(),
            &Options {
                audience: Some("client-123".to_owned()),
                ..Options::default()
            },
        );
        let settings =
            std::fs::read_to_string(service.dir.join("config/demo.toml")).expect("settings");
        assert!(settings.contains("audience = \"client-123\""), "{settings}");
    }

    #[test]
    fn the_issuer_flag_goes_to_the_local_overlay_and_never_the_committed_file() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (service, outcome) = converted(
            dir.path(),
            &Options {
                issuer: Some("https://id.example.com".to_owned()),
                ..Options::default()
            },
        );
        let local =
            std::fs::read_to_string(service.dir.join("config/demo.local.toml")).expect("local");
        assert!(
            local.contains("issuer = \"https://id.example.com\""),
            "{local}"
        );
        let base = std::fs::read_to_string(service.dir.join("config/demo.toml")).expect("base");
        assert!(!base.contains("id.example.com"), "{base}");
        assert!(
            outcome
                .amended
                .iter()
                .any(|path| path.ends_with("demo.local.toml"))
        );
    }

    #[test]
    fn without_the_flag_the_local_overlay_is_left_alone() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        let before =
            std::fs::read_to_string(service.dir.join("config/demo.local.toml")).expect("local");

        convert(&service, &Options::default()).expect("converts");

        let after =
            std::fs::read_to_string(service.dir.join("config/demo.local.toml")).expect("local");
        assert_eq!(before, after);
    }

    #[test]
    fn a_converted_service_with_a_database_gets_the_pool_in_its_test() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = service_with_database(dir.path());
        give_the_workspace(dir.path());

        convert(&service, &Options::default()).expect("a database service converts");

        let config = std::fs::read_to_string(service.src("config.rs")).expect("config.rs");
        assert!(
            config.contains("settings.database.validate()?;\n    settings.auth.validate()?;"),
            "{config}"
        );
        let test = std::fs::read_to_string(service.dir.join("tests/demo/oidc.rs")).expect("test");
        assert!(
            test.contains("build_router(crate::helpers::dead_pool(), "),
            "{test}"
        );
    }

    #[test]
    fn a_stateless_service_gets_the_stateless_router_call() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (service, _) = converted(dir.path(), &Options::default());
        let test = std::fs::read_to_string(service.dir.join("tests/demo/oidc.rs")).expect("test");
        assert!(!test.contains("dead_pool"), "{test}");
        assert!(!test.contains("@DB@"), "{test}");
        for sentinel in [
            "__name__",
            "__snake__",
            "__pascal__",
            "__screaming__",
            "__port__",
        ] {
            assert!(!test.contains(sentinel), "an unreplaced {sentinel}: {test}");
        }
    }

    #[test]
    fn a_second_service_reuses_the_crate_and_only_wires_itself() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (first, _) = converted(dir.path(), &Options::default());
        let lib = first.root.join("crates/oidc/src/lib.rs");
        // Marked so a rewrite is visible: the crate is a person's after the first run.
        std::fs::write(&lib, "// edited by the product\n").expect("edit the crate");

        // A second service, rendered next to the first.
        let sentinels = crate::scaffold::Sentinels::with_port("second", 5001);
        crate::api::fixtures::copy_rendered(
            &crate::repo::root().join("templates/service/services/__name__"),
            &dir.path().join("services/second"),
            &sentinels,
        );
        let second = Service {
            root: dir.path().to_path_buf(),
            dir: dir.path().join("services/second"),
            name: "second".to_owned(),
        };

        let outcome = convert(&second, &Options::default()).expect("the second service converts");

        assert!(!outcome.crate_written, "the crate was already there");
        assert_eq!(
            std::fs::read_to_string(&lib).expect("lib.rs"),
            "// edited by the product\n",
            "an existing crates/oidc must be left exactly as it is"
        );
        let manifest = std::fs::read_to_string(second.dir.join("Cargo.toml")).expect("manifest");
        assert!(manifest.contains("oidc ="), "{manifest}");
    }

    #[test]
    fn running_it_twice_is_refused_by_name_and_writes_nothing() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (service, _) = converted(dir.path(), &Options::default());
        let before = tree(dir.path());

        let error = convert(&service, &Options::default())
            .expect_err("a service that has it cannot take it again")
            .to_string();

        assert!(
            error.contains("already has authentication installed"),
            "{error}"
        );
        assert_eq!(before, tree(dir.path()), "a refusal must write nothing");
    }

    #[test]
    fn a_hand_wired_dependency_is_enough_to_be_refused() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        let manifest = service.dir.join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest).expect("manifest");
        std::fs::write(
            &manifest,
            text.replace(
                "[dependencies]\n",
                "[dependencies]\noidc = { path = \"../../crates/oidc\" }\n",
            ),
        )
        .expect("edit the manifest");
        let before = tree(dir.path());

        let error = convert(&service, &Options::default())
            .expect_err("a service that depends on oidc already has it")
            .to_string();

        assert!(error.contains("depends on `oidc`"), "{error}");
        assert_eq!(before, tree(dir.path()));
    }

    #[test]
    fn a_main_that_no_longer_matches_is_refused_without_writing_anything() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        let main = service.src("main.rs");
        let text = std::fs::read_to_string(&main).expect("main.rs");
        std::fs::write(
            &main,
            text.replace("identity::NoIdentities", "my_verifier::Custom"),
        )
        .expect("rewrite main.rs");
        let before = tree(dir.path());

        let error = format!(
            "{:#}",
            convert(&service, &Options::default())
                .expect_err("an anchor that is gone is a refusal")
        );

        assert!(
            error.contains("NoIdentities"),
            "the anchor should be quoted: {error}"
        );
        assert_eq!(
            before,
            tree(dir.path()),
            "a refusal must write nothing, crates/oidc included"
        );
    }

    #[test]
    fn a_settings_file_with_an_auth_table_of_its_own_is_refused_without_writing_anything() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        let settings = service.dir.join("config/demo.toml");
        let text = std::fs::read_to_string(&settings).expect("settings");
        std::fs::write(&settings, format!("{text}\n[auth]\nsomething = 1\n")).expect("edit");
        let before = tree(dir.path());

        let error = format!(
            "{:#}",
            convert(&service, &Options::default()).expect_err("a foreign [auth] is a refusal")
        );

        assert!(
            error.contains("`auth` table this command did not write"),
            "{error}"
        );
        assert_eq!(before, tree(dir.path()));
    }

    #[test]
    fn a_workspace_missing_a_shared_entry_is_refused_before_anything_is_written() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        let root = dir.path().join("Cargo.toml");
        let text = std::fs::read_to_string(&root).expect("Cargo.toml");
        std::fs::write(
            &root,
            text.lines()
                .filter(|line| !line.starts_with("thiserror ="))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("remove an entry");
        let before = tree(dir.path());

        let error = convert(&service, &Options::default())
            .expect_err("a missing workspace entry is a refusal")
            .to_string();

        assert!(error.contains("thiserror"), "{error}");
        assert_eq!(before, tree(dir.path()));
    }

    #[test]
    fn the_workspace_gains_the_entries_a_product_does_not_have() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (service, outcome) = converted(dir.path(), &Options::default());

        let root = std::fs::read_to_string(service.root.join("Cargo.toml")).expect("Cargo.toml");
        let parsed: toml::Value = toml::from_str(&root).expect("the result still parses");
        let dependencies = &parsed["workspace"]["dependencies"];
        // The one entry the command writes: the client crate does not use `jsonwebtoken`, so it
        // stays template-only and arrives only now.
        assert!(
            dependencies.get("jsonwebtoken").is_some(),
            "jsonwebtoken was not added: {root}"
        );
        // The shared HTTPS stack the client crate also uses survives `cli new`, so the command must
        // find it already there and add nothing for it.
        for name in [
            "rustls",
            "webpki-roots",
            "hyper-rustls",
            "hyper-util",
            "http-body-util",
            "url",
        ] {
            assert!(
                dependencies.get(name).is_some(),
                "{name} is missing from the shared section: {root}"
            );
        }
        assert!(
            root.contains("RUSTSEC-2023-0071"),
            "the reasons travel with the entries: {root}"
        );
        assert!(!root.contains("template-only"), "{root}");
        let opening = "# --- added by cargo xtask add-auth-oidc ---\n";
        let closing = "# --- end added by cargo xtask add-auth-oidc ---\n";
        assert_eq!(root.matches(opening).count(), 1, "{root}");
        assert_eq!(root.matches(closing).count(), 1, "{root}");
        let (before, marked) = root.split_once(opening).expect("opening ownership marker");
        let (owned, after) = marked
            .split_once(closing)
            .expect("closing ownership marker");
        let owned: toml::Table = toml::from_str(owned).expect("owned dependency block");
        let provided: toml::Table =
            toml::from_str(WORKSPACE_DEPENDENCIES).expect("embedded dependencies");
        assert_eq!(owned, provided, "only generated dependencies are owned");
        assert!(!before.contains("jsonwebtoken ="), "{before}");
        assert!(!after.contains("jsonwebtoken ="), "{after}");
        assert_eq!(
            strip_template_only(&root),
            root,
            "product ownership must survive template-only stripping"
        );
        let planned = plan_workspace(&service).expect("replanning workspace");
        assert!(
            planned.is_none(),
            "the marked dependencies must not be inserted twice"
        );
        assert!(
            outcome
                .amended
                .iter()
                .any(|path| path.ends_with("Cargo.toml")
                    && path.parent() == Some(service.root.as_path()))
        );
    }

    #[test]
    fn entries_a_product_already_has_are_left_as_it_chose_them() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        let root = dir.path().join("Cargo.toml");
        let text = std::fs::read_to_string(&root).expect("Cargo.toml");
        // Pin the one entry the command would otherwise write, so the command has nothing to add
        // and must leave the product's choice alone.
        std::fs::write(
            &root,
            text.replace(
                "\ntracing = \"0.1\"\n",
                "\ntracing = \"0.1\"\njsonwebtoken = \"=11.1.0\"\n",
            ),
        )
        .expect("pin one entry");

        convert(&service, &Options::default()).expect("converts");

        let after = std::fs::read_to_string(&root).expect("Cargo.toml");
        assert!(
            after.contains("jsonwebtoken = \"=11.1.0\""),
            "the product's pin must stand: {after}"
        );
        assert_eq!(after.matches("jsonwebtoken =").count(), 1, "{after}");
        assert!(
            !after.contains("RUSTSEC-2023-0071"),
            "nothing was written, so the explanation was not repeated: {after}"
        );
        assert!(
            !after.contains("added by cargo xtask add-auth-oidc"),
            "existing product dependencies must not acquire generated ownership: {after}"
        );
    }

    #[test]
    fn an_existing_crate_leaves_the_root_manifest_alone() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (first, _) = converted(dir.path(), &Options::default());
        let sentinels = crate::scaffold::Sentinels::with_port("second", 5001);
        crate::api::fixtures::copy_rendered(
            &crate::repo::root().join("templates/service/services/__name__"),
            &dir.path().join("services/second"),
            &sentinels,
        );
        let second = Service {
            root: dir.path().to_path_buf(),
            dir: dir.path().join("services/second"),
            name: "second".to_owned(),
        };
        let before = std::fs::read_to_string(first.root.join("Cargo.toml")).expect("Cargo.toml");

        convert(&second, &Options::default()).expect("converts");

        assert_eq!(
            before,
            std::fs::read_to_string(first.root.join("Cargo.toml")).expect("Cargo.toml")
        );
    }

    /// The block in the template's `Cargo.toml` and the copy this command writes are one text.
    #[test]
    fn the_workspace_entries_are_the_ones_the_command_writes() {
        let manifest = crate::repo::root().join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest).expect("Cargo.toml");
        if !text.contains("# --- template-only") {
            return; // a product: the command is the only copy
        }
        assert!(
            text.contains(WORKSPACE_DEPENDENCIES),
            "the template-only block in Cargo.toml is not tools/xtask/fixtures/oidc/workspace-dependencies.toml. \
             Edit one, then copy it into the other: the block is what `cli new` strips and this command \
             writes back"
        );
    }

    #[test]
    fn a_workspace_that_does_not_build_crates_is_refused() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        let root = dir.path().join("Cargo.toml");
        let text = std::fs::read_to_string(&root).expect("Cargo.toml");
        std::fs::write(&root, text.replace("    \"crates/*\",\n", "")).expect("drop the glob");

        let error = convert(&service, &Options::default())
            .expect_err("no crates/* glob is a refusal")
            .to_string();

        assert!(error.contains("crates/*"), "{error}");
    }

    #[test]
    fn a_service_missing_its_tests_is_refused_naming_the_file() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        std::fs::remove_file(service.dir.join("tests/demo/main.rs")).expect("remove");
        let before = tree(dir.path());

        let error = format!(
            "{:#}",
            convert(&service, &Options::default()).expect_err("a missing file is a refusal")
        );

        assert!(error.contains("tests/demo/main.rs"), "{error}");
        assert_eq!(before, tree(dir.path()));
    }

    #[test]
    fn the_issuer_flag_is_held_to_the_rule_the_service_applies_at_startup() {
        for issuer in [
            "https://id.example.com",
            "https://id.example.com/realms/main",
            "http://127.0.0.1:8080",
            "http://localhost:9000",
            "http://[::1]:9000",
        ] {
            check_issuer(issuer).unwrap_or_else(|error| panic!("{issuer}: {error}"));
        }
        for issuer in [
            "id.example.com",
            "ftp://id.example.com",
            "http://id.example.com",
            "http://10.0.0.5",
            "http://localhost.example.com",
            "https://user:secret@id.example.com",
            "https://id.example.com/?x=1",
            "https://",
        ] {
            assert!(check_issuer(issuer).is_err(), "{issuer} must be refused");
        }
    }

    #[test]
    fn a_service_with_no_publish_project_asks_for_an_audience() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        std::fs::write(
            dir.path().join("versions.toml"),
            "[publish]\nregistry = \"x\"\n",
        )
        .expect("rewrite versions.toml");

        let error = format!(
            "{:#}",
            convert(&service, &Options::default()).expect_err("no project")
        );

        assert!(error.contains("--audience"), "{error}");
    }

    /// `crates/oidc` in the template and the copy this command writes are one crate.
    ///
    /// The template has the crate for real so that `ci-test` compiles and runs it, and a product
    /// has only the embedded copy, so there are two files for every file and nothing but this test
    /// stops them drifting. Skipped where the crate is absent, which is every product: the copy is
    /// then the only one there is.
    #[test]
    fn the_embedded_crate_is_the_crate() {
        let crate_dir = crate::repo::root().join("crates/oidc");
        if !crate_dir.join("Cargo.toml").is_file() {
            return;
        }
        for (path, embedded) in OIDC_FILES {
            let real = std::fs::read_to_string(crate_dir.join(path))
                .unwrap_or_else(|error| panic!("crates/oidc/{path}: {error}"));
            assert!(
                real == *embedded,
                "tools/xtask/fixtures/oidc/{} is not crates/oidc/{path}. The copy is what \
                 `add-auth-oidc` writes into a product; refresh it with\n\n    \
                 cp -r crates/oidc/src tools/xtask/fixtures/oidc/ && \
                 cp crates/oidc/Cargo.toml tools/xtask/fixtures/oidc/Cargo.toml.in",
                if *path == "Cargo.toml" {
                    "Cargo.toml.in"
                } else {
                    path
                }
            );
        }
        // And the other direction: a file added to the crate but not to the table is a file a
        // product never gets, which compiles in the template and not in the product.
        let mut on_disk = Vec::new();
        let mut walk = vec![crate_dir.clone()];
        while let Some(dir) = walk.pop() {
            for entry in std::fs::read_dir(&dir).expect("read crates/oidc") {
                let path = entry.expect("an entry").path();
                if path.is_dir() {
                    walk.push(path);
                } else {
                    on_disk.push(
                        path.strip_prefix(&crate_dir)
                            .expect("under the crate")
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        on_disk.sort();
        let mut embedded: Vec<&str> = OIDC_FILES.iter().map(|(path, _)| *path).collect();
        embedded.sort_unstable();
        assert_eq!(
            on_disk.iter().map(String::as_str).collect::<Vec<_>>(),
            embedded,
            "crates/oidc and OIDC_FILES list different files"
        );
    }

    #[test]
    fn the_service_test_asks_for_nothing_the_service_does_not_have() {
        // Every sentinel and marker is replaced, for a service whose contract is named
        // after it (the `just add service` shape).
        let dir = tempfile::tempdir().expect("a temporary directory");
        if skip_without_template() {
            return;
        }
        let service = scaffolded_service(dir.path());
        let contract = contract(&service).expect("the scaffold has one Whoami");
        let text = test_file(&service, &contract, false).expect("render service test");
        assert!(
            !text.contains("__name__")
                && !text.contains("__snake__")
                && !text.contains("__pascal__")
                && !text.contains("@PKG@")
                && !text.contains("@CLIENT@")
                && !text.contains("@PATH@")
                && !text.contains("@DB@")
        );
        assert!(text.contains("demo::startup::build_router"), "{text}");
        assert!(text.contains("buffa::demo::v1::WhoamiRequest"), "{text}");
        assert!(
            text.contains("connect::demo::v1::DemoServiceClient"),
            "{text}"
        );
        assert!(text.contains("demo.v1.DemoService/Whoami"), "{text}");
    }

    /// A service whose contract is **not** named after it: `services/api` serves
    /// `counter.v1.CounterService`, and the test must read that off `PROTECTED`, not
    /// guess it from the crate name `api` (which would name `buffa::api::v1`, and not
    /// compile).
    fn counter_shaped_service(root: &std::path::Path) -> Service {
        let service = scaffolded_service(root);
        give_the_workspace(root);
        let auth = service.auth_test();
        let text = std::fs::read_to_string(&auth).expect("auth.rs");
        std::fs::write(
            &auth,
            text.replace(
                "demo.v1.DemoService/Whoami",
                "counter.v1.CounterService/Whoami",
            ),
        )
        .expect("rewrite auth.rs");
        service
    }

    #[test]
    fn the_generated_test_names_the_contract_the_service_declares_not_its_name() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = counter_shaped_service(dir.path());

        convert(&service, &Options::default()).expect("a counter-shaped service converts");

        let test =
            std::fs::read_to_string(service.dir.join("tests/demo/oidc.rs")).expect("oidc.rs");
        assert!(test.contains("buffa::counter::v1::WhoamiRequest"), "{test}");
        assert!(
            test.contains("connect::counter::v1::CounterServiceClient"),
            "{test}"
        );
        assert!(test.contains("counter.v1.CounterService/Whoami"), "{test}");
        assert!(
            !test.contains("buffa::demo::") && !test.contains("connect::demo::"),
            "the contract must come from PROTECTED, not from the crate name `demo`: {test}"
        );
    }

    #[test]
    fn a_service_whose_protected_list_has_no_whoami_is_refused_before_writing() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        let auth = service.auth_test();
        let text = std::fs::read_to_string(&auth).expect("auth.rs");
        std::fs::write(
            &auth,
            text.replace("demo.v1.DemoService/Whoami", "demo.v1.DemoService/Ping"),
        )
        .expect("rewrite auth.rs");
        let before = tree(dir.path());

        let error = format!(
            "{:#}",
            convert(&service, &Options::default()).expect_err("no Whoami is a refusal")
        );

        assert!(error.contains("Whoami"), "{error}");
        assert!(error.contains("tests/demo/auth.rs"), "{error}");
        assert_eq!(before, tree(dir.path()), "a refusal must write nothing");
    }

    #[test]
    fn a_service_with_two_whoami_entries_is_refused_before_writing() {
        if skip_without_template() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        give_the_workspace(dir.path());
        let auth = service.auth_test();
        let text = std::fs::read_to_string(&auth).expect("auth.rs");
        std::fs::write(
            &auth,
            text.replace(
                "demo.v1.DemoService/Whoami",
                "demo.v1.DemoService/Whoami\", \"counter.v1.CounterService/Whoami",
            ),
        )
        .expect("rewrite auth.rs");
        let before = tree(dir.path());

        let error = format!(
            "{:#}",
            convert(&service, &Options::default()).expect_err("two Whoamis is a refusal")
        );

        assert!(error.contains("Whoami"), "{error}");
        assert!(error.contains("tests/demo/auth.rs"), "{error}");
        assert_eq!(before, tree(dir.path()), "a refusal must write nothing");
    }
}
