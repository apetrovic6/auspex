//! The HTTPS client the verifier fetches discovery and keys with, and the things that are easy to
//! get wrong about it: which roots it trusts, how much it will read, and what it will follow.
//!
//! # Why not `reqwest`
//!
//! It was the first choice and it is wrong here, for a reason that is not about this crate.
//! `reqwest`'s TLS features are unified across everything one cargo invocation builds, and
//! `opentelemetry-otlp` (in `crates/telemetry`) builds a **blocking `reqwest` client** in `init`.
//! Any TLS feature this crate asked of `reqwest` therefore reached the telemetry exporter too:
//! with the provider-less spelling that client panics at construction, and with the default one it
//! verifies against the platform trust store, which does not exist in the `FROM scratch` image, so
//! it fails to *build* there. Either way a service with an OTLP endpoint configured died at
//! startup, in any workspace-unified build — and no per-package test could see it. Leaving
//! `reqwest`'s features alone, and doing this crate's HTTPS with `hyper-util` and `hyper-rustls`
//! directly, removes the shared switch. (`docs/adr/0027`.)
//!
//! # Roots
//!
//! The service image is `FROM scratch` — no `/etc/ssl`, no CA bundle — so the client is built from a
//! `rustls::ClientConfig` whose root store is `webpki-roots`, compiled in, and nothing consults the
//! platform. That configuration would otherwise work in the dev shell and a NixOS host and fail only
//! inside the container, as a handshake error that reads like a provider outage.
//!
//! # What it will not do
//!
//! Follow a redirect (a status of 3xx is a failure: a discovery document that redirects is
//! misconfigured or being steered, and `https` to `http` is the one hop that must never be followed
//! silently), keep an idle connection (a fetch every few minutes at most, and a pooled connection
//! to a provider would outlive it), read more than [`MAX_BODY_BYTES`], or wait longer than the
//! timeout it was given.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::de::DeserializeOwned;

/// The most a discovery document or key set may weigh.
///
/// A real one is a few kilobytes. The cap exists because the body comes from the network, on a
/// path every cold request waits behind.
pub const MAX_BODY_BYTES: usize = 1 << 20;

/// Why a fetch produced nothing. Carries no URL and no body: it is logged, and a body is whatever
/// the other end chose to send.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("the request timed out")]
    Timeout,
    #[error("could not connect")]
    Connect,
    #[error("the request failed")]
    Transport,
    #[error("the server answered HTTP {0}")]
    Status(u16),
    #[error("the response was larger than {MAX_BODY_BYTES} bytes")]
    TooLarge,
    #[error("the response was not the JSON that was expected")]
    Malformed,
}

/// The compiled-in Mozilla roots.
pub fn webpki_roots() -> rustls::RootCertStore {
    rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

/// A TLS configuration that trusts exactly `roots`, on the aws-lc-rs provider.
///
/// `builder_with_provider` and not `ClientConfig::builder()`: the latter asks the process for a
/// default provider and **panics** when two are compiled in, which `sqlx`'s `ring` and this crate's
/// `aws-lc-rs` are.
///
/// # Errors
///
/// Returns an error if the provider supports no usable protocol version, which it does.
pub fn tls_config(roots: rustls::RootCertStore) -> Result<rustls::ClientConfig, rustls::Error> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    // ALPN is left empty: `HttpsConnectorBuilder` sets it from the versions enabled on it
    // (`enable_http1`, so `http/1.1` and nothing that is not spoken here) and panics if it finds one
    // already set.
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// The client, and the timeout every request through it is held to.
///
/// `Clone` is cheap: the inner hyper client is an `Arc`.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: HyperClient<HttpsConnector<HttpConnector>, Full<Bytes>>,
    pub(crate) timeout: Duration,
}

impl Client {
    /// A client that trusts exactly the roots in `tls`.
    ///
    /// `https_or_http`: plain `http://` is let through the connector because loopback issuers are
    /// (for tests and local development), and **which** URLs may be plain is decided by
    /// `check_endpoint` before a request is made — not here, where the scheme is all there is to see.
    #[must_use]
    pub fn new(tls: rustls::ClientConfig, timeout: Duration) -> Self {
        let connector = HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .build();
        let inner = HyperClient::builder(TokioExecutor::new())
            // No idle pool: see the module docs. It is also what lets a test take the provider down
            // and have the next fetch fail, rather than ride a connection that outlived it.
            .pool_max_idle_per_host(0)
            .build(connector);
        Self { inner, timeout }
    }

    /// GET `url` and parse the body as JSON, reading at most [`MAX_BODY_BYTES`], all within the
    /// client's timeout — connecting, the response head and the body together.
    pub async fn get_json<T: DeserializeOwned>(&self, url: &url::Url) -> Result<T, FetchError> {
        let uri: hyper::Uri = url.as_str().parse().map_err(|_| FetchError::Transport)?;
        let request = hyper::Request::get(uri)
            .body(Full::from(Bytes::new()))
            .map_err(|_| FetchError::Transport)?;

        // A timeout on the whole exchange, and not only on connecting: a provider that accepts and
        // then says nothing is the case this exists for, and it is what holds a request's wait to
        // the budget the service gave it (`timeouts::REQUEST`).
        let body = tokio::time::timeout(self.timeout, async {
            let response = self.inner.request(request).await.map_err(|error| {
                if error.is_connect() {
                    FetchError::Connect
                } else {
                    FetchError::Transport
                }
            })?;

            let status = response.status();
            if !status.is_success() {
                // Including 3xx: a redirect is not followed, so it is an answer we cannot use.
                return Err(FetchError::Status(status.as_u16()));
            }
            // The header is a hint the sender controls; the limit on the body below is the bound.
            if response
                .headers()
                .get(hyper::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|length| length > MAX_BODY_BYTES as u64)
            {
                return Err(FetchError::TooLarge);
            }

            let collected = Limited::new(response.into_body(), MAX_BODY_BYTES)
                .collect()
                .await
                .map_err(|error| {
                    if error
                        .downcast_ref::<http_body_util::LengthLimitError>()
                        .is_some()
                    {
                        FetchError::TooLarge
                    } else {
                        FetchError::Transport
                    }
                })?;
            Ok(collected.to_bytes())
        })
        .await
        .map_err(|_| FetchError::Timeout)??;

        serde_json::from_slice(&body).map_err(|_| FetchError::Malformed)
    }

    /// POST `url` with a form-urlencoded body and return the full response.
    ///
    /// Used by the `OAuth2` token and revocation exchanges. The caller is responsible for
    /// parsing — token responses carry error information in the body even on non-2xx statuses.
    #[cfg(any(feature = "bff", test))]
    #[allow(dead_code)]
    pub async fn post_form(&self, url: &str, body: &[u8]) -> Result<FormResponse, FetchError> {
        let uri: hyper::Uri = url.parse().map_err(|_| FetchError::Transport)?;
        let request = hyper::Request::post(uri)
            .header(
                hyper::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(Full::from(Bytes::copy_from_slice(body)))
            .map_err(|_| FetchError::Transport)?;

        tokio::time::timeout(self.timeout, async {
            let response = self.inner.request(request).await.map_err(|error| {
                if error.is_connect() {
                    FetchError::Connect
                } else {
                    FetchError::Transport
                }
            })?;

            let status = response.status();
            let resp_headers = response.headers().clone();

            let collected = Limited::new(response.into_body(), MAX_BODY_BYTES)
                .collect()
                .await
                .map_err(|error| {
                    if error
                        .downcast_ref::<http_body_util::LengthLimitError>()
                        .is_some()
                    {
                        FetchError::TooLarge
                    } else {
                        FetchError::Transport
                    }
                })?;
            Ok(FormResponse {
                status: status.as_u16(),
                headers: resp_headers,
                body: collected.to_bytes(),
            })
        })
        .await
        .map_err(|_| FetchError::Timeout)?
    }

    /// GET `url` with a `Bearer` authorization header, parse the body as JSON.
    #[cfg(any(feature = "bff", test))]
    #[allow(dead_code)]
    pub async fn get_json_bearer<T: DeserializeOwned>(
        &self,
        url: &str,
        token: &str,
    ) -> Result<T, FetchError> {
        let uri: hyper::Uri = url.parse().map_err(|_| FetchError::Transport)?;
        let request = hyper::Request::get(uri)
            .header(hyper::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Full::from(Bytes::new()))
            .map_err(|_| FetchError::Transport)?;

        let body = tokio::time::timeout(self.timeout, async {
            let response = self.inner.request(request).await.map_err(|error| {
                if error.is_connect() {
                    FetchError::Connect
                } else {
                    FetchError::Transport
                }
            })?;

            let status = response.status();
            if !status.is_success() {
                return Err(FetchError::Status(status.as_u16()));
            }

            let collected = Limited::new(response.into_body(), MAX_BODY_BYTES)
                .collect()
                .await
                .map_err(|error| {
                    if error
                        .downcast_ref::<http_body_util::LengthLimitError>()
                        .is_some()
                    {
                        FetchError::TooLarge
                    } else {
                        FetchError::Transport
                    }
                })?;
            Ok(collected.to_bytes())
        })
        .await
        .map_err(|_| FetchError::Timeout)??;

        serde_json::from_slice(&body).map_err(|_| FetchError::Malformed)
    }
}

/// The response from a form POST — status, headers, and raw body.
///
/// Token endpoints signal errors in the body on non-2xx statuses, so the caller parses.
#[derive(Debug)]
#[cfg(any(feature = "bff", test))]
#[allow(dead_code)]
pub struct FormResponse {
    status: u16,
    headers: hyper::HeaderMap,
    body: Bytes,
}

#[cfg(any(feature = "bff", test))]
#[allow(dead_code)]
impl FormResponse {
    /// The HTTP status code.
    #[must_use]
    pub(crate) const fn status(&self) -> u16 {
        self.status
    }

    /// The response headers.
    #[must_use]
    pub const fn headers(&self) -> &hyper::HeaderMap {
        &self.headers
    }

    /// The response body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_store_is_the_compiled_in_one() {
        // The platform has no store to count; this one has the Mozilla set, and an empty
        // `RootCertStore` — the failure mode of building it wrong — would trust nothing.
        let roots = webpki_roots();
        assert!(!roots.is_empty(), "an empty root store trusts no provider");
        assert_eq!(roots.roots.len(), webpki_roots::TLS_SERVER_ROOTS.len());
    }

    #[test]
    fn the_tls_configuration_leaves_alpn_to_the_connector() {
        let config = tls_config(webpki_roots()).expect("the default versions are supported");
        assert!(
            config.alpn_protocols.is_empty(),
            "hyper-rustls panics on a configuration that already names protocols"
        );
    }
}
