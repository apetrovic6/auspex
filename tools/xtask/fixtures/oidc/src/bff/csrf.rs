//! CSRF protection for the BFF's POST endpoints.
//!
//! Two checks, tried in order:
//!
//! 1. `Sec-Fetch-Site: same-origin` — the Fetch Metadata header modern browsers send. It
//!    cannot be forged by a cross-site request because the browser controls it.
//! 2. When absent (older browsers, non-browser clients), an `Origin` header that equals the
//!    configured public origin. If neither header is present with a cross-site value, a request
//!    with no `Origin` from a non-browser client is also rejected.
//!
//! Either check passing is enough. A request that fails CSRF gets a 403 with no body.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// axum middleware that checks CSRF on POST requests.
pub async fn csrf_middleware(
    State(public_origin): State<String>,
    request: Request,
    next: Next,
) -> Response {
    if csrf_check_passes(request.headers(), &public_origin) {
        next.run(request).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

fn csrf_check_passes(headers: &HeaderMap, public_origin: &str) -> bool {
    // 1. Fetch Metadata: `Sec-Fetch-Site: same-origin` means this is a same-origin request.
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return site == "same-origin";
    }

    // 2. When absent, check the `Origin` header.
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        return origin == public_origin;
    }

    // No Sec-Fetch-Site and no Origin: reject.
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers() -> axum::http::HeaderMap {
        axum::http::HeaderMap::new()
    }

    #[test]
    fn same_origin_sec_fetch_site_passes() {
        let mut h = headers();
        h.insert("sec-fetch-site", "same-origin".parse().unwrap());
        let uri = "https://example.com";
        assert!(csrf_check_passes(&h, uri));
    }

    #[test]
    fn cross_site_sec_fetch_site_fails() {
        let mut h = headers();
        h.insert("sec-fetch-site", "cross-site".parse().unwrap());
        let uri = "https://example.com";
        assert!(!csrf_check_passes(&h, uri));
    }

    #[test]
    fn matching_origin_passes_when_no_sec_fetch_site() {
        let mut h = headers();
        h.insert("origin", "https://example.com".parse().unwrap());
        let uri = "https://example.com";
        assert!(csrf_check_passes(&h, uri));
    }

    #[test]
    fn foreign_origin_fails() {
        let mut h = headers();
        h.insert("origin", "https://evil.com".parse().unwrap());
        let uri = "https://example.com";
        assert!(!csrf_check_passes(&h, uri));
    }

    #[test]
    fn neither_header_fails() {
        let h = headers();
        let uri = "https://example.com";
        assert!(!csrf_check_passes(&h, uri));
    }

    #[test]
    fn cross_site_sec_fetch_wins_over_good_origin() {
        let mut h = headers();
        h.insert("sec-fetch-site", "cross-site".parse().unwrap());
        h.insert("origin", "https://example.com".parse().unwrap());
        let uri = "https://example.com";
        assert!(!csrf_check_passes(&h, uri));
    }
}
