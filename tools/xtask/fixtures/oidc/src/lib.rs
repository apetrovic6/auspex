//! Verifies an OIDC provider's access tokens, for `crates/identity`'s seam.
//!
//! This is the one place in the repository that knows a provider's name for a thing — a token's
//! format, where its keys live, what `iss` and `aud` mean — which is exactly what
//! `crates/identity` refuses to hold (`docs/adr/0012`). A service reaches it through
//! `cargo xtask add-auth-oidc <service>` and nothing else: the template ships the seam with a
//! verifier that authenticates nobody, and this crate is what a product swaps in
//! (`docs/adr/0027`).
//!
//! # What a token has to be
//!
//! A JWT signed with one of the configured algorithms &mdash; **RS256** is the default, and a product
//! may add PS256, ES256, `EdDSA` and the other public-key algorithms. The allow-list is fixed: no
//! `none` and no HMAC (HS*) are ever accepted (an HMAC key would be the public JWK, the classic
//! key-confusion attack). A token must name a `kid` whose key the provider publishes and whose
//! key type fits the algorithm, and whose claims carry an `exp` that has not passed, an `nbf`
//! that has (when present), an `iss` equal to the configured issuer, an `aud` containing the
//! configured audience, and a non-empty `sub`. The subject becomes the [`Principal`]. Groups,
//! roles and e-mail are not read: a provider that does not issue them (Pocket ID's access tokens
//! carry none) would make a verifier that required them a verifier that accepts nobody.
//!
//! # The two kinds of "no"
//!
//! `docs/adr/0012` makes a verifier say which one it means:
//!
//! - [`VerifyFailure::Rejected`] — the token is not valid. Malformed, foreign, expired, wrong
//!   audience, signed by a key the provider does not publish. The caller sees `unauthenticated`.
//! - [`VerifyFailure::Unavailable`] — **we could not check**: the provider is unreachable and the
//!   cache holds no key that would settle it. The caller sees `internal`, because a good
//!   credential refused as bad is the failure the split exists to prevent.
//!
//! Neither carries the token. The reasons are fixed strings chosen here, never text taken from the
//! token or from the network.

#[cfg(feature = "bff")]
pub mod bff;
mod http;
mod keys;
#[cfg(feature = "testing")]
pub mod testing;
#[cfg(test)]
mod tests;

use std::net::IpAddr;
use std::time::Duration;

use identity::{BoxFuture, Credential, Principal, VerifyCredential, VerifyFailure};
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{Algorithm, Validation, decode, decode_header};
use serde::Deserialize;

use crate::keys::{KeyError, KeySource};

/// The algorithms a verifier is willing to accept.
///
/// Defaults to `[RS256]`. HS* and `none` are never in this set: an HMAC key would be the public
/// JWK, the classic key-confusion attack.
#[derive(Debug, Clone)]
pub struct AlgorithmSet {
    /// At least one algorithm. The first one is what [`Validation`] uses as its base; the rest
    /// are accepted from a token's header.
    algorithms: Vec<Algorithm>,
}

/// The public-key algorithms this crate will allow a product to configure.
///
/// `none` and every HS* variant are absent on purpose: an HMAC key would be the public JWK, which
/// is a classic confusion attack. That was always the reason, and the list is exactly what was
/// hard-coded before multi-algorithm support.
pub const ALLOWED_ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

impl AlgorithmSet {
    /// RS256 only — the default when nothing is configured.
    #[must_use]
    pub fn default_set() -> Self {
        Self {
            algorithms: vec![Algorithm::RS256],
        }
    }

    /// Build a set from a list of algorithm names as they appear in TOML, e.g. `"RS256"`.
    ///
    /// # Errors
    ///
    /// Returns an error when the list is empty, when an entry is not a recognized public-key
    /// algorithm, or when the list has a duplicate, with a message that names the problem.
    pub fn from_strs(names: &[String]) -> Result<Self, ConfigError> {
        if names.is_empty() {
            return Err(ConfigError::EmptyAlgorithms);
        }
        let mut seen = Vec::with_capacity(names.len());
        for name in names {
            let algorithm: Algorithm = name
                .parse()
                .map_err(|_| ConfigError::UnknownAlgorithm(name.clone()))?;
            if !ALLOWED_ALGORITHMS.contains(&algorithm) {
                return Err(ConfigError::UnknownAlgorithm(name.clone()));
            }
            if seen.contains(&algorithm) {
                return Err(ConfigError::DuplicateAlgorithm(name.clone()));
            }
            seen.push(algorithm);
        }
        Ok(Self { algorithms: seen })
    }

    /// The first algorithm in the set, for the `Validation` baseline.
    #[must_use]
    pub fn first(&self) -> Algorithm {
        self.algorithms[0]
    }

    /// All algorithms as a slice, for passing to `jsonwebtoken::Validation`.
    #[must_use]
    pub fn as_slice(&self) -> &[Algorithm] {
        &self.algorithms
    }

    /// Whether `algorithm` is in the configured set.
    #[must_use]
    pub fn contains(&self, algorithm: Algorithm) -> bool {
        self.algorithms.contains(&algorithm)
    }
}

/// What the verifier is configured with: who issues the tokens, and who they must be for.
#[derive(Debug, Clone)]
pub struct Config {
    /// The issuer's identifier, as it appears in `iss` and as the discovery document names
    /// itself — `https://id.example.com`, compared as an exact string.
    pub issuer: String,
    /// The audience a token must carry in `aud`. For a provider that issues access tokens with
    /// `aud` set to the client, this is that client's id.
    pub audience: String,
    /// The signing algorithms this verifier accepts. Defaults to `[RS256]`.
    #[allow(
        clippy::struct_field_names,
        reason = "the field is the same concept as the type"
    )]
    pub algorithms: AlgorithmSet,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            audience: String::new(),
            algorithms: AlgorithmSet::default_set(),
        }
    }
}

/// Why a [`Config`] cannot be used. Every message names what is wrong with the field, so a
/// service can print it as-is under its own section name.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("the issuer is empty")]
    EmptyIssuer,
    #[error("the audience is empty")]
    EmptyAudience,
    #[error("the issuer is not a usable URL: {0}")]
    BadIssuer(&'static str),
    /// A plain `http://` issuer anywhere but loopback.
    ///
    /// Keys fetched over cleartext are keys anyone on the path can replace, and a replaced key set
    /// is a verifier that accepts whatever its replacer signs.
    #[error(
        "the issuer must be https:// (plain http:// is accepted only for localhost, \
         127.0.0.0/8 and ::1)"
    )]
    InsecureIssuer,
    #[error("could not build the HTTPS client: {0}")]
    Client(String),
    #[error("auth.algorithms must not be empty")]
    EmptyAlgorithms,
    #[error("auth.algorithms: {0:?} is not a recognised signing algorithm")]
    UnknownAlgorithm(String),
    #[error("auth.algorithms: {0:?} appears more than once")]
    DuplicateAlgorithm(String),
}

/// The intervals the key cache works to. [`Timing::default`] is what a service runs with; the
/// fields are public so a test can shrink them.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long a fetched key set is trusted without asking again. Past it, the next verification
    /// that needs a key refreshes first — and keeps the old set if that fails.
    pub refresh_interval: Duration,
    /// The shortest gap between two fetches, whatever asked for them. This is the rate limit on
    /// refetching for an unknown `kid`, and it is also what stops a cold start against a dead
    /// provider from retrying on every request.
    pub min_refetch_interval: Duration,
    /// Per HTTP request. A cold verification makes two (discovery, then keys), and the service's
    /// own deadline (`timeouts::REQUEST`, 5 s) has to outlast both: keep `2 × request_timeout`
    /// below it, or a provider that accepts and never answers is reported as an empty 408
    /// instead of the `internal` this crate means to produce.
    pub request_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            refresh_interval: Duration::from_secs(15 * 60),
            min_refetch_interval: Duration::from_secs(30),
            request_timeout: Duration::from_millis(1500),
        }
    }
}

/// The OIDC verifier. Build it once and share it; it holds the key cache.
pub struct Verifier {
    keys: KeySource,
    /// The parts of validation that do not depend on the token's algorithm. The issuer, audience
    /// and required-claims list are the same for every request; the algorithm is chosen per-request
    /// because `jsonwebtoken::Validation` requires all algorithms in its list to share a family
    /// (RSA, EC, or Ed), and a configured set may span two or three.
    issuer: String,
    audience: String,
    algorithms: AlgorithmSet,
}

/// Seconds of clock skew tolerated on `exp` and `nbf`. Small on purpose: a replica's clock is
/// disciplined by NTP, and every second here is a second an expired token still works.
const LEEWAY_SECONDS: u64 = 30;

/// The claims this verifier reads. `iss`, `aud`, `exp` and `nbf` are checked by `jsonwebtoken`
/// and need no field; only the subject is used afterwards.
#[derive(Deserialize)]
struct Claims {
    sub: String,
}

impl Verifier {
    /// Build a verifier. Fetches nothing: the first token is what causes the first request.
    ///
    /// # Errors
    ///
    /// Returns an error if the [`Config`] is unusable ([`Config::validate`]) or the HTTPS client
    /// cannot be built.
    pub fn new(config: &Config) -> Result<Self, ConfigError> {
        Self::with_timing(config, Timing::default())
    }

    /// [`Verifier::new`] with explicit [`Timing`].
    ///
    /// # Errors
    ///
    /// Same as [`Verifier::new`].
    pub fn with_timing(config: &Config, timing: Timing) -> Result<Self, ConfigError> {
        Self::build(config, timing, http::webpki_roots())
    }

    /// [`Verifier::with_timing`] trusting `extra` roots as well as the compiled-in ones.
    ///
    /// For the one test that proves the client uses the root store it was given. Not part of the
    /// crate outside `testing`: a deployment that needs a private CA is a decision to make in a
    /// configuration option with a name, not a constructor argument.
    ///
    /// # Errors
    ///
    /// Same as [`Verifier::new`], and if a certificate cannot be added.
    #[cfg(feature = "testing")]
    pub fn with_extra_roots(
        config: &Config,
        timing: Timing,
        extra: Vec<rustls::pki_types::CertificateDer<'static>>,
    ) -> Result<Self, ConfigError> {
        let mut roots = http::webpki_roots();
        for certificate in extra {
            roots
                .add(certificate)
                .map_err(|error| ConfigError::Client(error.to_string()))?;
        }
        Self::build(config, timing, roots)
    }

    fn build(
        config: &Config,
        timing: Timing,
        roots: rustls::RootCertStore,
    ) -> Result<Self, ConfigError> {
        config.validate()?;

        let tls =
            http::tls_config(roots).map_err(|error| ConfigError::Client(error.to_string()))?;
        let client = http::Client::new(tls, timing.request_timeout);

        let discovery = format!(
            "{}/.well-known/openid-configuration",
            config.issuer.trim_end_matches('/')
        );
        let discovery_url = url::Url::parse(&discovery)
            .map_err(|_| ConfigError::BadIssuer("it does not form a discovery URL"))?;

        Ok(Self {
            keys: KeySource::new(
                client,
                config.issuer.clone(),
                discovery_url,
                timing,
                config.algorithms.as_slice(),
            ),
            issuer: config.issuer.clone(),
            audience: config.audience.clone(),
            algorithms: config.algorithms.clone(),
        })
    }

    /// Build a [`Validation`] for `algorithm`, with the issuer, audience and required claims
    /// from this verifier's configuration.
    fn validation_for(&self, algorithm: Algorithm) -> Validation {
        let mut validation = Validation::new(algorithm);
        validation.leeway = LEEWAY_SECONDS;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.validate_aud = true;
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation
    }

    async fn verify_token(&self, token: &str) -> Result<Principal, VerifyFailure> {
        // The header alone, before any network: a token with the wrong algorithm is refused
        // without waking the provider, which is also why `alg: none` and HS256 (the key-confusion
        // attack, signing with the public key as the HMAC secret) cost nothing to answer.
        let header = decode_header(token).map_err(|_| rejected("not a well-formed JWT"))?;
        if !self.algorithms.contains(header.alg) {
            return Err(rejected(
                "the token's algorithm is not in the configured set",
            ));
        }
        // RFC 7515 §4.1.11: `crit` lists extensions the recipient MUST understand or reject the
        // token, and this verifier understands none.
        if header.crit.is_some() {
            return Err(rejected(
                "the token requires header extensions this verifier does not understand",
            ));
        }
        let kid = header
            .kid
            .ok_or_else(|| rejected("the token names no signing key"))?;

        let key = self
            .keys
            .key(&kid, header.alg)
            .await
            .map_err(|error| match error {
                KeyError::Unknown => rejected("the provider publishes no key with that id"),
                KeyError::Unavailable(reason) => VerifyFailure::Unavailable(reason.into()),
            })?;

        // `jsonwebtoken::Validation` requires all algorithms in its list to share a family
        // (RSA, EC, or Ed). A configured set may span families, so the validation is built
        // per-request for the token's specific algorithm.
        let validation = self.validation_for(header.alg);

        let data = decode::<Claims>(token, &key, &validation)
            .map_err(|error| rejected(describe(error.kind())))?;

        if data.claims.sub.trim().is_empty() {
            return Err(rejected("the token's subject is empty"));
        }
        Ok(Principal {
            subject: data.claims.sub,
        })
    }
}

impl VerifyCredential for Verifier {
    fn verify(&self, credential: Credential) -> BoxFuture<'_, Result<Principal, VerifyFailure>> {
        Box::pin(async move { self.verify_token(credential.token()).await })
    }
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Verifier").finish_non_exhaustive()
    }
}

fn rejected(reason: &'static str) -> VerifyFailure {
    VerifyFailure::Rejected(reason.into())
}

/// A fixed sentence for a validation failure.
///
/// `jsonwebtoken`'s own messages are not used because some of them quote the claim they
/// choked on (a `serde` "invalid type: string \"…\"" carries the value), and a claim value is
/// part of the credential. The kinds are matched, the text is ours.
const fn describe(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::ExpiredSignature => "the token has expired",
        ErrorKind::ImmatureSignature => "the token is not valid yet",
        ErrorKind::InvalidAudience => "the token is for a different audience",
        ErrorKind::InvalidIssuer => "the token was issued by a different issuer",
        ErrorKind::InvalidSignature => "the token's signature does not verify",
        ErrorKind::InvalidAlgorithm => "the token's algorithm is not allowed",
        ErrorKind::MissingRequiredClaim(_) => "the token lacks a required claim",
        _ => "the token is not valid",
    }
}

impl Config {
    /// Check the configuration without touching the network.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] for an empty issuer or audience, an issuer that is not an
    /// absolute `https://` URL (or `http://` on loopback), or one carrying credentials, a query or
    /// a fragment — none of which an issuer identifier may have. Also checks the algorithm set
    /// is valid.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.issuer.trim().is_empty() {
            return Err(ConfigError::EmptyIssuer);
        }
        if self.audience.trim().is_empty() {
            return Err(ConfigError::EmptyAudience);
        }
        if self.algorithms.algorithms.is_empty() {
            return Err(ConfigError::EmptyAlgorithms);
        }
        let url = url::Url::parse(&self.issuer)
            .map_err(|_| ConfigError::BadIssuer("it does not parse as an absolute URL"))?;
        check_endpoint(&url)?;
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ConfigError::BadIssuer("it carries a query or a fragment"));
        }
        Ok(())
    }
}

/// The rule for every URL this crate will fetch: `https`, or `http` to the machine itself.
///
/// Applied to the issuer and, again, to the `jwks_uri` the discovery document names, so a
/// document served over TLS cannot point key fetching at cleartext.
pub(crate) fn check_endpoint(url: &url::Url) -> Result<(), ConfigError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::BadIssuer("it carries credentials"));
    }
    let Some(host) = url.host_str() else {
        return Err(ConfigError::BadIssuer("it has no host"));
    };
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            // `host_str` keeps the brackets on an IPv6 literal, which `IpAddr` does not parse.
            let bare = host.trim_start_matches('[').trim_end_matches(']');
            let loopback = bare == "localhost"
                || bare
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback());
            if loopback {
                Ok(())
            } else {
                Err(ConfigError::InsecureIssuer)
            }
        }
        _ => Err(ConfigError::BadIssuer(
            "its scheme is neither https nor http",
        )),
    }
}
