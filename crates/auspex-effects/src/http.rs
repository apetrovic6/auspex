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
        let work = async {
            let uri: http::Uri = url
                .as_str()
                .parse()
                .map_err(|e| format!("bad URL {url}: {e}"))?;
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::USER_AGENT, "auspex")
                .header(header::ACCEPT, accept)
                .body(Empty::<Bytes>::new())
                .map_err(|e| format!("bad request: {e}"))?;
            let resp = self.inner.request(req).await.map_err(|e| chain(&e))?;
            let status = resp.status();
            let body = Limited::new(resp.into_body(), body_cap)
                .collect()
                .await
                .map_err(|e| format!("reading the answer: {e}"))?
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

/// A base URL from configuration: `http` or `https` only, with a host. Query and fragment
/// are dropped; a path prefix is kept (`https://host/prometheus`).
pub(crate) fn base_url(s: &str) -> Result<Url, String> {
    let mut url = Url::parse(s).map_err(|e| format!("bad URL {s:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "URL {s:?} must be http or https, not {}",
            url.scheme()
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(format!("URL {s:?} has no host"));
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
