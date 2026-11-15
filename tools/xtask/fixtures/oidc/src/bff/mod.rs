//! Browser login through the service (BFF — Backend For Frontend).
//!
//! The BFF handles `OAuth2` authorization-code flows with PKCE so the SPA never sees a refresh
//! token. The refresh token is stored in an encrypted `__Host-session` cookie (`HttpOnly`,
//! Secure, SameSite=Strict); the SPA holds only a short-lived access token in memory and sends
//! it as `Authorization: Bearer` on Connect calls (ADR 0012 unchanged).
//!
//! # Cookies
//!
//! - **Flow cookie** `__Host-login-flow`: holds state + PKCE verifier. SameSite=Lax (must
//!   survive the `IdP`'s top-level redirect), 10-minute max-age, encrypted.
//! - **Session cookie** `__Host-session`: holds the refresh token. SameSite=Strict, no
//!   max-age (lasts until the browser closes or logout), encrypted.
//!
//! Both are AES-256-GCM with a random nonce per write. The key is 32 bytes read from a file.
//!
//! # CSRF
//!
//! POST endpoints require `Sec-Fetch-Site: same-origin`, or when absent an `Origin` equal to
//! the configured public origin. The refresh token never appears in any response body or log.
//!
//! # Multi-tab
//!
//! The SPA wraps `/auth/session` calls in `navigator.locks.request("auth-session", ...)` so
//! tabs serialise refreshes. The BFF is stateless: no DB, no server-side session table. A
//! non-SPA caller can still race and is documented as accepted.

pub mod cookies;
pub mod csrf;
pub mod routes;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::{Router, middleware};

use crate::bff::cookies::CookieKey;
use crate::bff::routes::{BffConfig, BffState};

/// The BFF's configuration, derived from `AuthSettings`.
#[derive(Debug, Clone)]
pub struct BffSetup {
    /// The browser-facing origin, validated with [`parse_public_url`].
    pub public_url: url::Url,
    /// The OIDC provider's issuer URL.
    pub issuer: String,
    /// The OIDC client id.
    pub client_id: String,
    /// Scopes requested at login (default: `openid profile email`).
    pub scopes: Vec<String>,
    /// Optional `audience` parameter sent to the authorization endpoint.
    pub login_audience: Option<String>,
    /// 32-byte cookie encryption key.
    pub cookie_key: CookieKey,
}

/// Build the BFF's axum router.
///
/// Mount this at the root, before the SPA fallback, outside the `/api` Connect nest.
///
pub fn bff_router(setup: BffSetup) -> Router {
    router_with_roots(setup, crate::http::webpki_roots())
}

/// Build a BFF trusting explicit test CA roots in addition to the compiled-in roots.
///
/// # Errors
///
/// Returns an error when an extra certificate cannot be added.
#[cfg(feature = "testing")]
pub fn bff_router_with_extra_roots(
    setup: BffSetup,
    extra: Vec<rustls::pki_types::CertificateDer<'static>>,
) -> Result<Router, rustls::Error> {
    let mut roots = crate::http::webpki_roots();
    for certificate in extra {
        roots.add(certificate)?;
    }
    Ok(router_with_roots(setup, roots))
}

fn router_with_roots(setup: BffSetup, roots: rustls::RootCertStore) -> Router {
    let config = BffConfig {
        public_url: setup.public_url.clone(),
        issuer: setup.issuer,
        client_id: setup.client_id,
        scopes: setup.scopes,
        login_audience: setup.login_audience,
    };
    let state = BffState::new(config, setup.cookie_key, roots);
    let shared = Arc::new(state);

    Router::new()
        .route("/auth/login", get(routes::login))
        .route("/auth/callback", get(routes::callback))
        .route(
            "/auth/session",
            post(routes::session).route_layer(middleware::from_fn_with_state(
                setup.public_url.origin().ascii_serialization(),
                csrf::csrf_middleware,
            )),
        )
        .route(
            "/auth/logout",
            post(routes::logout).route_layer(middleware::from_fn_with_state(
                setup.public_url.origin().ascii_serialization(),
                csrf::csrf_middleware,
            )),
        )
        .with_state(shared)
}

/// Validate the browser-facing origin used for callbacks and CSRF checks.
///
/// # Errors
///
/// Rejects surrounding whitespace, trailing-dot hosts, non-HTTPS URLs except loopback
/// HTTP, credentials, and non-origin components.
pub fn parse_public_url(value: &str) -> Result<url::Url, &'static str> {
    if value.trim() != value {
        return Err("must not contain leading or trailing whitespace");
    }
    let url = url::Url::parse(value).map_err(|_| "must be an absolute URL")?;
    if url.host_str().is_some_and(|host| host.ends_with('.')) {
        return Err("must not use a trailing-dot host");
    }
    if url.host_str().is_none()
        || !(url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))))
    {
        return Err("must use https (http is allowed only for localhost or 127.0.0.1)");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || value.split_once("://").is_some_and(|(_, rest)| {
            rest.split(['/', '?', '#'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        })
    {
        return Err("must not contain userinfo");
    }
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Err("must contain only an origin, without path, query or fragment");
    }
    Ok(url)
}
