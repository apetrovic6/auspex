//! The HTTP client the Prometheus and narinfo adapters share.
//!
//! hyper-util's client over hyper-rustls, with rustls on the `ring` provider and Mozilla's
//! roots compiled in (`webpki-roots`), so it needs no system CA bundle and no OpenSSL.
//! HTTP/1 only. It does not follow redirects. Build one with [`HttpClient::new`] and share
//! it; it pools connections.

use std::error::Error as _;
use std::sync::Arc;
use std::time::Duration;

use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt as _, Empty, Limited};
use hyper::body::Bytes;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::Cancelled;

/// A shared HTTP(S) client.
#[derive(Clone, Debug)]
pub struct HttpClient {
    inner: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
}

impl HttpClient {
    /// A client that speaks `http` and `https`.
    ///
    /// # Errors
    ///
    /// When rustls refuses its own default protocol versions (it does not).
    pub fn new() -> Result<Self, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS configuration: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .build();
        Ok(Self {
            inner: Client::builder(TokioExecutor::new()).build(https),
        })
    }

    /// Sends one request without a body and reads at most `body_cap` bytes of the answer,
    /// all within `timeout`. Any failure is an `Err` with a reason.
    pub(crate) async fn send(
        &self,
        method: Method,
        url: &Url,
        accept: &str,
        timeout: Duration,
        body_cap: usize,
        cancel: &CancellationToken,
    ) -> Result<Result<(StatusCode, Bytes), String>, Cancelled> {
        self.send_call(
            &Call {
                method,
                url,
                accept,
                timeout,
                body_cap,
                authorization: None,
            },
            cancel,
        )
        .await
    }

    /// [`Self::send`] with the options that most callers do not need.
    pub(crate) async fn send_call(
        &self,
        call: &Call<'_>,
        cancel: &CancellationToken,
    ) -> Result<Result<(StatusCode, Bytes), String>, Cancelled> {
        let Call {
            method,
            url,
            accept,
            timeout,
            body_cap,
            authorization,
        } = call;
        let (timeout, body_cap) = (*timeout, *body_cap);
        let work = async {
            let uri: http::Uri = url
                .as_str()
                .parse()
                .map_err(|e| format!("bad URL {url}: {e}"))?;
            let mut builder = Request::builder()
                .method(method.clone())
                .uri(uri)
                .header(header::USER_AGENT, "auspex")
                .header(header::ACCEPT, *accept);
            if let Some(value) = authorization {
                builder = builder.header(header::AUTHORIZATION, (*value).clone());
            }
            let req = builder
                .body(Empty::<Bytes>::new())
                .map_err(|e| format!("bad request: {e}"))?;
            let resp = self.inner.request(req).await.map_err(|e| chain(&e))?;
            let status = resp.status();
            let body = Limited::new(resp.into_body(), body_cap)
                .collect()
                .await
                .map_err(|e| {
                    if e.downcast_ref::<http_body_util::LengthLimitError>()
                        .is_some()
                    {
                        format!("the answer is larger than {body_cap} bytes")
                    } else {
                        format!("reading the answer: {e}")
                    }
                })?
                .to_bytes();
            Ok((status, body))
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Cancelled),
            r = tokio::time::timeout(timeout, work) => Ok(r.unwrap_or_else(|_| {
                Err(format!("no answer within {} ms", timeout.as_millis()))
            })),
        }
    }
}

/// One request and its limits, for [`HttpClient::send_call`].
pub(crate) struct Call<'a> {
    pub method: Method,
    pub url: &'a Url,
    pub accept: &'a str,
    pub timeout: Duration,
    pub body_cap: usize,
    /// Sent as `Authorization`. Build it with `set_sensitive(true)`: its `Debug` is then
    /// `Sensitive`, in this crate's errors and in hyper's own.
    pub authorization: Option<&'a header::HeaderValue>,
}

/// An error and its sources, which is where hyper keeps the useful part ("connection
/// refused").
fn chain(e: &hyper_util::client::legacy::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

/// A base URL from configuration: `http` or `https` only, with a host, and without
/// credentials. Query and fragment are dropped; a path prefix is kept
/// (`https://host/prometheus`).
///
/// No error quotes the URL (review finding F8): a configured URL can carry a password, and
/// these errors are shown on screen and in `--json`. Credentials are refused rather than
/// silently dropped: hyper does not send `user:pass@` as basic auth, so such a URL would
/// only ever fail with a 401.
pub(crate) fn base_url(s: &str) -> Result<Url, String> {
    let mut url = Url::parse(s).map_err(|e| format!("bad URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "the URL must be http or https, not {}",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "the URL contains credentials (user:password@), which auspex does not send; \
             remove them"
                .to_owned(),
        );
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("the URL has no host".to_owned());
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// `base` with path segments appended (each percent-encoded by `url`).
pub(crate) fn join(base: &Url, segments: &[&str]) -> Url {
    let mut url = base.clone();
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().extend(segments);
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls() {
        assert!(base_url("ftp://x").is_err());
        assert!(base_url("file:///etc/passwd").is_err());
        assert!(base_url("not a url").is_err());
        let u = base_url("https://h:9090/prom/?q=1#f").unwrap();
        assert_eq!(
            join(&u, &["api", "v1", "query"]).as_str(),
            "https://h:9090/prom/api/v1/query"
        );
        let u = base_url("http://h").unwrap();
        assert_eq!(join(&u, &["a.narinfo"]).as_str(), "http://h/a.narinfo");
        // A segment cannot escape its place.
        assert_eq!(join(&u, &["../x?y"]).as_str(), "http://h/..%2Fx%3Fy");
    }
}
