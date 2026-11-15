//! Driving the real HTTP stack without a socket or a database.
//!
//! Two ways in, and the choice is deliberate.
//!
//! [`client_transport`] is the default: it drives the **generated Connect client** against
//! the real router, so a test asserts what an actual caller sees — typed responses and
//! `ConnectError`s — rather than a particular JSON encoding. It also exercises the client
//! half of the generated code, which is code this repository ships and would otherwise
//! have no coverage at all. `ClientConfig` defaults to protobuf, so this path covers the
//! binary codec rather than only JSON.
//!
//! [`raw_call`] exists for the cases a typed client cannot express by construction: calling
//! a method that does not exist, or sending to a path outside the API prefix. The client
//! derives both from the contract, which is exactly why it cannot be used to test what
//! happens when they are wrong.
//!
//! Neither touches a socket. `ServiceTransport` wraps any tower service, and an
//! `axum::Router` is one.

use std::num::NonZeroU32;
use std::sync::Arc;

use api::config::DatabaseSettings;
use api::db;
use api::startup::{Reflection, Web, build_router};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, Uri};
use connectrpc::client::{ClientConfig, ServiceTransport};
use http_body_util::BodyExt as _;
use identity::{BoxFuture, Credential, Principal, VerifyCredential, VerifyFailure};
use serde_json::Value;
use tower::ServiceExt as _;

/// The credential these tests present.
///
/// A literal in a test file, not a secret: nothing outside `tests/` accepts it, because the
/// service ships [`identity::NoIdentities`] and this verifier is built here. It exists so the
/// authenticated path can be driven through the real router with no provider and no key
/// anywhere in the repository.
pub const TOKEN: &str = "test-token";

/// The subject the test verifier names for [`TOKEN`].
pub const SUBJECT: &str = "test-subject";

/// What the test verifier says when it refuses, which must never reach the wire.
pub const REJECTION: &str = "the test verifier accepts exactly one token";

/// What the test verifier says when it **cannot answer**, which must never reach the wire
/// either — and which is a different fact from [`REJECTION`].
///
/// A JWKS fetch that failed, a session lookup against a database that is not there, an
/// introspection call that timed out. The whole reason `VerifyFailure` was split: this is a
/// statement about the service, not about the credential, and before the split it left the
/// building as 401 "authentication required" like any other refusal.
pub const UNAVAILABLE_CAUSE: &str = "the test verifier could not reach the identity provider";

/// A verifier that accepts exactly [`TOKEN`] and refuses everything else.
///
/// Lives here rather than in one test so that every test drives the same stack: the router is
/// the real one, and the only thing faked is the credential policy — which is the part a
/// product owns and the part these tests are about.
struct TestVerifier;

impl VerifyCredential for TestVerifier {
    fn verify(&self, credential: Credential) -> BoxFuture<'_, Result<Principal, VerifyFailure>> {
        let accepted = credential.token() == TOKEN;
        Box::pin(async move {
            if accepted {
                Ok(Principal {
                    subject: SUBJECT.to_owned(),
                })
            } else {
                // A verdict, and the verdict is no — so `Rejected`, which is the arm that keeps
                // today's behaviour byte for byte. `tests/api/auth.rs` asserts this string
                // appears in no response, which is what keeps a verifier's complaint in the log.
                Err(VerifyFailure::Rejected(REJECTION.into()))
            }
        })
    }
}

/// A verifier that never reaches a verdict.
///
/// The case the shipped `NoIdentities` cannot produce, and therefore the case no test in this
/// repository exercised until `VerifyFailure` grew the arm: a product's verifier does I/O, and
/// the first time its dependency is down every caller holding a valid credential is told their
/// credential is bad. This is that outage, with no dependency to be down.
struct UnavailableVerifier;

impl VerifyCredential for UnavailableVerifier {
    fn verify(&self, _credential: Credential) -> BoxFuture<'_, Result<Principal, VerifyFailure>> {
        // No credential comparison, because the point is that this verifier never got as far as
        // making one.
        Box::pin(async { Err(VerifyFailure::Unavailable(UNAVAILABLE_CAUSE.into())) })
    }
}

/// The test verifier, as the router wants it.
#[must_use]
pub fn test_verifier() -> Arc<dyn VerifyCredential> {
    Arc::new(TestVerifier)
}

/// A verifier that cannot answer, as the router wants it.
#[must_use]
pub fn unavailable_verifier() -> Arc<dyn VerifyCredential> {
    Arc::new(UnavailableVerifier)
}

/// A pool pointed at a database that is never contacted.
///
/// The pool is lazy, so nothing connects until a handler issues a query. That makes every
/// route which does not touch the database testable with no Postgres anywhere, which is what
/// lets these run in CI unconditionally. Split out of [`router`] because the routing-table
/// test needs the same pool without a router around it.
pub fn dead_pool() -> sea_orm::DatabaseConnection {
    let settings = DatabaseSettings {
        // The structured shape, which is what a development checkout uses. The URL shape has
        // its own test; a pool whose URL is assembled from parts is this one.
        url: String::new(),
        host: "127.0.0.1".to_owned(),
        // Port 1 is never a database. Chosen so a test that accidentally depends on one
        // fails loudly instead of quietly using a real one.
        port: 1,
        username: "unused".to_owned(),
        password: String::new(),
        database_name: "unused".to_owned(),
        max_connections: NonZeroU32::new(1).expect("1 is non-zero"),
    };
    db::connect(&settings).expect("the connection URL should be valid")
}

/// The real router, with [`TestVerifier`] resolving credentials and reflection served.
///
/// Reflection stays **on** here deliberately, although off is what a deployment gets: every
/// other helper routes through this router, so flipping it to the production default would
/// leave the rest of the suite quietly proving something else. Withheld has its own
/// constructor below.
pub fn router() -> Router {
    build_router(
        dead_pool(),
        test_verifier(),
        Reflection::Served,
        Web::Absent,
    )
}

/// [`router`] as a deployment gets it: reflection withheld.
///
/// Separate rather than a parameter on [`router`], so that no existing call site changes
/// meaning and the default any new test reaches for is still the stack the rest assume.
pub fn router_without_reflection() -> Router {
    build_router(
        dead_pool(),
        test_verifier(),
        Reflection::Withheld,
        Web::Absent,
    )
}

/// [`router`], serving a built single-page application from `web`.
///
/// A constructor rather than a parameter on [`router`], for the reason
/// [`router_without_reflection`] is one: the SPA changes what an unknown root path answers,
/// which every other test's assumptions depend on it *not* doing.
pub fn router_with_web(web: Web) -> Router {
    build_router(dead_pool(), test_verifier(), Reflection::Served, web)
}

/// [`router`], with a verifier that reaches no verdict.
///
/// The outage case, and the only way to drive it through the real nest: `build_router` takes the
/// verifier as an argument, so there is no layer on top of an existing router that could swap
/// the policy out. Everything else in the stack is the same router the rest of the suite uses —
/// same nest, same timeouts, same handler — with a verifier that cannot answer.
pub fn router_with_unavailable_verifier() -> Router {
    build_router(
        dead_pool(),
        unavailable_verifier(),
        Reflection::Served,
        Web::Absent,
    )
}

/// Transport that sends generated-client requests straight into the router.
pub fn client_transport() -> ServiceTransport<Router> {
    ServiceTransport::new(router())
}

/// Client configuration pointing at the API prefix.
///
/// The authority is arbitrary — nothing resolves it, because the transport never leaves
/// the process — but it must be present for a valid absolute URI. The path prefix is what
/// matters: the client appends `/package.Service/Method` to it, and the router nests the
/// services under the same prefix.
pub fn client_config() -> ClientConfig {
    ClientConfig::new(
        format!("http://in-process{}", api::API_PREFIX)
            .parse::<Uri>()
            .expect("a valid base URI"),
    )
}

/// [`client_config`], presenting [`TOKEN`] on every call.
///
/// A default header rather than a wrapped transport: this is the hook a shell with a fixed
/// credential uses (`ClientConfig::with_default_header`), so the test drives the same path a
/// product would. A credential that changes between calls would use
/// `CallOptions::with_header` on the generated `*_with_options` methods instead.
pub fn authorized_client_config() -> ClientConfig {
    client_config().with_default_header(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {TOKEN}")).expect("the test token is ASCII"),
    )
}

/// Issue a raw HTTP request and return the status and decoded JSON body.
///
/// For assertions about things the generated client cannot say — an unknown method, a path
/// outside the prefix. Prefer [`client_transport`] for anything a real caller would do.
pub async fn raw_call(path: &str, body: &str) -> (StatusCode, Value) {
    raw_call_with_headers(path, body, HeaderMap::new()).await
}

/// [`raw_call`] against a router the caller built.
///
/// For a test whose subject is a different *configuration* of the stack rather than a
/// different request. Everything else goes through [`raw_call`], so "the router" means one
/// thing across the suite.
pub async fn raw_call_to(router: Router, path: &str, body: &str) -> (StatusCode, Value) {
    call(router, path, body, HeaderMap::new()).await
}

/// [`raw_call`] with an `Authorization` header, for the cases where *what the caller
/// presented* is the thing under test — a rejected credential, a malformed one.
pub async fn raw_call_authorized(
    path: &str,
    body: &str,
    authorization: &str,
) -> (StatusCode, Value) {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(authorization).expect("a test header value is ASCII"),
    );
    raw_call_with_headers(path, body, headers).await
}

/// [`raw_call_to`] with an `Authorization` header.
///
/// The combination the outage case needs: a *different router* and a *presented credential*, and
/// a credential is mandatory here rather than incidental — the layer does not wake a verifier
/// that has nothing to ask about, so a router whose verifier is down answers `unauthenticated`
/// to an anonymous caller exactly as the healthy one does.
pub async fn raw_call_to_authorized(
    router: Router,
    path: &str,
    body: &str,
    authorization: &str,
) -> (StatusCode, Value) {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(authorization).expect("a test header value is ASCII"),
    );
    call(router, path, body, headers).await
}

/// Issue a plain `GET` against a router and return the status and the body as text.
///
/// The probes are the only routes in this stack that are not Connect methods, so this is the
/// second way into the same router the rest of the suite drives. Text rather than JSON: a
/// probe's body is for whoever is reading `curl` output, and the assertion that matters is
/// the status code.
pub async fn get_call(router: Router, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request should be well formed");

    let response = router
        .oneshot(request)
        .await
        .expect("the router is infallible");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body should be readable")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn raw_call_with_headers(path: &str, body: &str, headers: HeaderMap) -> (StatusCode, Value) {
    call(router(), path, body, headers).await
}

async fn call(router: Router, path: &str, body: &str, headers: HeaderMap) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .expect("request should be well formed");
    // `http::request::Builder` has no consuming `headers(..)`, so the extras go on after the
    // body is attached rather than into the builder.
    request.headers_mut().extend(headers);

    let response = router
        .oneshot(request)
        .await
        .expect("the router is infallible");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body should be readable")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}
