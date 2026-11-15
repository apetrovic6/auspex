//! The seam between "an authenticated request" and "a handler".
//!
//! This crate answers one question — who is this request from — and refuses to answer any
//! other. It contains no identity provider, no token minting, no password storage and no
//! session persistence, and adding one is a mistake: authentication is the most
//! product-specific thing a template can contain, and a prebuilt one is the thing every
//! product rips out. What cannot be retrofitted is the *shape* of the boundary, which is
//! what is here. See `docs/adr/0012`.
//!
//! # The admission rule
//!
//! In: the identity type, the credential parse, the verifier trait, the layer that attaches
//! one, the accessor a handler uses, and the Connect mapping for "no identity" and for "we
//! could not determine one". Out:
//! anything that knows a provider's name, a token's format, a role's meaning or a session's
//! lifetime. A rule with no outside is not a rule, and this crate is small because the
//! outside is large.
//!
//! # The division of labour
//!
//! Load-bearing, and the reason a layer here cannot be replaced by a middleware that
//! rejects:
//!
//! - [`IdentityLayer`] **parses and attaches**. It reads a credential off the request, asks
//!   a [`VerifyCredential`] whose it is, and puts the answer in the request's extensions. It
//!   never rejects, because a layer that rejects has to know which methods are public — and
//!   that knowledge belongs with the method, not with the router.
//! - The **handler decides** whether it needs an identity, by calling
//!   [`IdentityContext::principal`]. Fail-closed: no extension, no identity, no principal —
//!   so a router that forgot to mount the layer refuses protected calls rather than serving
//!   them.
//!
//! Nothing here is configurable, because there is nothing to configure. A verifier is code,
//! not a key; a config section no code reads is a promise nobody keeps.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use connectrpc::{ConnectError, RequestContext};
use http::header::AUTHORIZATION;
use http::{HeaderMap, Request};
use tower::{Layer, Service};

/// A boxed future, so [`VerifyCredential`] can be a trait object.
///
/// Hand-rolled rather than pulled from `futures` or written with `async_trait`: a real
/// verifier does I/O (a JWKS fetch, a session lookup, an introspection call), so the trait
/// has to be async, and `dyn`-compatible async means a boxed future. Both of those routes
/// cost a dependency this crate does not otherwise want, for one trait.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The cause a verifier gives for whichever failure in [`VerifyFailure`] it means.
///
/// Boxed rather than an enum, because the cause belongs to the product: a JWKS fetch's HTTP
/// status, a Postgres error, a JWT library's expiry variant. This crate cannot name those
/// types, so it names the two things a cause can *mean* instead — [`VerifyFailure`].
///
/// Logged, never returned to the caller — see [`Unauthenticated`] and [`VerifierUnavailable`]
/// for what does go back.
pub type VerifyError = Box<dyn std::error::Error + Send + Sync>;

/// Why a verifier did not produce a principal, and **which kind of "not" it is**.
///
/// Two variants because a verifier that does I/O has two unrelated reasons to fail, and one
/// channel carrying both makes them indistinguishable on the wire: "this credential is not
/// valid" and "I could not reach the thing that would tell me" both arrived as a
/// [`VerifyError`] and both left as 401 `unauthenticated`. The first is a statement about the
/// caller, and it has to stay indistinguishable from never having sent a credential (ADR
/// 0012). The second is a statement about this service, and saying it out loud leaks nothing
/// about the credential.
///
/// Deliberately **no** `From<VerifyError>` impl. A blanket conversion would let an
/// implementation fail without ever deciding which fact it means, and the variant it happened
/// to get would be whichever one the compiler picked — the exact collapse this type exists to
/// remove. Naming the variant is the point of the split, not the price of it.
#[derive(Debug)]
pub enum VerifyFailure {
    /// The verifier reached a verdict and the verdict was no: expired, bad signature, unknown
    /// subject, a token minted for somebody else's audience.
    ///
    /// Becomes [`Identity::Anonymous`] and [`Unauthenticated`] on the wire, exactly as this
    /// type had one channel before. The cause is logged at `debug`, not `warn`: a
    /// token-guessing scan can present as many rejected credentials as it likes, and that must
    /// not be a way to fill a production log.
    Rejected(VerifyError),
    /// The verifier never reached a verdict: the JWKS fetch failed, the introspection call
    /// timed out, the session store was down.
    ///
    /// Becomes [`Identity::Indeterminate`] and [`VerifierUnavailable`] on the wire. This is not
    /// a statement about the caller's credential — it is an outage of a dependency this service
    /// chose — so it is logged at `warn` and answered with a server-side code.
    Unavailable(VerifyError),
}

/// A credential as it arrived, parsed but not judged.
///
/// [`Debug`](fmt::Debug) is hand-written to omit the token. That is not squeamishness about
/// a type nobody prints: it is about the `tracing::debug!(?credential)` somebody writes
/// three files away while debugging a 401, which would put a live bearer token in the log —
/// and this repository ships logs to whatever a product points them at.
///
/// Deliberately **not** `Clone`, and not `Copy`. Every copy of a live credential is a copy
/// that can end up in a log line, a core dump, or the `Debug` of some larger struct, and
/// [`VerifyCredential::verify`] takes this by value so the common case needs none. An
/// implementation that retries, or that keys a cache on the token, calls
/// [`Credential::token`] and makes its own `String` at that line — where the copy is visible
/// in review rather than spread by a derive.
pub struct Credential {
    /// The token, opaque. Whether it is a JWT, a session id or something else is the
    /// verifier's business; this crate knows only the scheme, because the header says so.
    token: String,
}

impl Credential {
    /// The token as it arrived.
    ///
    /// The one way out, and it exists for a product's [`VerifyCredential`] implementation,
    /// which can do nothing otherwise: a verifier that cannot read the credential is not an
    /// insertion point, it is a decoration. The field stays private so the hand-written
    /// [`Debug`](fmt::Debug) remains the only formatting path — a `pub` field would put
    /// `{:?}` back on the table for every struct that embeds this one, which is exactly the
    /// leak the redaction exists to prevent.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// `Authorization: Bearer <token>`, and nothing else.
    ///
    /// One scheme, because a second is a decision about a second product. An unrecognised
    /// scheme is [`None`], not an error: a browser extension's `Authorization: Negotiate`
    /// must not turn into a 401 on a public method. The scheme is matched
    /// case-insensitively because RFC 9110 §11.6.2 says it is case-insensitive, and a client
    /// that sends `bearer` failing silently is the invisible kind of bug.
    ///
    /// With several `Authorization` headers this takes the first, which is what
    /// [`HeaderMap::get`] returns and what everything else reading the header sees. A
    /// request carrying two is malformed; being clever about which one wins would only make
    /// the two disagree.
    fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
        let (scheme, rest) = value.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("Bearer") {
            return None;
        }
        let token = rest.trim();
        (!token.is_empty()).then(|| Self {
            token: token.to_owned(),
        })
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The token is the one thing this must not print, and a test below asserts it does
        // not — so the test fails the day somebody "simplifies" this into a derive.
        f.debug_struct("Credential")
            .field("token", &"<redacted>")
            .finish()
    }
}

/// An authenticated caller.
///
/// One field, deliberately. A product grows claims onto it the day it needs them; the
/// template has no opinion about what a user is, and any field it invented would be wrong
/// for most products and would have to be deleted rather than extended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// The subject the verifier named. Opaque to this crate.
    pub subject: String,
}

/// Who a request turned out to be from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    /// No credential, an unusable one, or one the verifier reached a verdict about and
    /// rejected. All three are the same value on purpose — see [`Unauthenticated`] for why the
    /// distinction never reaches the wire, and [`resolve`] for where each is logged.
    ///
    /// What is deliberately **not** here is the fourth case. A verifier that never reached a
    /// verdict says nothing about this caller's credential, so folding it in here would be a
    /// claim about the caller that the evidence does not support — that is
    /// [`Identity::Indeterminate`].
    Anonymous,
    /// The verifier accepted the credential.
    Authenticated(Principal),
    /// The verifier could not reach a verdict, so this request's identity is unknown rather
    /// than absent.
    ///
    /// The state has to travel in the request's extensions like the other two, because the
    /// code is produced later and elsewhere: [`IdentityContext::principal`] runs in the
    /// handler, and by then the verifier that failed is not in scope. A state that only existed
    /// inside [`resolve`] could only be reported as something else.
    Indeterminate,
}

impl Identity {
    /// Attach to a request's extensions.
    ///
    /// Overwrites rather than merges: `Extensions::insert` replaces any previous value and
    /// returns it, and the return is dropped here. A layer mounted twice by accident leaves
    /// one identity on the request, not two answers for a handler to reconcile.
    pub fn insert_into(self, extensions: &mut http::Extensions) {
        extensions.insert(self);
    }
}

/// A protected method's answer to a request that carries no identity.
///
/// A type rather than a `ConnectError::unauthenticated(..)` call at each site, so the message
/// lives in exactly one place and every protected method in every service says the same
/// thing. A literal at each call site is a literal that drifts.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unauthenticated;

/// The one message.
///
/// Constant, and identical for a missing credential, a malformed one and one the verifier
/// rejected: the difference is an oracle on the wire, and it is already in the log, where the
/// cause belongs. It also reaches the trace backend as `exception.message`, because
/// `telemetry::http_layer` reads the Connect error body to name the failure — which is the
/// mechanical reason this carries nothing but itself.
///
/// The fourth case — a verifier that reached no verdict — is the one the three-cause rule never
/// covered, and it gets its own constant (`VERIFIER_UNAVAILABLE_MESSAGE`) rather than this
/// one: sharing a message with "authentication required" would put a service outage in the
/// client-error series and tell the caller their credential is the problem.
const UNAUTHENTICATED_MESSAGE: &str = "authentication required";

impl From<Unauthenticated> for ConnectError {
    fn from(_: Unauthenticated) -> Self {
        Self::unauthenticated(UNAUTHENTICATED_MESSAGE)
    }
}

/// A protected method's answer to a request this service could not identify **because it could
/// not find out**, not because the credential was refused.
///
/// The sibling of [`Unauthenticated`], and who the failure belongs to is the difference between
/// them: that one is the caller's answer, this one is ours. A product's verifier that does I/O
/// fails whenever its dependency does, and a caller who presented a perfectly good credential
/// must not be told their credential is bad — a client that reacts to 401 by discarding its
/// token would re-authenticate against the same dead dependency.
///
/// Maps to `internal`, **not** to `unavailable`, and that is a wire-compatibility decision
/// rather than a taste one: `apps/counter/client` collapses `ErrorCode::Unavailable` into
/// `Failure::Unreachable { endpoint }` unconditionally, which renders as "cannot reach the API
/// at &lt;endpoint&gt;" in all five shells, because connectrpc-rust reports a *refused
/// connection* as `Unavailable` and connect-kotlin and connect-swift have parallel rules. A
/// verifier outage is not a connection failure — the request arrived, was routed, and reached a
/// handler — so `unavailable` would have the client describe our dependency as the caller's
/// network. `internal` already maps to `Code::ServerFault` and "the API had a problem with that
/// request", which is true, and needs no change in the client, the shells or the i18n strings.
#[derive(Clone, Copy, Debug, Default)]
pub struct VerifierUnavailable;

/// The one message for that case, constant for the same reason [`UNAUTHENTICATED_MESSAGE`] is:
/// it reaches the trace backend as `exception.message`, so it carries nothing but itself — not
/// the credential, and not anything the verifier said about why it could not answer.
const VERIFIER_UNAVAILABLE_MESSAGE: &str = "the service could not check that credential";

impl From<VerifierUnavailable> for ConnectError {
    fn from(_: VerifierUnavailable) -> Self {
        Self::internal(VERIFIER_UNAVAILABLE_MESSAGE)
    }
}

/// The one insertion point a product owns.
///
/// Implement this and pass it to the service's `build_router`; nothing else in the template
/// changes. Async because a real implementation does I/O, and `Send + Sync` because the layer
/// holding it runs inside a connection task that outlives any one request.
pub trait VerifyCredential: Send + Sync {
    /// Resolve a credential to a principal, or say **which kind of not** it is.
    ///
    /// # Errors
    ///
    /// [`VerifyFailure::Rejected`] means "this credential is not this caller's" — logged at
    /// `debug`, and the request carries [`Identity::Anonymous`] and answers
    /// [`Unauthenticated`]. [`VerifyFailure::Unavailable`] means "I did not reach a verdict" —
    /// logged at `warn`, and the request carries [`Identity::Indeterminate`] and answers
    /// [`VerifierUnavailable`].
    ///
    /// Nothing an implementation returns reaches the wire, so a cause is free to be specific in
    /// either arm. Which arm it is stays the implementation's call, and it has to be made: there
    /// is no conversion from a bare [`VerifyError`] precisely so that an implementation cannot
    /// fail without saying which fact it means.
    fn verify(&self, credential: Credential) -> BoxFuture<'_, Result<Principal, VerifyFailure>>;
}

/// The default: nobody is ever authenticated.
///
/// Shipped rather than left absent, so the failure direction is a decision on the page
/// instead of an accident of construction. With this in the router a protected method answers
/// [`Unauthenticated`] to every caller until a product supplies a real verifier — which is
/// the point of the seam, not a gap in it.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoIdentities;

impl VerifyCredential for NoIdentities {
    fn verify(&self, _credential: Credential) -> BoxFuture<'_, Result<Principal, VerifyFailure>> {
        Box::pin(async {
            // A refusal, not an outage. This verifier is not unreachable — it is absent by
            // policy — so the caller's answer is "you have no identity here" rather than "this
            // service could not check", and the demo of a service with no verifier stays in the
            // 401 series where a caller can act on it. Logged by `resolve`, never returned: the
            // wire message is `UNAUTHENTICATED_MESSAGE` whatever went wrong here.
            Err(VerifyFailure::Rejected(
                "this service has no credential verifier configured".into(),
            ))
        })
    }
}

/// Attaches an [`Identity`] to every request's extensions.
///
/// Mount it with axum's `Router::layer`, **not** `route_layer`: `route_layer` never reaches
/// the fallback router, and a Connect service is mounted as a fallback. On a router with no
/// routes of its own axum panics — "Adding a `route_layer` before any routes is a no-op" —
/// and the day that router grows a route, the panic becomes the no-op it describes and every
/// RPC is left with no identity and nothing failing.
///
/// Stateless apart from the verifier, and it never rejects: see the crate docs for why a
/// layer that decides which methods are public is the wrong shape.
#[derive(Clone)]
pub struct IdentityLayer {
    verifier: Arc<dyn VerifyCredential>,
}

impl IdentityLayer {
    /// A layer that resolves credentials with `verifier`.
    #[must_use]
    pub fn new(verifier: Arc<dyn VerifyCredential>) -> Self {
        Self { verifier }
    }
}

impl fmt::Debug for IdentityLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A `dyn` verifier has no useful `Debug`, and naming its type would say nothing a
        // reader could act on.
        f.debug_struct("IdentityLayer").finish_non_exhaustive()
    }
}

impl<S> Layer<S> for IdentityLayer {
    type Service = IdentityService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        IdentityService {
            inner,
            verifier: Arc::clone(&self.verifier),
        }
    }
}

/// The [`IdentityLayer`]'s service.
#[derive(Clone)]
pub struct IdentityService<S> {
    inner: S,
    verifier: Arc<dyn VerifyCredential>,
}

impl<S> fmt::Debug for IdentityService<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityService").finish_non_exhaustive()
    }
}

impl<S, B> Service<Request<B>> for IdentityService<S>
where
    S: Service<Request<B>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        // Cloned per call, the standard tower pattern: the caller keeps a ready-checked
        // service while the future owns its own copy.
        let mut inner = self.inner.clone();
        let verifier = Arc::clone(&self.verifier);

        Box::pin(async move {
            // Resolved before the body is touched: the layer reads headers only, so it needs
            // no body machinery and cannot disturb a streaming request.
            let identity = resolve(request.headers(), verifier.as_ref()).await;
            identity.insert_into(request.extensions_mut());
            inner.call(request).await
        })
    }
}

/// Parse the credential, if any, and ask the verifier whose it is.
///
/// Never fails. Every way this can go wrong lands on one of the three [`Identity`] values, and
/// which one is the whole policy: no header, a scheme this crate does not know, a value that is
/// not text and a verdict of no are all [`Identity::Anonymous`], because the layer's job is to
/// report what it found and the handler's job is to decide what that means. Only a verifier that
/// did not reach a verdict is [`Identity::Indeterminate`], because that is the one outcome that
/// is not a finding about the request. A layer that returned an error here would be a layer that
/// rejects, and then it would need to know which methods are public.
///
/// Split out of [`IdentityService::call`] so the whole policy is testable without a service, a
/// router or a socket.
async fn resolve(headers: &HeaderMap, verifier: &dyn VerifyCredential) -> Identity {
    // No credential: do not wake the verifier at all. A public RPC must not pay for a JWKS
    // fetch it has no use for, and an unauthenticated scan must not either.
    let Some(credential) = Credential::from_headers(headers) else {
        return Identity::Anonymous;
    };
    match verifier.verify(credential).await {
        Ok(principal) => Identity::Authenticated(principal),
        Err(VerifyFailure::Rejected(cause)) => {
            // The cause is logged; the credential is not, and neither is any part of it.
            // `Debug` on `Credential` is redacted for exactly the case where somebody widens
            // this line later.
            //
            // `debug`, and it stays that way: a scan that presents a thousand guessed tokens
            // can otherwise write a thousand lines into a production log, which turns the
            // cheapness of refusing them into a denial of service on the log itself.
            tracing::debug!(%cause, "credential rejected");
            Identity::Anonymous
        }
        Err(VerifyFailure::Unavailable(cause)) => {
            // `warn`, because this one is about us: our dependency is not answering, and every
            // caller holding a valid credential is about to be turned away. It is also the arm a
            // product's own outage shows up in, so it has to be visible at the level somebody is
            // watching when the error rate moves.
            tracing::warn!(%cause, "credential verifier could not reach a verdict");
            Identity::Indeterminate
        }
    }
}

/// Reading the identity out of a Connect handler's context.
///
/// An extension trait rather than a free function so the call site reads like the framework's
/// own: `ctx.principal()?`.
pub trait IdentityContext {
    /// The identity this request carries.
    ///
    /// [`Identity::Anonymous`] when the layer did not run, which is the fail-closed
    /// direction: forgetting to mount the layer makes protected methods refuse rather than
    /// serve. Note which way that falls — a router with no layer is a router that cannot
    /// answer, and it reports "no identity" rather than "we are broken", because the missing
    /// layer is a defect in *this* service that a test can catch, while
    /// [`Identity::Indeterminate`] is a state only a running verifier can produce.
    fn identity(&self) -> Identity;

    /// The principal behind this request, or the error a protected method returns.
    ///
    /// Owned rather than borrowed because `RequestContext::extensions` cannot lend a
    /// reference out of a function that also has to synthesise the [`Identity::Anonymous`]
    /// case. A `Principal` is one `String`, so the clone is not the thing to optimise.
    ///
    /// # Errors
    ///
    /// [`Unauthenticated`] when the request carries no identity, and [`VerifierUnavailable`]
    /// when the service could not determine one. Both messages are constants: neither
    /// distinguishes one history from another, and neither carries anything the verifier said.
    fn principal(&self) -> Result<Principal, ConnectError>;
}

impl IdentityContext for RequestContext {
    fn identity(&self) -> Identity {
        self.extensions()
            .get::<Identity>()
            .cloned()
            .unwrap_or(Identity::Anonymous)
    }

    fn principal(&self) -> Result<Principal, ConnectError> {
        match self.identity() {
            Identity::Authenticated(principal) => Ok(principal),
            Identity::Anonymous => Err(Unauthenticated.into()),
            // The third arm, and the reason `Identity` has a third variant: the verdict-or-no-
            // verdict distinction is made in the layer and has to survive to here, because this
            // is where the wire code is chosen and the verifier is long gone by then.
            Identity::Indeterminate => Err(VerifierUnavailable.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use connectrpc::ErrorCode;
    use http::{HeaderValue, Response};
    use tracing::subscriber::DefaultGuard;
    use tracing::{Level, Subscriber};

    use super::*;

    /// What a [`FakeVerifier`] does when it is asked.
    ///
    /// Three modes because [`VerifyFailure`] has two arms and a verifier that only has one of
    /// them cannot exercise the other: `Reject` is "I reached a verdict and it was no", which is
    /// the caller's problem, and `Unavailable` is "I never reached a verdict", which is ours.
    /// Collapsing the two into a bool — which is what this was — is how a test suite ends up
    /// proving only the half the shipped verifier can produce.
    #[derive(Clone, Copy, Default)]
    enum Mode {
        /// Accept the token [`FakeVerifier::accepting`] was given, refuse everything else.
        #[default]
        Accept,
        /// Refuse every credential, whatever it says.
        Reject,
        /// Never answer: the dependency a real verifier would call is down.
        Unavailable,
    }

    /// The credential the redaction tests present, and the cause a verifier gives when it cannot
    /// answer. Neither may reach the wire — see
    /// `the_unavailable_answer_carries_neither_the_credential_nor_the_cause`.
    const GUESSED_TOKEN: &str = "a-token-somebody-guessed";
    const UNAVAILABLE_CAUSE: &str = "the fake verifier could not reach the identity provider";

    /// A verifier that accepts one token and counts how often it is asked.
    #[derive(Default)]
    struct FakeVerifier {
        accepted: &'static str,
        calls: AtomicUsize,
        mode: Mode,
    }

    impl FakeVerifier {
        const fn accepting(token: &'static str) -> Self {
            Self {
                accepted: token,
                calls: AtomicUsize::new(0),
                mode: Mode::Accept,
            }
        }

        const fn rejecting() -> Self {
            Self {
                accepted: "",
                calls: AtomicUsize::new(0),
                mode: Mode::Reject,
            }
        }

        const fn unavailable() -> Self {
            Self {
                accepted: "",
                calls: AtomicUsize::new(0),
                mode: Mode::Unavailable,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl VerifyCredential for FakeVerifier {
        fn verify(
            &self,
            credential: Credential,
        ) -> BoxFuture<'_, Result<Principal, VerifyFailure>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mode = self.mode;
            let accepted = self.accepted;
            Box::pin(async move {
                match mode {
                    Mode::Accept if credential.token() == accepted => Ok(Principal {
                        subject: "subject-from-verifier".to_owned(),
                    }),
                    // Specific on purpose: nothing here may reach the wire, so an
                    // implementation is free to say exactly what went wrong.
                    Mode::Accept | Mode::Reject => {
                        Err(VerifyFailure::Rejected(REJECTED_CAUSE.into()))
                    }
                    // The other arm, and the one `NoIdentities` can never produce: a verdict
                    // that was not reached, rather than a verdict of no.
                    Mode::Unavailable => Err(VerifyFailure::Unavailable(UNAVAILABLE_CAUSE.into())),
                }
            })
        }
    }

    /// Drive one request through the layer and report the identity the inner service saw.
    ///
    /// A real tower service rather than a call into `resolve`, so the test covers the layer
    /// the router mounts — including that the extension is attached before the inner service
    /// runs, which is the ordering a handler depends on.
    async fn through_layer(
        request: Request<()>,
        verifier: Arc<dyn VerifyCredential>,
    ) -> Option<Identity> {
        let seen: Arc<Mutex<Option<Identity>>> = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&seen);
        let inner = tower::service_fn(move |request: Request<()>| {
            let captured = Arc::clone(&captured);
            async move {
                *captured.lock().expect("this test does not poison") =
                    request.extensions().get::<Identity>().cloned();
                Ok::<Response<()>, Infallible>(Response::new(()))
            }
        });

        let mut service = IdentityLayer::new(verifier).layer(inner);
        service
            .call(request)
            .await
            .expect("the inner service is infallible");
        seen.lock().expect("this test does not poison").clone()
    }

    /// A handler context carrying `identity`, which is what the layer leaves on a request.
    ///
    /// `with_extensions` is how connectrpc moves an `http::Request`'s extensions into a
    /// `RequestContext`, so this is the same shape a real request arrives with rather than a
    /// stand-in for one.
    fn context_with(identity: Identity) -> RequestContext {
        let mut extensions = http::Extensions::new();
        identity.insert_into(&mut extensions);
        RequestContext::new(HeaderMap::new()).with_extensions(extensions)
    }

    /// A request with one `Authorization` header.
    fn request_with(value: &str) -> Request<()> {
        let mut request = Request::new(());
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(value).expect("the test's header values are ASCII"),
        );
        request
    }

    /// What a [`FakeVerifier`] says when it refuses, so the log tests can name it.
    const REJECTED_CAUSE: &str = "the fake verifier did not recognise that token";

    /// One recorded event: the level it was emitted at, and everything it said.
    type Logged = Arc<Mutex<Vec<(Level, String)>>>;

    /// A subscriber that keeps every event this crate emits, with its level.
    ///
    /// Because the level is half of what the split was for, and nothing else in the tree would
    /// notice it move. `debug` for a refusal is deliberate — a scan presenting a thousand guessed
    /// tokens must not write a thousand lines into a production log — and `warn` for a verifier
    /// that reached no verdict is the reason that arm exists: a deployment runs at info or above,
    /// so the `debug!` line this replaced meant an outage of our own dependency left no trace
    /// anywhere. Both are one macro away from being wrong again.
    ///
    /// Hand-rolled rather than `tracing_subscriber` with a buffer, because this crate carries four
    /// dependencies on purpose and recording a level needs nothing beyond `tracing` itself.
    #[derive(Debug, Default)]
    struct LoggedEvents {
        events: Logged,
    }

    impl LoggedEvents {
        /// A handle to the recording, taken before the recorder is installed: `set_default` moves
        /// it, and the assertion is made afterwards.
        #[must_use]
        fn handle(&self) -> Logged {
            Arc::clone(&self.events)
        }
    }

    /// One event, rendered as `"message field=value …"`.
    ///
    /// The fields matter as much as the message: the cause is `%cause`, which is a field and not
    /// part of the message, and "the cause reached the log" is the half of ADR 0012's redaction
    /// rule that only holds if somewhere does have it.
    struct Capture(Option<String>);

    impl tracing::field::Visit for Capture {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let rendered = if field.name() == "message" {
                format!("{value:?}")
            } else {
                format!("{} {value:?}", field.name())
            };
            match &mut self.0 {
                Some(line) => *line = format!("{line} {rendered}"),
                None => self.0 = Some(rendered),
            }
        }
    }

    impl Subscriber for LoggedEvents {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            // Every event, at every level: the assertion is about *which* level each arm chose,
            // and a filter here would be the thing that decides the answer.
            true
        }

        // The span half is empty on purpose. `Subscriber` has no defaults for it, and this crate
        // emits events, not spans — the spans on a request belong to `RpcLayer` and
        // `telemetry::http_layer`, in a nest this crate's tests do not build.
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut capture = Capture(None);
            event.record(&mut capture);
            self.events
                .lock()
                .expect("no test poisons this")
                .push((*event.metadata().level(), capture.0.unwrap_or_default()));
        }
    }

    /// Install `recorder` as this thread's subscriber for the rest of the test.
    ///
    /// Thread-local, and `#[tokio::test]` runs the whole future on the test thread, which is where
    /// `resolve` emits its event. The guard restores the previous subscriber on drop.
    fn record_logging(recorder: LoggedEvents) -> (Logged, DefaultGuard) {
        let events = recorder.handle();
        let guard = tracing::subscriber::set_default(recorder);
        (events, guard)
    }

    /// The lines logged at `level`, out of a recording.
    fn logged_at(recording: &[(Level, String)], level: Level) -> Vec<String> {
        recording
            .iter()
            .filter(|(recorded, _)| *recorded == level)
            .map(|(_, line)| line.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_bearer_header_becomes_an_identity_in_the_extensions() {
        let verifier = Arc::new(FakeVerifier::accepting("good-token"));
        let identity = through_layer(
            request_with("Bearer good-token"),
            Arc::clone(&verifier) as Arc<dyn VerifyCredential>,
        )
        .await;

        assert_eq!(
            identity,
            Some(Identity::Authenticated(Principal {
                subject: "subject-from-verifier".to_owned()
            })),
            "the verifier's answer must be what the handler finds, not the raw token"
        );
        assert_eq!(verifier.calls(), 1);
    }

    #[tokio::test]
    async fn a_request_with_no_credential_never_wakes_the_verifier() {
        let verifier = Arc::new(FakeVerifier::accepting("good-token"));
        let identity = through_layer(
            Request::new(()),
            Arc::clone(&verifier) as Arc<dyn VerifyCredential>,
        )
        .await;

        assert_eq!(identity, Some(Identity::Anonymous));
        assert_eq!(
            verifier.calls(),
            0,
            "a public RPC must not pay for a verification it has no use for, and neither \
             must an unauthenticated scan"
        );
    }

    #[tokio::test]
    async fn a_rejected_credential_is_anonymous_and_says_nothing() {
        let verifier = Arc::new(FakeVerifier::rejecting());
        let identity = through_layer(
            request_with("Bearer a-token-somebody-guessed"),
            Arc::clone(&verifier) as Arc<dyn VerifyCredential>,
        )
        .await;

        assert_eq!(identity, Some(Identity::Anonymous));
        assert_eq!(verifier.calls(), 1, "it was asked, and refused");
        // The refusal is indistinguishable from never having sent one — on the wire that is
        // the same code and the same constant message either way.
        let error = RequestContext::new(HeaderMap::new())
            .principal()
            .expect_err("an anonymous context has no principal");
        assert_eq!(error.code, ErrorCode::Unauthenticated);
        assert_eq!(error.message.as_deref(), Some(UNAUTHENTICATED_MESSAGE));
    }

    #[tokio::test]
    async fn a_verifier_that_reached_no_verdict_is_not_anonymous() {
        // The fourth case, and the reason `Identity` grew a third variant: this request did have
        // a credential, and this service did ask about it. It just did not find out. Reporting
        // that as `Anonymous` is what made a dependency outage look like a bad credential.
        let verifier = Arc::new(FakeVerifier::unavailable());
        let identity = through_layer(
            request_with(&format!("Bearer {GUESSED_TOKEN}")),
            Arc::clone(&verifier) as Arc<dyn VerifyCredential>,
        )
        .await;

        assert_eq!(identity, Some(Identity::Indeterminate));
        assert_eq!(verifier.calls(), 1, "it was asked, and could not answer");
    }

    #[tokio::test]
    async fn an_indeterminate_identity_answers_internal_with_a_constant_message() {
        // Driven through the layer rather than constructed, because the thing being asserted is
        // the whole path: the state has to survive from the verifier that failed to the handler
        // that chooses the code, and a test that built `Identity::Indeterminate` by hand would
        // not prove the layer can produce it.
        let verifier = Arc::new(FakeVerifier::unavailable());
        let identity = through_layer(
            request_with(&format!("Bearer {GUESSED_TOKEN}")),
            verifier as Arc<dyn VerifyCredential>,
        )
        .await
        .expect("the layer always attaches an identity");
        assert_eq!(identity, Identity::Indeterminate);

        let error = context_with(identity)
            .principal()
            .expect_err("an identity nobody determined is not a principal");

        // `internal`, not `unavailable`: `apps/counter/client` maps `Unavailable` to "cannot
        // reach the API" in every shell, and a verifier's dead dependency is not the caller's
        // network. `internal` is already "the API had a problem with that request", which is
        // what happened. See `docs/adr/0012`.
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(
            error.message.as_deref(),
            Some(VERIFIER_UNAVAILABLE_MESSAGE),
            "one message for this case too, and none of the caller's"
        );
    }

    #[test]
    fn the_unavailable_answer_carries_neither_the_credential_nor_the_cause() {
        // The mirror of `a_credentials_debug_does_not_contain_the_token`, for the new channel.
        // `VerifierUnavailable` is a unit struct, so it *cannot* carry a cause — but the
        // assertion is what keeps it that way the day somebody widens it into one, and
        // `telemetry::http_layer` exports the Connect message as `exception.message`, so a cause
        // that leaked into the message would land in whatever trace backend a product runs.
        let error = context_with(Identity::Indeterminate)
            .principal()
            .expect_err("an identity nobody determined is not a principal");

        let rendered = format!("{error:?}");
        assert!(
            !rendered.contains(UNAVAILABLE_CAUSE),
            "the verifier's reason for not answering reached the wire: {rendered}"
        );
        assert!(
            !rendered.contains(GUESSED_TOKEN),
            "the credential reached the wire: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_verifier_that_reached_no_verdict_is_logged_at_warn_with_its_cause() {
        // The other half of the split, and the half no assertion about the wire can make. The
        // complaint this whole change answers is that a product's own outage left nothing in a log
        // a deployment actually emits: `debug` is below what anybody runs at, so the line this arm
        // replaced was not a place anybody could look.
        let (events, _guard) = record_logging(LoggedEvents::default());
        through_layer(
            request_with(&format!("Bearer {GUESSED_TOKEN}")),
            Arc::new(FakeVerifier::unavailable()),
        )
        .await;

        let recording = events.lock().expect("no test poisons this").clone();
        let warned = logged_at(&recording, Level::WARN);
        assert_eq!(warned.len(), 1, "the whole recording: {recording:?}");
        assert!(
            warned[0].contains(UNAVAILABLE_CAUSE),
            "the cause is the only thing this arm has to say, and it is what a deployment acts \
             on: {}",
            warned[0]
        );
        // Redaction is not arm-specific. The wire may not carry the credential; neither may the
        // log, which is one hop from whatever a product ships that log to.
        assert!(
            !warned[0].contains(GUESSED_TOKEN),
            "the credential reached the log: {}",
            warned[0]
        );
    }

    #[tokio::test]
    async fn a_rejected_credential_is_still_logged_at_debug() {
        // The pair, because a level is decided in two directions and only one of them is the
        // interesting one. Lifting a refusal to `warn` would be the same defect inverted: a scan
        // presenting a thousand guessed tokens a second writes a thousand lines a second into a
        // production log, which turns refusing them cheaply into a denial of service on the log
        // itself. That is why this arm keeps `debug` and why keeping it has to be asserted rather
        // than left alone because nobody touched it.
        let (events, _guard) = record_logging(LoggedEvents::default());
        through_layer(
            request_with(&format!("Bearer {GUESSED_TOKEN}")),
            Arc::new(FakeVerifier::rejecting()),
        )
        .await;

        let recording = events.lock().expect("no test poisons this").clone();
        assert!(
            logged_at(&recording, Level::WARN).is_empty(),
            "a refusal is not an outage, and must not be loud: {recording:?}"
        );
        let debug = logged_at(&recording, Level::DEBUG);
        assert_eq!(debug.len(), 1, "the whole recording: {recording:?}");
        assert!(
            debug[0].contains(REJECTED_CAUSE),
            "the cause belongs in the log, which is where ADR 0012 says it goes: {}",
            debug[0]
        );
        assert!(
            !debug[0].contains(GUESSED_TOKEN),
            "the credential reached the log: {}",
            debug[0]
        );
    }

    #[tokio::test]
    async fn no_credential_shape_authenticates() {
        let verifier = Arc::new(FakeVerifier::accepting("good-token"));
        for value in [
            "Token abc",           // a scheme this crate does not know
            "Bearer",              // no token at all
            "Bearer ",             // an empty one
            "Bearer    ",          // whitespace only
            "basic Zm9vOmJhcg==",  // a different scheme entirely
            "",                    // an empty header
            "Bearer good-token x", // trailing junk after a valid token
        ] {
            let identity = through_layer(
                request_with(value),
                Arc::clone(&verifier) as Arc<dyn VerifyCredential>,
            )
            .await;
            assert_eq!(
                identity,
                Some(Identity::Anonymous),
                "{value:?} must not authenticate"
            );
        }
    }

    #[tokio::test]
    async fn a_credential_that_is_not_text_is_anonymous() {
        let verifier = Arc::new(FakeVerifier::accepting("good-token"));
        let mut request = Request::new(());
        // A header value can hold any byte; `to_str` refuses the ones that are not visible
        // ASCII, and the layer must treat that as "no credential" rather than panic.
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff\xfe").expect("a valid header value"),
        );

        let identity = through_layer(request, verifier as Arc<dyn VerifyCredential>).await;
        assert_eq!(identity, Some(Identity::Anonymous));
    }

    #[tokio::test]
    async fn the_scheme_is_matched_case_insensitively() {
        let verifier = Arc::new(FakeVerifier::accepting("good-token"));
        let identity = through_layer(
            request_with("bearer good-token"),
            verifier as Arc<dyn VerifyCredential>,
        )
        .await;

        assert!(
            matches!(identity, Some(Identity::Authenticated(_))),
            "RFC 9110 §11.6.2: a client that lowercases the scheme must not be silently \
             anonymous, which is the invisible kind of failure"
        );
    }

    #[tokio::test]
    async fn two_authorization_headers_do_not_panic_or_authenticate_twice() {
        let verifier = Arc::new(FakeVerifier::accepting("good-token"));
        let mut request = Request::new(());
        let value = HeaderValue::from_static("Bearer good-token");
        request.headers_mut().append(AUTHORIZATION, value.clone());
        request.headers_mut().append(AUTHORIZATION, value);

        let identity =
            through_layer(request, Arc::clone(&verifier) as Arc<dyn VerifyCredential>).await;
        assert_eq!(verifier.calls(), 1, "one request, one verification");
        assert!(matches!(identity, Some(Identity::Authenticated(_))));
    }

    #[test]
    fn a_credentials_debug_does_not_contain_the_token() {
        // The leak test. `Credential`'s `Debug` is hand-written for this, and the crate doc's
        // argument about `tracing::debug!(?credential)` is only true while this passes.
        let credential = Credential {
            token: "a-token-that-must-not-appear".to_owned(),
        };

        let rendered = format!("{credential:?}");
        assert!(
            !rendered.contains("a-token-that-must-not-appear"),
            "{rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn a_context_with_no_identity_at_all_refuses() {
        // The layer was never mounted, or the request never passed through it. Fail closed:
        // a protected method must refuse, not serve, and nothing about the router can make
        // that choice silently the other way.
        let ctx = RequestContext::new(HeaderMap::new());

        assert_eq!(ctx.identity(), Identity::Anonymous);
        let error = ctx
            .principal()
            .expect_err("no extension means no principal");
        assert_eq!(error.code, ErrorCode::Unauthenticated);
        assert_eq!(
            error.message.as_deref(),
            Some(UNAUTHENTICATED_MESSAGE),
            "the message is a constant, and carries nothing about why"
        );
    }

    #[test]
    fn an_anonymous_identity_is_indistinguishable_from_a_rejected_one() {
        // The no-oracle property, stated as a value: three different histories, one answer.
        let rejected = Identity::Anonymous;
        let missing = Identity::Anonymous;
        assert_eq!(rejected, missing);

        let error = ConnectError::from(Unauthenticated);
        assert_eq!(error.code, ErrorCode::Unauthenticated);
        assert_eq!(error.message.as_deref(), Some(UNAUTHENTICATED_MESSAGE));
    }

    #[test]
    fn attaching_twice_leaves_one_identity() {
        let mut extensions = http::Extensions::new();
        Identity::Authenticated(Principal {
            subject: "first".to_owned(),
        })
        .insert_into(&mut extensions);
        Identity::Anonymous.insert_into(&mut extensions);

        assert_eq!(
            extensions.get::<Identity>(),
            Some(&Identity::Anonymous),
            "a layer mounted twice must leave one answer, not two to reconcile"
        );
    }
}
