//! The provider's signing keys: fetched when first needed, cached, and kept through an outage.
//!
//! # What this decides
//!
//! - **Lazy.** Nothing is fetched at startup. A replica that boots while the provider is down must
//!   come up and serve its public methods, for the reason `db::connect` is lazy: a dependency
//!   outage should be visible and self-healing, not a crash loop.
//! - **A cached key outlives the provider.** Refreshing is how the cache stays current, not a
//!   condition of using it: a failed refresh leaves the last good key set in place, so a token
//!   signed by a key we already hold keeps verifying through a provider outage.
//! - **A set with no usable key is a failed fetch.** A provider that answers 200 with
//!   `"keys": []` (mid-deploy, misconfigured) has not revoked everything, and replacing the cache
//!   with nothing turned that glitch into a 401 for every caller. Only a non-empty set replaces the
//!   old one, so a rotation that drops a key still drops it.
//! - **An unknown `kid` may refetch, but not often.** A provider rotates keys by publishing a new
//!   one, so a `kid` we have not seen is a legitimate reason to look again. It is also something
//!   any caller can fabricate, so the refetch is rate-limited ([`Timing::min_refetch_interval`]):
//!   a flood of random `kid`s costs the provider one request per interval, not one per token.
//! - **A key we hold is never re-fetched on a bad signature.** A token that names a known `kid`
//!   and fails its signature is simply wrong; refetching on it would hand the same fabrication
//!   to anyone who wanted the provider hit. The periodic refresh picks up a key re-published under
//!   an old `kid`.
//! - **Two keys under one `kid` are ambiguous, and neither is trusted.** Which of them signed is
//!   not a question to guess at, and "the last one wins" would let the order of a list the provider
//!   controls decide it.
//! - **A key is used only for the algorithm the verifier was asked to check.** Its `kty` (RSA,
//!   EC, `OKP`) must fit the token's algorithm family: RSA keys for RS* and PS*, EC P-256 for ES256,
//!   EC P-384 for ES384, `OKP` Ed25519 for `EdDSA`. When the JWK carries an `alg`, that must equal
//!   the token's; a curve mismatch is refused.
//!
//! # Concurrency, and why the fetch is not the caller's
//!
//! A refresh runs on **its own task**, which records its own outcome. A caller that starts one
//! waits for it, and is free to be dropped (the client went away, an outer timeout fired): the
//! fetch finishes and records what it found regardless. When the fetch belonged to the caller, a
//! cancelled first request left the rate limit spent with no outcome behind it, and every request
//! for the next interval was told a healthy provider was down.
//!
//! One refresh is in flight at a time; a caller that needs a key waits for it (a `watch` channel,
//! closed when the task is done) and then reads the result. A caller that already holds a usable
//! key does not wait at all: it uses the stale set while the refresh runs. That is the cheap
//! version of a background refresh, and enough for this traffic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey};
use serde::Deserialize;
use tokio::sync::watch;

use crate::http::FetchError;
use crate::{Timing, check_endpoint};

/// Why no key could be produced for a `kid`.
#[derive(Debug)]
pub enum KeyError {
    /// The provider answered, and does not publish this key: a statement about the token.
    Unknown,
    /// We could not find out: a statement about this service's reach, not about the token.
    Unavailable(&'static str),
}

/// A key with enough metadata to decide whether it can verify a given algorithm.
#[derive(Debug, Clone)]
struct StoredKey {
    key: Arc<DecodingKey>,
    /// The key's type and curve, for matching against the token's algorithm family.
    params: AlgorithmParameters,
    /// When the JWK carries an `alg`, the key is restricted to that algorithm.
    algorithm: Option<KeyAlgorithm>,
}

/// The keys of one successful fetch.
struct KeySet {
    keys: HashMap<String, Arc<StoredKey>>,
    fetched: Instant,
}

/// Everything the refresh task and its callers share, under one lock held only briefly: never
/// across a network call.
#[allow(
    clippy::struct_field_names,
    reason = "the `last_` prefix is the point: these three are one fact, the outcome of the last attempt"
)]
struct State {
    keys: Option<Arc<KeySet>>,
    last_attempt: Option<Instant>,
    /// Whether the last attempt produced a key set. Starts false: nothing has been fetched.
    last_ok: bool,
    /// What went wrong the last time, for the one-line answer to a request that waited on it.
    last_failure: &'static str,
    /// Closed when the refresh in flight is done.
    inflight: Option<watch::Receiver<()>>,
}

struct Inner {
    client: crate::http::Client,
    issuer: String,
    discovery_url: url::Url,
    timing: Timing,
    /// The configured algorithms. A refresh whose keys are usable for none of these is a
    /// failed fetch, so a misconfigured provider (e.g. publishing only RS384 keys while
    /// RS256 is configured) does not evict the last good key set.
    algorithms: Vec<Algorithm>,
    state: Mutex<State>,
}

pub struct KeySource {
    inner: Arc<Inner>,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

#[derive(Deserialize)]
struct RawKeys {
    keys: Vec<serde_json::Value>,
}

impl KeySource {
    pub fn new(
        client: crate::http::Client,
        issuer: String,
        discovery_url: url::Url,
        timing: Timing,
        algorithms: &[Algorithm],
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                issuer,
                discovery_url,
                timing,
                algorithms: algorithms.to_vec(),
                state: Mutex::new(State {
                    keys: None,
                    last_attempt: None,
                    last_ok: false,
                    last_failure: "no key set has been fetched",
                    inflight: None,
                }),
            }),
        }
    }

    /// The key published under `kid`, usable for `algorithm`.
    ///
    /// Must be called inside a tokio runtime: a refresh is spawned.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard is the decision: look, claim the refresh, and record that it is in flight, as one step"
    )]
    pub async fn key(&self, kid: &str, algorithm: Algorithm) -> Result<Arc<DecodingKey>, KeyError> {
        let mut done = {
            let mut state = self.inner.lock();
            if let Some(key) = self.inner.fresh_key(&state, kid, algorithm) {
                return Ok(key);
            }

            // A receiver whose sender is gone is a refresh that is over (or died): not in flight.
            let inflight = state
                .inflight
                .clone()
                .filter(|receiver| receiver.has_changed().is_ok());
            if let Some(receiver) = inflight {
                // Someone is already fetching. A caller with a key in hand does not queue behind
                // them: the key it holds is the one a successful refresh would hand it again.
                if let Some(key) = state
                    .keys
                    .as_ref()
                    .and_then(|set| matching_key(set, kid, algorithm))
                {
                    return Ok(key);
                }
                receiver
            } else if state
                .last_attempt
                .is_none_or(|at| at.elapsed() >= self.inner.timing.min_refetch_interval)
            {
                state.last_attempt = Some(Instant::now());
                let (finished, receiver) = watch::channel(());
                state.inflight = Some(receiver.clone());
                let inner = Arc::clone(&self.inner);
                tokio::spawn(async move { inner.refresh(finished).await });
                receiver
            } else {
                return Self::verdict(&state, kid, algorithm);
            }
        };

        // Returns when the task drops its sender, which is the only thing it ever does to it.
        let _ = done.changed().await;
        Self::verdict(&self.inner.lock(), kid, algorithm)
    }

    /// Whatever the cache holds now, stale or not. A refresh that failed leaves the last good set
    /// behind, and a key in it is still a key the provider published.
    fn verdict(
        state: &State,
        kid: &str,
        algorithm: Algorithm,
    ) -> Result<Arc<DecodingKey>, KeyError> {
        match state
            .keys
            .as_ref()
            .and_then(|set| matching_key(set, kid, algorithm))
        {
            Some(key) => Ok(key),
            // The provider answered the last time we asked and this key is not in it.
            None if state.last_ok => Err(KeyError::Unknown),
            // The last time we asked, nobody (usable) answered. We do not know whether the key
            // exists.
            None => Err(KeyError::Unavailable(state.last_failure)),
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The key for `kid` if the set holding it is young enough to trust without asking again.
    fn fresh_key(
        &self,
        state: &State,
        kid: &str,
        algorithm: Algorithm,
    ) -> Option<Arc<DecodingKey>> {
        let set = state
            .keys
            .as_ref()
            .filter(|set| set.fetched.elapsed() < self.timing.refresh_interval)?;
        matching_key(set, kid, algorithm)
    }

    /// One refresh, run to completion whether or not anyone is still waiting for it.
    async fn refresh(self: Arc<Self>, finished: watch::Sender<()>) {
        let outcome = tokio::time::timeout(self.timing.fetch_budget(), self.fetch()).await;

        let mut state = self.lock();
        match outcome {
            Ok(Ok(set)) => {
                state.keys = Some(Arc::new(set));
                state.last_ok = true;
            }
            Ok(Err(failure)) => {
                tracing::warn!(
                    reason = failure.reason(),
                    "could not refresh the identity provider's keys"
                );
                state.last_ok = false;
                state.last_failure = failure.reason();
            }
            Err(_elapsed) => {
                tracing::warn!("refreshing the identity provider's keys ran out of time");
                state.last_ok = false;
                state.last_failure = "refreshing the key set ran out of time";
            }
        }
        state.inflight = None;
        drop(state);
        // Closing the channel is what wakes the waiters, after the outcome is recorded.
        drop(finished);
    }

    async fn fetch(&self) -> Result<KeySet, Failure> {
        let discovery: Discovery = self.client.get_json(&self.discovery_url).await?;

        // OIDC Discovery §4.3: the issuer in the document MUST be the one the document
        // was fetched for. Without the comparison, whoever controls what answers at the
        // discovery URL (a misrouted proxy, a hijacked name) chooses which keys we trust for an
        // issuer we were configured with.
        if discovery.issuer != self.issuer {
            return Err(Failure::IssuerMismatch);
        }
        let jwks_uri = url::Url::parse(&discovery.jwks_uri).map_err(|_| Failure::BadJwksUri)?;
        check_endpoint(&jwks_uri).map_err(|_| Failure::BadJwksUri)?;

        let raw: RawKeys = self.client.get_json(&jwks_uri).await?;

        let mut by_kid: HashMap<String, Vec<Arc<StoredKey>>> = HashMap::new();
        for (kid, key) in raw.keys.iter().filter_map(usable) {
            by_kid.entry(kid).or_default().push(key);
        }
        let before = by_kid.len();
        let keys: HashMap<String, Arc<StoredKey>> = by_kid
            .into_iter()
            .filter_map(|(kid, mut keys)| (keys.len() == 1).then(|| (kid, keys.remove(0))))
            .collect();
        if keys.len() != before {
            tracing::warn!(
                ambiguous = before - keys.len(),
                "the identity provider published several keys under one kid; none of those is trusted"
            );
        }
        // Nothing usable is not an answer: see the module docs. The caller keeps what it had.
        if keys.is_empty() {
            return Err(Failure::NoUsableKeys);
        }
        // The set must also have at least one key usable for the configured algorithms,
        // or a refresh replaces good cached keys with keys of a wrong family that every
        // lookup will reject (the cache regression found in PR #94 review).
        let any_usable = keys.values().any(|stored| {
            self.algorithms.iter().any(|&alg| {
                if stored
                    .algorithm
                    .is_some_and(|jwk_alg| KeyAlgorithm::from(alg) != jwk_alg)
                {
                    return false;
                }
                key_fits_algorithm(&stored.params, alg)
            })
        });
        if !any_usable {
            return Err(Failure::NoUsableKeys);
        }
        Ok(KeySet {
            keys,
            fetched: Instant::now(),
        })
    }
}

/// Why a fetch did not produce a key set.
enum Failure {
    Fetch(FetchError),
    IssuerMismatch,
    BadJwksUri,
    NoUsableKeys,
}

impl Failure {
    /// A fixed description, so a log line never carries anything the network sent.
    const fn reason(&self) -> &'static str {
        match self {
            Self::Fetch(FetchError::Timeout) => "the provider did not answer in time",
            Self::Fetch(FetchError::Connect) => "could not connect to the provider",
            Self::Fetch(FetchError::Transport) => "the request to the provider failed",
            Self::Fetch(FetchError::Status(_)) => "the provider answered with an error status",
            Self::Fetch(FetchError::TooLarge) => "the provider's response was too large",
            Self::Fetch(FetchError::Malformed) => {
                "the provider's response was not what discovery describes"
            }
            Self::IssuerMismatch => {
                "the discovery document names a different issuer than the configured one"
            }
            Self::BadJwksUri => "the discovery document's jwks_uri is not an acceptable endpoint",
            Self::NoUsableKeys => "the provider's key set holds no usable signing key",
        }
    }
}

impl From<FetchError> for Failure {
    fn from(error: FetchError) -> Self {
        Self::Fetch(error)
    }
}

/// `(kid, key)` for a signing key this verifier could use, or nothing.
///
/// Parsed one key at a time: a set may carry keys of a kind this crate has no use for (an
/// encryption key), and one of those must not make the whole set unreadable. Skipped
/// quietly because the set is the provider's, not ours, to arrange.
///
/// Accepts RSA, EC, and OKP keys marked for signing (or with no `use`). The `alg`
/// parameter on the JWK is stored for later matching against the token's algorithm.
fn usable(value: &serde_json::Value) -> Option<(String, Arc<StoredKey>)> {
    let jwk: Jwk = serde_json::from_value(value.clone()).ok()?;
    let kid = jwk.common.key_id.clone()?;

    // A key marked for encryption is not one this verifier will ever be asked to check a
    // signature with.
    if jwk
        .common
        .public_key_use
        .as_ref()
        .is_some_and(|usage| *usage != PublicKeyUse::Signature)
    {
        return None;
    }

    let params = jwk.algorithm.clone();
    let key_alg = jwk.common.key_algorithm;
    let key = build_decoding_key(&params)?;
    Some((
        kid,
        Arc::new(StoredKey {
            key: Arc::new(key),
            params,
            algorithm: key_alg,
        }),
    ))
}

/// Build a [`DecodingKey`] from the JWK's algorithm parameters.
fn build_decoding_key(params: &AlgorithmParameters) -> Option<DecodingKey> {
    match params {
        AlgorithmParameters::RSA(rsa) => DecodingKey::from_rsa_components(&rsa.n, &rsa.e).ok(),
        AlgorithmParameters::EllipticCurve(ec) => {
            DecodingKey::from_ec_components(&ec.x, &ec.y).ok()
        }
        AlgorithmParameters::OctetKeyPair(okp) => DecodingKey::from_ed_components(&okp.x).ok(),
        // OctetKey (symmetric), Other (unsupported) — not usable for signature verification.
        _ => None,
    }
}

/// The key from `set` under `kid` that is usable for `algorithm`, or nothing.
fn matching_key(set: &KeySet, kid: &str, algorithm: Algorithm) -> Option<Arc<DecodingKey>> {
    let stored = set.keys.get(kid)?;

    // A JWK that specifies an `alg` must match the token's algorithm.
    if stored
        .algorithm
        .is_some_and(|jwk_alg| KeyAlgorithm::from(algorithm) != jwk_alg)
    {
        return None;
    }

    // The key's type must fit the token's algorithm family.
    if !key_fits_algorithm(&stored.params, algorithm) {
        return None;
    }

    Some(Arc::clone(&stored.key))
}

/// Whether a key with the given JWK parameters can verify a token signed with `algorithm`.
fn key_fits_algorithm(params: &AlgorithmParameters, algorithm: Algorithm) -> bool {
    match algorithm {
        Algorithm::RS256
        | Algorithm::RS384
        | Algorithm::RS512
        | Algorithm::PS256
        | Algorithm::PS384
        | Algorithm::PS512 => {
            matches!(params, AlgorithmParameters::RSA(_))
        }
        Algorithm::ES256 => {
            matches!(params, AlgorithmParameters::EllipticCurve(ec) if ec.curve == EllipticCurve::P256)
        }
        Algorithm::ES384 => {
            matches!(params, AlgorithmParameters::EllipticCurve(ec) if ec.curve == EllipticCurve::P384)
        }
        Algorithm::EdDSA => {
            matches!(params, AlgorithmParameters::OctetKeyPair(okp) if okp.curve == EllipticCurve::Ed25519)
        }
        // HS256, HS384, HS512, or future variants — the verifier never configures these.
        _ => false,
    }
}

impl Timing {
    /// The whole of one refresh: discovery, then the key set. Two requests, each bounded by
    /// `request_timeout`, so the budget is both of them and not a third.
    #[must_use]
    pub fn fetch_budget(&self) -> Duration {
        self.request_timeout * 2
    }
}
