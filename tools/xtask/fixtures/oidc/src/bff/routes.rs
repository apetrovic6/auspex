//! BFF route handlers: login, callback, session, logout.
//!
//! Public client + PKCE, no client secret. The refresh token lives in an encrypted
//! `__Host-session` cookie; the SPA holds only a short-lived access token in memory.
//!
//! The `OAuth2` protocol is implemented directly rather than through the `oauth2` crate,
//! avoiding the `AsyncHttpClient` trait complexity. The only crypto needed is SHA-256
//! (for PKCE S256) and random bytes (for state and verifier), both of which `sha2` and
//! `rand_core` provide.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use rand_core::{OsRng, RngCore};
use serde::Serialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::bff::cookies::{self, CookieKey, FlowValue, SessionValue};

/// The BFF's static configuration.
#[derive(Debug, Clone)]
pub(crate) struct BffConfig {
    pub public_url: Url,
    pub issuer: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub login_audience: Option<String>,
}

/// The BFF's runtime state, shared across handlers.
pub(crate) struct BffState {
    config: BffConfig,
    cookie_key: CookieKey,
    /// Cached discovery document fields.
    discovery: tokio::sync::OnceCell<Endpoints>,
    /// HTTP client for token/revocation requests.
    http: crate::http::Client,
}

/// Fields from the provider's discovery document that the BFF needs.
#[derive(Debug, Clone)]
struct Endpoints {
    authorization: Url,
    token: Url,
    revocation: Option<Url>,
    end_session: Option<Url>,
    userinfo: Option<Url>,
}

impl BffState {
    pub(crate) fn new(
        config: BffConfig,
        cookie_key: CookieKey,
        roots: rustls::RootCertStore,
    ) -> Self {
        let tls = crate::http::tls_config(roots)
            .expect("TLS config is infallible with compiled-in roots");
        let http = crate::http::Client::new(tls, Duration::from_millis(1500));
        Self {
            config,
            cookie_key,
            discovery: tokio::sync::OnceCell::new(),
            http,
        }
    }

    async fn discover(&self) -> Result<&Endpoints, DiscoveryError> {
        self.discovery
            .get_or_try_init(|| async {
                fn required<'a>(
                    doc: &'a serde_json::Value,
                    field: &str,
                ) -> Result<&'a str, DiscoveryError> {
                    doc[field]
                        .as_str()
                        .ok_or(DiscoveryError::Malformed("missing field"))
                }
                let discovery_url = format!(
                    "{}/.well-known/openid-configuration",
                    self.config.issuer.trim_end_matches('/')
                );
                let doc: serde_json::Value = self
                    .http
                    .get_json(
                        &discovery_url
                            .parse()
                            .map_err(|_| DiscoveryError::Unavailable)?,
                    )
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;

                Ok(Endpoints {
                    authorization: required(&doc, "authorization_endpoint")?
                        .parse()
                        .map_err(|_| DiscoveryError::Malformed("authorization_endpoint"))?,
                    token: required(&doc, "token_endpoint")?
                        .parse()
                        .map_err(|_| DiscoveryError::Malformed("token_endpoint"))?,
                    revocation: doc["revocation_endpoint"]
                        .as_str()
                        .and_then(|s| s.parse().ok()),
                    end_session: doc["end_session_endpoint"]
                        .as_str()
                        .and_then(|s| s.parse().ok()),
                    userinfo: doc["userinfo_endpoint"]
                        .as_str()
                        .and_then(|s| s.parse().ok()),
                })
            })
            .await
    }
}

#[derive(Debug)]
enum DiscoveryError {
    Unavailable,
    #[allow(dead_code)]
    Malformed(&'static str),
}

// -------------------------------------------------------------------------------------------------
// GET /auth/login
// -------------------------------------------------------------------------------------------------

pub(crate) async fn login(State(state): State<Arc<BffState>>) -> Result<Response, LoginError> {
    let doc = state
        .discover()
        .await
        .map_err(|_| LoginError::ProviderUnavailable)?;

    // Generate PKCE verifier and S256 challenge.
    let code_verifier = random_base64url(32);
    let code_challenge = pkce_s256_challenge(&code_verifier);
    let csrf_state = random_base64url(16);

    // Build the authorization URL.
    let scopes = state.config.scopes.join(" ");
    let mut auth_url = doc.authorization.clone();
    {
        let mut query = auth_url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &state.config.client_id);
        query.append_pair(
            "redirect_uri",
            state
                .config
                .public_url
                .join("auth/callback")
                .expect("origin URL")
                .as_str(),
        );
        query.append_pair("scope", &scopes);
        query.append_pair("state", &csrf_state);
        query.append_pair("code_challenge", &code_challenge);
        query.append_pair("code_challenge_method", "S256");
        if let Some(audience) = &state.config.login_audience {
            query.append_pair("audience", audience);
        }
    }

    // Store state + PKCE verifier in the flow cookie.
    let flow = FlowValue::new(csrf_state, code_verifier);
    let cookie_value = flow.to_cookie_value(&state.cookie_key);

    let mut response = found(auth_url.as_str());
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookies::set_flow_cookie(&cookie_value, 600)
            .parse()
            .expect("a valid Set-Cookie value"),
    );
    Ok(response)
}

#[derive(Debug)]
pub(crate) enum LoginError {
    ProviderUnavailable,
}

impl IntoResponse for LoginError {
    fn into_response(self) -> Response {
        match self {
            Self::ProviderUnavailable => (
                StatusCode::BAD_GATEWAY,
                "the identity provider is not available",
            )
                .into_response(),
        }
    }
}

// -------------------------------------------------------------------------------------------------
// GET /auth/callback
// -------------------------------------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
pub(crate) struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    #[serde(rename = "error_description")]
    error_description: Option<String>,
}

pub(crate) async fn callback(
    State(state): State<Arc<BffState>>,
    Query(query): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Response {
    // Provider error → redirect to / with ?login=denied.
    if let Some(error) = query.error {
        tracing::warn!(%error, description = ?query.error_description, "login denied by provider");
        let target = "/?login=denied".to_string();
        return redirect_clearing_flow(&target);
    }

    // Extract flow cookie.
    let Some(flow_value) = extract_flow_cookie(&headers, &state.cookie_key) else {
        return bad_request_clearing_flow("missing or invalid login flow cookie");
    };

    // Check state parameter.
    let Some(ref query_state) = query.state else {
        return bad_request_clearing_flow("missing state parameter");
    };

    if !constant_time_eq(query_state.as_bytes(), flow_value.state.as_bytes()) {
        return bad_request_clearing_flow("state mismatch");
    }

    // Exchange the authorization code.
    let Some(ref code) = query.code else {
        return bad_request_clearing_flow("missing authorization code");
    };

    let Ok(doc) = state.discover().await else {
        return gateway_clearing_flow();
    };

    let redirect_uri = state
        .config
        .public_url
        .join("auth/callback")
        .expect("origin URL");
    let token_params = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", &state.config.client_id),
        ("code_verifier", &flow_value.code_verifier),
    ];
    let body = form_urlencode(&token_params);

    match state
        .http
        .post_form(doc.token.as_str(), body.as_bytes())
        .await
    {
        Ok(response) => {
            if response.status() >= 500 {
                log_token_error(&response);
                return gateway_clearing_flow();
            }
            let body = String::from_utf8_lossy(response.body());
            if (200..300).contains(&response.status())
                && let Ok(token) = serde_json::from_str::<TokenResponse>(&body)
                && let Some(refresh_token) = token.refresh_token
            {
                let session = SessionValue::new(refresh_token);
                let cookie_value = session.to_cookie_value(&state.cookie_key);

                let mut response = found("/");
                response.headers_mut().insert(
                    header::SET_COOKIE,
                    cookies::set_session_cookie(&cookie_value)
                        .parse()
                        .expect("a valid Set-Cookie value"),
                );
                response.headers_mut().append(
                    header::SET_COOKIE,
                    cookies::clear_flow_cookie()
                        .parse()
                        .expect("a valid Set-Cookie value"),
                );
                return response;
            }
            log_token_error(&response);
            redirect_clearing_flow("/?login=denied")
        }
        Err(err) => {
            tracing::warn!(?err, "token exchange failed");
            gateway_clearing_flow()
        }
    }
}

// -------------------------------------------------------------------------------------------------
// POST /auth/session
// -------------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub access_token: String,
    pub expires_in: u64,
    pub name: String,
}

pub(crate) async fn session(State(state): State<Arc<BffState>>, headers: HeaderMap) -> Response {
    let Some(session) = extract_session_cookie(&headers, &state.cookie_key) else {
        return session_error_response(StatusCode::UNAUTHORIZED, true);
    };

    let Ok(doc) = state.discover().await else {
        return (
            StatusCode::BAD_GATEWAY,
            "the identity provider is not available",
        )
            .into_response();
    };

    let token_params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", &session.refresh_token),
        ("client_id", &state.config.client_id),
    ];
    let body = form_urlencode(&token_params);

    match state
        .http
        .post_form(doc.token.as_str(), body.as_bytes())
        .await
    {
        Ok(response) => {
            let resp_body = String::from_utf8_lossy(response.body());
            if (200..300).contains(&response.status())
                && let Ok(token) = serde_json::from_str::<TokenResponse>(&resp_body)
            {
                let access_token = token.access_token;
                let expires_in = token.expires_in.unwrap_or(300);
                let name = fetch_userinfo(&state, doc, &access_token)
                    .await
                    .unwrap_or_default();

                // Rotate: store the new refresh token.
                let new_rt = token.refresh_token.unwrap_or(session.refresh_token);
                let new_session = SessionValue::new(new_rt);
                let cookie_value = new_session.to_cookie_value(&state.cookie_key);

                let mut response = Json(SessionResponse {
                    access_token,
                    expires_in,
                    name,
                })
                .into_response();
                response.headers_mut().insert(
                    header::SET_COOKIE,
                    cookies::set_session_cookie(&cookie_value)
                        .parse()
                        .expect("a valid Set-Cookie value"),
                );
                return response;
            }

            // Check for invalid_grant → credential expired.
            if let Ok(error) = serde_json::from_str::<OAuthErrorResponse>(&resp_body)
                && error.error == "invalid_grant"
            {
                return session_error_response(StatusCode::UNAUTHORIZED, true);
            }

            log_token_error(&response);
            (
                StatusCode::BAD_GATEWAY,
                "the identity provider returned an unexpected response",
            )
                .into_response()
        }
        Err(err) => {
            tracing::warn!(?err, "token refresh failed");
            (
                StatusCode::BAD_GATEWAY,
                "the identity provider is not available",
            )
                .into_response()
        }
    }
}

fn session_error_response(status: StatusCode, clear_cookie: bool) -> Response {
    let mut response = status.into_response();
    if clear_cookie {
        response.headers_mut().insert(
            header::SET_COOKIE,
            cookies::clear_session_cookie()
                .parse()
                .expect("a valid Set-Cookie value"),
        );
    }
    response
}

// -------------------------------------------------------------------------------------------------
// POST /auth/logout
// -------------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct LogoutResponse {
    pub end_session_endpoint: Option<String>,
}

pub(crate) async fn logout(
    State(state): State<Arc<BffState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::SET_COOKIE,
        cookies::clear_session_cookie()
            .parse()
            .expect("a valid Set-Cookie value"),
    );

    // Best-effort RFC 7009 revocation.
    if let Some(session) = extract_session_cookie(&headers, &state.cookie_key)
        && let Ok(doc) = state.discover().await
        && let Some(revocation_endpoint) = &doc.revocation
    {
        let params = [
            ("token", session.refresh_token.as_str()),
            ("client_id", state.config.client_id.as_str()),
        ];
        let body = form_urlencode(&params);
        let _ = state
            .http
            .post_form(revocation_endpoint.as_str(), body.as_bytes())
            .await;
    }

    let end_session_endpoint = state
        .discover()
        .await
        .ok()
        .and_then(|d| d.end_session.clone())
        .map(|u| u.to_string());

    (
        response_headers,
        Json(LogoutResponse {
            end_session_endpoint,
        }),
    )
}

// -------------------------------------------------------------------------------------------------
// Response helpers
// -------------------------------------------------------------------------------------------------

fn found(target: &str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, target)]).into_response()
}

fn redirect_clearing_flow(target: &str) -> Response {
    let mut response = found(target);
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookies::clear_flow_cookie()
            .parse()
            .expect("a valid Set-Cookie value"),
    );
    response
}

fn bad_request_clearing_flow(message: &str) -> Response {
    let mut response = (StatusCode::BAD_REQUEST, message.to_owned()).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookies::clear_flow_cookie()
            .parse()
            .expect("a valid Set-Cookie value"),
    );
    response
}

fn gateway_clearing_flow() -> Response {
    let mut response = (
        StatusCode::BAD_GATEWAY,
        "the identity provider is not available",
    )
        .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookies::clear_flow_cookie()
            .parse()
            .expect("a valid Set-Cookie value"),
    );
    response
}

// -------------------------------------------------------------------------------------------------
// PKCE helpers
// -------------------------------------------------------------------------------------------------

/// Generate `len` random bytes, base64url-encoded without padding.
fn random_base64url(len: usize) -> String {
    let mut bytes = vec![0_u8; len];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

/// Compute the S256 PKCE challenge from a code verifier.
fn pkce_s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Serialize key-value pairs as `application/x-www-form-urlencoded`.
fn form_urlencode(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", url_escape(k), url_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// URL-encode a string for form bodies.
fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_owned(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// -------------------------------------------------------------------------------------------------
// Cookie helpers
// -------------------------------------------------------------------------------------------------

fn extract_flow_cookie(headers: &HeaderMap, key: &CookieKey) -> Option<FlowValue> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    let value = parse_cookie(cookie_header, "__Host-login-flow")?;
    FlowValue::from_cookie_value(value, key).ok()
}

fn extract_session_cookie(headers: &HeaderMap, key: &CookieKey) -> Option<SessionValue> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    let value = parse_cookie(cookie_header, "__Host-session")?;
    SessionValue::from_cookie_value(value, key).ok()
}

fn parse_cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}=");
    for part in header.split(';') {
        let trimmed = part.trim();
        if let Some(value) = trimmed.strip_prefix(&prefix) {
            return Some(value);
        }
    }
    None
}

// -------------------------------------------------------------------------------------------------
// Token response types
// -------------------------------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct OAuthErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

// Never include a raw response: even error responses can contain credentials.
fn log_token_error(response: &crate::http::FormResponse) {
    let error = serde_json::from_slice::<OAuthErrorResponse>(response.body()).ok();
    tracing::warn!(
        status = response.status(),
        error = ?error.as_ref().map(|e| e.error.as_str()),
        error_description = ?error.as_ref().and_then(|e| e.error_description.as_deref()),
        "unexpected token response"
    );
}

// -------------------------------------------------------------------------------------------------
// Userinfo
// -------------------------------------------------------------------------------------------------

async fn fetch_userinfo(
    state: &BffState,
    doc: &Endpoints,
    access_token: &str,
) -> Result<String, ()> {
    let Some(userinfo_url) = &doc.userinfo else {
        return Err(());
    };
    let body: serde_json::Value = state
        .http
        .get_json_bearer(userinfo_url.as_str(), access_token)
        .await
        .map_err(|_| ())?;
    Ok(body["name"]
        .as_str()
        .or_else(|| body["email"].as_str())
        .unwrap_or("")
        .to_owned())
}

// -------------------------------------------------------------------------------------------------
// Constant-time comparison
// -------------------------------------------------------------------------------------------------

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0_u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"abc", b"abc"));
    }

    #[test]
    fn constant_time_eq_mismatches() {
        assert!(!constant_time_eq(b"abc", b"xyz"));
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn parse_cookie_extracts_value() {
        assert_eq!(
            parse_cookie("__Host-session=abc123; Path=/", "__Host-session"),
            Some("abc123")
        );
        assert_eq!(
            parse_cookie("a=1; __Host-session=abc123; b=2", "__Host-session"),
            Some("abc123")
        );
        assert_eq!(parse_cookie("a=1; b=2", "__Host-session"), None);
    }

    #[test]
    fn pkce_challenge_is_base64url_sha256() {
        // From RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_s256_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
