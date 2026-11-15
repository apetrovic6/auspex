//! Test support: an identity provider that lives in the test process, and the keys it signs with.
//!
//! Behind the `testing` feature, which a crate enables from its `[dev-dependencies]` and from
//! nowhere else. Used by this crate's own tests and by the service tests `cargo xtask
//! add-auth-oidc` writes, so that the happy path of a protected method is driven through the real
//! router, the real verifier and a real HTTP fetch — without a provider, a network, or a key
//! that means anything.
//!
//! # The keys
//!
//! Everything under `src/testing/*.pem` is **test-only and public**: it is committed, it is in
//! this repository's history, and it protects nothing. A token signed with it is accepted by a
//! verifier pointed at a [`TestIdp`] and by nothing else, because no real issuer publishes the
//! matching public half. They were generated once and are not rotated:
//!
//! ```text
//! openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out test-only-signing-key-a.pem
//! openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out test-only-signing-key-b.pem
//! openssl rsa -in test-only-signing-key-a.pem -pubout -out test-only-signing-key-a.pub.pem
//! openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out test-only-ec-key.pem
//! openssl genpkey -algorithm ED25519 -out test-only-ed25519-key.pem
//!
//! # the TLS pair: a CA, and a leaf for 127.0.0.1 it signed (36500 days, so it cannot expire
//! # under a test)
//! openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out test-only-tls-ca.pem \
//!     -days 36500 -subj '/CN=oidc test-only CA' \
//!     -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign'
//! openssl req -newkey rsa:2048 -nodes -keyout leaf.key -out leaf.csr -subj '/CN=127.0.0.1'
//! # ext.cnf: subjectAltName=IP:127.0.0.1, basicConstraints=CA:FALSE, extendedKeyUsage=serverAuth,
//! #          keyUsage=digitalSignature,keyEncipherment
//! openssl x509 -req -in leaf.csr -CA test-only-tls-ca.pem -CAkey ca.key -CAcreateserial \
//!     -out test-only-tls-leaf.pem -days 36500 -extfile ext.cnf
//! openssl pkcs8 -topk8 -nocrypt -in leaf.key -out test-only-tls-leaf-key.pem
//! ```
//!
//! Two RSA signing keys, because "the provider rotated" and "the provider re-published a `kid`
//! with different key material" are different attacks on a cache and each needs a second key to
//! stage. An EC P-256 key and an Ed25519 key for the algorithm-family tests.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use jsonwebtoken::jwk::{Jwk, PublicKeyUse};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// The audience the helpers put in a token and the tests configure a verifier with.
pub const AUDIENCE: &str = "test-audience";

/// The subject [`claims`] names.
pub const SUBJECT: &str = "test-subject";

/// One of the committed signing keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigningKey {
    A,
    B,
    /// An EC P-256 key for ES256 tests.
    Ec,
    /// An Ed25519 key for `EdDSA` tests.
    Ed25519,
}

impl SigningKey {
    const fn private_pem(self) -> &'static str {
        match self {
            Self::A => include_str!("test-only-signing-key-a.pem"),
            Self::B => include_str!("test-only-signing-key-b.pem"),
            Self::Ec => include_str!("test-only-ec-key.pem"),
            Self::Ed25519 => include_str!("test-only-ed25519-key.pem"),
        }
    }

    /// The public half of [`SigningKey::A`], as PEM — what an algorithm-confusion attack uses as
    /// an HMAC secret.
    #[must_use]
    pub const fn a_public_pem() -> &'static str {
        include_str!("test-only-signing-key-a.pub.pem")
    }

    fn encoding_key(self) -> EncodingKey {
        match self {
            Self::A | Self::B => EncodingKey::from_rsa_pem(self.private_pem().as_bytes())
                .expect("the committed test key parses"),
            Self::Ec => EncodingKey::from_ec_pem(self.private_pem().as_bytes())
                .expect("the committed EC key parses"),
            Self::Ed25519 => EncodingKey::from_ed_pem(self.private_pem().as_bytes())
                .expect("the committed Ed25519 key parses"),
        }
    }

    /// The default algorithm for this key.
    #[must_use]
    pub const fn default_algorithm(self) -> Algorithm {
        match self {
            Self::A | Self::B => Algorithm::RS256,
            Self::Ec => Algorithm::ES256,
            Self::Ed25519 => Algorithm::EdDSA,
        }
    }

    /// The public JWK for this key, published under `kid`, as a provider would serve it.
    ///
    /// Public so a test can alter it (a `use` of `enc`, an `alg` of `RS512`) and hand the result to
    /// [`TestIdp::publish_raw`].
    ///
    /// # Panics
    ///
    /// Panics if the committed key does not parse, which it does.
    #[must_use]
    pub fn jwk(self, kid: &str) -> Value {
        let mut jwk = Jwk::from_encoding_key(&self.encoding_key(), self.default_algorithm())
            .expect("a key has public components");
        jwk.common.key_id = Some(kid.to_owned());
        jwk.common.public_key_use = Some(PublicKeyUse::Signature);
        serde_json::to_value(jwk).expect("a JWK serializes")
    }

    /// The public JWK for this key, published for a specific `algorithm`.
    ///
    /// # Panics
    ///
    /// Panics if the committed key material does not parse, which it does.
    #[must_use]
    pub fn jwk_for(self, kid: &str, algorithm: Algorithm) -> Value {
        let mut jwk = Jwk::from_encoding_key(&self.encoding_key(), algorithm)
            .expect("a key has public components");
        jwk.common.key_id = Some(kid.to_owned());
        jwk.common.public_key_use = Some(PublicKeyUse::Signature);
        serde_json::to_value(jwk).expect("a JWK serializes")
    }

    /// A JWK whose `crv` field is `fake_curve` regardless of the actual key type, for
    /// testing curve-mismatch rejection. Only makes sense for EC keys.
    /// The JWK carries no `alg` so the key-type check is the only gate.
    ///
    /// # Panics
    ///
    /// Panics if the committed key material does not parse.
    #[must_use]
    pub fn jwk_with_fake_curve(self, kid: &str, algorithm: Algorithm, fake_curve: &str) -> Value {
        let mut value = self.jwk_for(kid, algorithm);
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "crv".to_owned(),
                serde_json::Value::String(fake_curve.to_owned()),
            );
            obj.remove("alg");
        }
        value
    }
}

/// Seconds since the epoch.
///
/// # Panics
///
/// Panics if the clock reads before 1970.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs()
}

/// Claims a verifier configured with `issuer` and [`AUDIENCE`] accepts: five minutes of life,
/// [`SUBJECT`] as the subject.
#[must_use]
pub fn claims(issuer: &str) -> Value {
    json!({
        "iss": issuer,
        "aud": AUDIENCE,
        "sub": SUBJECT,
        "exp": now() + 300,
        "iat": now(),
    })
}

/// Sign `claims` as RS256 with `key`, naming `kid` in the header.
///
/// # Panics
///
/// Panics if signing fails, which a committed key does not.
#[must_use]
pub fn sign(key: SigningKey, kid: &str, claims: &Value) -> String {
    sign_with(key, key.default_algorithm(), kid, claims)
}

/// Sign `claims` with `algorithm` using `key`, naming `kid` in the header.
///
/// # Panics
///
/// Panics if signing fails, which a committed key does not.
#[must_use]
pub fn sign_with(key: SigningKey, algorithm: Algorithm, kid: &str, claims: &Value) -> String {
    let mut header = Header::new(algorithm);
    header.kid = Some(kid.to_owned());
    encode(&header, claims, &key.encoding_key()).expect("signing with a committed key works")
}

/// What the provider answers with, and how often it was asked.
struct Provider {
    issuer: String,
    published: Mutex<Vec<(String, SigningKey)>>,
    /// What the discovery document claims as `issuer`, when it is not the real one.
    claimed_issuer: Mutex<Option<String>>,
    /// Where the discovery document sends key fetching, when it is not this provider's own.
    claimed_jwks_uri: Mutex<Option<String>>,
    /// Key set served verbatim instead of the published keys, when a test needs a JWK it could not
    /// get from a committed key (a different `use`, a different `alg`, two under one `kid`).
    raw_keys: Mutex<Option<Vec<Value>>>,
    /// How long the next discovery request is held before it is answered, once.
    slow_next_discovery: Mutex<Option<std::time::Duration>>,
    discovery_hits: AtomicUsize,
    jwks_hits: AtomicUsize,
    /// Authorization codes currently outstanding.
    auth_codes: Mutex<std::collections::HashMap<String, AuthCode>>,
    /// Refresh tokens that are valid: token → (`access_token`, `user_name`).
    refresh_tokens: Mutex<std::collections::HashMap<String, (String, String)>>,
    /// Refresh tokens that have been revoked.
    revoked_tokens: Mutex<std::collections::HashSet<String>>,
    /// Counter for generating unique tokens and codes.
    token_counter: AtomicUsize,
    token_hits: AtomicUsize,
    revoke_hits: AtomicUsize,
    token_failure: Mutex<bool>,
    revoke_failure: Mutex<bool>,
}

/// An authorization code stored by the fake provider.
#[allow(dead_code)]
struct AuthCode {
    redirect_uri: String,
    state: String,
    pkce_verifier: Option<String>,
}

impl Provider {
    fn new(issuer: String, published: &[(&str, SigningKey)]) -> Arc<Self> {
        Arc::new(Self {
            issuer,
            published: Mutex::new(
                published
                    .iter()
                    .map(|(kid, key)| ((*kid).to_owned(), *key))
                    .collect(),
            ),
            claimed_issuer: Mutex::new(None),
            claimed_jwks_uri: Mutex::new(None),
            raw_keys: Mutex::new(None),
            slow_next_discovery: Mutex::new(None),
            discovery_hits: AtomicUsize::new(0),
            jwks_hits: AtomicUsize::new(0),
            auth_codes: Mutex::new(std::collections::HashMap::new()),
            refresh_tokens: Mutex::new(std::collections::HashMap::new()),
            revoked_tokens: Mutex::new(std::collections::HashSet::new()),
            token_counter: AtomicUsize::new(0),
            token_hits: AtomicUsize::new(0),
            revoke_hits: AtomicUsize::new(0),
            token_failure: Mutex::new(false),
            revoke_failure: Mutex::new(false),
        })
    }

    fn discovery(&self) -> Value {
        self.discovery_hits.fetch_add(1, Ordering::SeqCst);
        let claimed = self
            .claimed_issuer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let jwks_uri = self
            .claimed_jwks_uri
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| format!("{}/jwks", self.issuer));
        json!({
            "issuer": claimed.unwrap_or_else(|| self.issuer.clone()),
            "jwks_uri": jwks_uri,
            "authorization_endpoint": format!("{}/authorize", self.issuer),
            "token_endpoint": format!("{}/token", self.issuer),
            "revocation_endpoint": format!("{}/revoke", self.issuer),
            "userinfo_endpoint": format!("{}/userinfo", self.issuer),
        })
    }

    fn take_discovery_delay(&self) -> Option<std::time::Duration> {
        self.slow_next_discovery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn jwks(&self) -> Value {
        self.jwks_hits.fetch_add(1, Ordering::SeqCst);
        let raw = self
            .raw_keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(raw) = raw {
            return json!({ "keys": raw });
        }
        let keys: Vec<Value> = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(kid, key)| key.jwk(kid))
            .collect();
        json!({ "keys": keys })
    }

    /// Store an authorization code that can be exchanged.
    fn add_auth_code(&self, code: &str, redirect_uri: &str, state: &str, verifier: Option<&str>) {
        self.auth_codes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                code.to_owned(),
                AuthCode {
                    redirect_uri: redirect_uri.to_owned(),
                    state: state.to_owned(),
                    pkce_verifier: verifier.map(str::to_owned),
                },
            );
    }

    /// Add a refresh token that can be used.
    fn add_refresh_token(&self, token: &str, user_name: &str) -> String {
        let access = self.next_access_token();
        self.refresh_tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(token.to_owned(), (access.clone(), user_name.to_owned()));
        access
    }

    /// Revoke a refresh token so it cannot be used again.
    fn revoke_token(&self, token: &str) {
        self.revoked_tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(token.to_owned());
        self.refresh_tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(token);
    }

    fn next_access_token(&self) -> String {
        let n = self.token_counter.fetch_add(1, Ordering::SeqCst);
        format!("access-token-{n}")
    }
}

/// An identity provider on `127.0.0.1`, over plain HTTP.
///
/// Serves `/.well-known/openid-configuration` and `/jwks`, from whatever it currently publishes.
/// Dropping it, or [`TestIdp::stop`], closes the port, which is what "the provider is down" looks
/// like to the client: a refused connection.
pub struct TestIdp {
    issuer: String,
    provider: Arc<Provider>,
    server: Option<JoinHandle<()>>,
}

impl TestIdp {
    /// Start a provider publishing `keys` as `(kid, key)` pairs.
    ///
    /// # Panics
    ///
    /// Panics if a loopback port cannot be bound.
    #[allow(clippy::too_many_lines)]
    pub async fn start(keys: &[(&str, SigningKey)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let issuer = format!("http://{}", listener.local_addr().expect("a bound address"));
        let provider = Provider::new(issuer.clone(), keys);

        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(|State(provider): State<Arc<Provider>>| async move {
                    if let Some(delay) = provider.take_discovery_delay() {
                        tokio::time::sleep(delay).await;
                    }
                    Json(provider.discovery())
                }),
            )
            .route(
                "/jwks",
                get(|State(provider): State<Arc<Provider>>| async move { Json(provider.jwks()) }),
            )
            .route(
                "/authorize",
                get(
                    |State(provider): State<Arc<Provider>>,
                     axum::extract::Query(params): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        let code = format!(
                            "code-{}",
                            provider.token_counter.fetch_add(1, Ordering::SeqCst)
                        );
                        let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
                        let state = params.get("state").cloned().unwrap_or_default();
                        let pkce_verifier = params.get("code_challenge").map(|_| "test-verifier");
                        provider.add_auth_code(&code, &redirect_uri, &state, pkce_verifier);
                        let sep = if redirect_uri.contains('?') { "&" } else { "?" };
                        let location = format!("{redirect_uri}{sep}code={code}&state={state}");
                        axum::response::Redirect::to(&location)
                    },
                ),
            )
            .route(
                "/token",
                axum::routing::post(
                    |State(provider): State<Arc<Provider>>,
                     axum::extract::Form(params): axum::extract::Form<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        provider.token_hits.fetch_add(1, Ordering::SeqCst);
                        if *provider
                            .token_failure
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                        {
                            return (
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error": "temporarily_unavailable"})),
                            );
                        }
                        let grant_type = params.get("grant_type").map_or("", String::as_str);
                        if grant_type == "authorization_code" {
                            let code = params.get("code").map_or("", String::as_str);
                            let got_code = {
                                let mut codes = provider
                                    .auth_codes
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner);
                                codes.remove(code).is_some_and(|code| {
                                    params.get("redirect_uri") == Some(&code.redirect_uri)
                                })
                            };
                            if got_code {
                                let refresh = format!(
                                    "refresh-{}",
                                    provider.token_counter.fetch_add(1, Ordering::SeqCst)
                                );
                                let access = provider.add_refresh_token(&refresh, "Test User");
                                (
                                    axum::http::StatusCode::OK,
                                    Json(json!({
                                        "access_token": access,
                                        "token_type": "Bearer",
                                        "expires_in": 300,
                                        "refresh_token": refresh,
                                    })),
                                )
                            } else {
                                (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    Json(json!({"error": "invalid_grant"})),
                                )
                            }
                        } else if grant_type == "refresh_token" {
                            let refresh = params.get("refresh_token").map_or("", String::as_str);
                            let is_revoked = provider
                                .revoked_tokens
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .contains(refresh);
                            if is_revoked {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    Json(json!({"error": "invalid_grant"})),
                                );
                            }
                            let tokens = provider
                                .refresh_tokens
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            if let Some((_old_access, user_name)) = tokens.get(refresh) {
                                let user_name = user_name.clone();
                                drop(tokens);
                                provider.revoke_token(refresh);
                                let new_refresh = format!(
                                    "refresh-{}",
                                    provider.token_counter.fetch_add(1, Ordering::SeqCst)
                                );
                                let access = provider.add_refresh_token(&new_refresh, &user_name);
                                (
                                    axum::http::StatusCode::OK,
                                    Json(json!({
                                        "access_token": access,
                                        "token_type": "Bearer",
                                        "expires_in": 300,
                                        "refresh_token": new_refresh,
                                    })),
                                )
                            } else {
                                (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    Json(json!({"error": "invalid_grant"})),
                                )
                            }
                        } else {
                            (
                                axum::http::StatusCode::BAD_REQUEST,
                                Json(json!({"error": "unsupported_grant_type"})),
                            )
                        }
                    },
                ),
            )
            .route(
                "/revoke",
                axum::routing::post(
                    |State(provider): State<Arc<Provider>>,
                     axum::extract::Form(params): axum::extract::Form<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        provider.revoke_hits.fetch_add(1, Ordering::SeqCst);
                        if *provider
                            .revoke_failure
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                        {
                            return axum::http::StatusCode::SERVICE_UNAVAILABLE;
                        }
                        if let Some(token) = params.get("token") {
                            provider.revoke_token(token);
                        }
                        axum::http::StatusCode::OK
                    },
                ),
            )
            .route(
                "/userinfo",
                get(|| async move {
                    Json(json!({
                        "sub": "test-user",
                        "name": "Test User",
                        "email": "test@example.com",
                    }))
                }),
            )
            .with_state(Arc::clone(&provider));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            issuer,
            provider,
            server: Some(server),
        }
    }

    /// The issuer this provider is, as a verifier is configured with it.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Replace the published keys, as a rotation does.
    pub fn publish(&self, keys: &[(&str, SigningKey)]) {
        *self
            .provider
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = keys
            .iter()
            .map(|(kid, key)| ((*kid).to_owned(), *key))
            .collect();
    }

    /// Serve exactly `keys` as the key set, whatever was published.
    pub fn publish_raw(&self, keys: Vec<Value>) {
        *self
            .provider
            .raw_keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(keys);
    }

    /// Hold the next discovery request for `delay` before answering it. Once.
    pub fn slow_next_discovery(&self, delay: std::time::Duration) {
        *self
            .provider
            .slow_next_discovery
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(delay);
    }

    /// Make the discovery document name `issuer` (or the real one again, for `None`).
    pub fn claim_issuer(&self, issuer: Option<&str>) {
        *self
            .provider
            .claimed_issuer
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = issuer.map(ToOwned::to_owned);
    }

    /// Make the discovery document send key fetching to `uri` (or home again, for `None`).
    pub fn claim_jwks_uri(&self, uri: Option<&str>) {
        *self
            .provider
            .claimed_jwks_uri
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = uri.map(ToOwned::to_owned);
    }

    /// How many times the discovery document was fetched.
    #[must_use]
    pub fn discovery_hits(&self) -> usize {
        self.provider.discovery_hits.load(Ordering::SeqCst)
    }

    /// How many times the key set was fetched.
    #[must_use]
    pub fn jwks_hits(&self) -> usize {
        self.provider.jwks_hits.load(Ordering::SeqCst)
    }

    /// Create a fresh authorization code for a login round-trip.
    /// Returns `(code, code_verifier)` to use in the token exchange.
    #[must_use]
    pub fn issue_code(&self, redirect_uri: &str, state: &str) -> (String, String) {
        let code = format!(
            "code-{}",
            self.provider.token_counter.fetch_add(1, Ordering::SeqCst)
        );
        let verifier = "test-pkce-verifier";
        self.provider
            .add_auth_code(&code, redirect_uri, state, Some(verifier));
        (code, verifier.to_owned())
    }

    /// Inject a refresh token directly (bypassing the authorize flow).
    /// Returns the access token that goes with it.
    #[must_use]
    pub fn seed_refresh_token(&self, refresh_token: &str, user_name: &str) -> String {
        self.provider.add_refresh_token(refresh_token, user_name)
    }

    /// Revoke a refresh token.
    pub fn revoke_refresh_token(&self, token: &str) {
        self.provider.revoke_token(token);
    }

    /// Number of token endpoint requests, including rejected requests.
    #[must_use]
    pub fn token_hits(&self) -> usize {
        self.provider.token_hits.load(Ordering::SeqCst)
    }

    /// Number of revocation endpoint requests.
    #[must_use]
    pub fn revoke_hits(&self) -> usize {
        self.provider.revoke_hits.load(Ordering::SeqCst)
    }

    /// Make token exchange requests return HTTP 503.
    pub fn fail_token_requests(&self) {
        *self
            .provider
            .token_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
    }

    /// Make revocation requests return HTTP 503 without revoking.
    pub fn fail_revocation_requests(&self) {
        *self
            .provider
            .revoke_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
    }

    /// Take the provider down. The port is closed when this returns.
    pub async fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
            // Awaited so the listener is dropped, not merely scheduled to be.
            let _ = server.await;
        }
    }
}

impl Drop for TestIdp {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

/// The committed test CA, as the DER a root store takes.
///
/// # Panics
///
/// Panics if the committed certificate does not parse.
#[must_use]
pub fn tls_ca() -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(include_bytes!("test-only-tls-ca.pem"))
        .expect("the committed CA parses")
}

/// An identity provider on `127.0.0.1` over TLS, with a certificate the committed test CA signed.
///
/// A hand-written HTTP/1.1 responder under `tokio-rustls` rather than `axum` over TLS, because the
/// point of this server is the handshake and the dependency for serving TLS through `axum` is a
/// second HTTP stack for one test.
pub struct TestTlsIdp {
    issuer: String,
    provider: Arc<Provider>,
    server: JoinHandle<()>,
}

impl TestTlsIdp {
    /// Start a TLS provider publishing `keys`.
    ///
    /// # Panics
    ///
    /// Panics if a loopback port cannot be bound or the committed certificate does not load.
    pub async fn start(keys: &[(&str, SigningKey)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address: SocketAddr = listener.local_addr().expect("a bound address");
        let issuer = format!("https://{address}");
        let provider = Provider::new(issuer.clone(), keys);

        let certificate = CertificateDer::from_pem_slice(include_bytes!("test-only-tls-leaf.pem"))
            .expect("the committed leaf parses");
        let key = PrivateKeyDer::from_pem_slice(include_bytes!("test-only-tls-leaf-key.pem"))
            .expect("the committed leaf key parses");
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("the default protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .expect("the committed leaf and key agree");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

        let answers = Arc::clone(&provider);
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let provider = Arc::clone(&answers);
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(stream).await else {
                        // A client that refuses our certificate ends up here: the handshake
                        // fails, and that failure is the assertion of the test that wants it.
                        return;
                    };
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match tls.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => request.extend_from_slice(&buffer[..read]),
                        }
                    }
                    let text = String::from_utf8_lossy(&request);
                    let path = text.split_whitespace().nth(1).unwrap_or_default();
                    let body = match path {
                        "/.well-known/openid-configuration" => provider.discovery(),
                        "/jwks" => provider.jwks(),
                        _ => Value::Null,
                    }
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = tls.write_all(response.as_bytes()).await;
                    let _ = tls.shutdown().await;
                });
            }
        });

        Self {
            issuer,
            provider,
            server,
        }
    }

    /// The issuer, as `https://127.0.0.1:<port>`.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// How many times the key set was fetched.
    #[must_use]
    pub fn jwks_hits(&self) -> usize {
        self.provider.jwks_hits.load(Ordering::SeqCst)
    }
}

impl Drop for TestTlsIdp {
    fn drop(&mut self) {
        self.server.abort();
    }
}
