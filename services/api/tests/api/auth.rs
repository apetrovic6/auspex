//! The identity seam, wired into the real router.
//!
//! `crates/identity`'s own tests drive the layer over a synthetic tower service; these drive
//! it over `build_router` — the real axum nest, the real `ConnectRpcService`, the real
//! handlers — which is what proves the layer is mounted where a request actually hits it and
//! that its extension survives the nest into a handler's `RequestContext`. A layer mounted
//! with `route_layer` instead of `layer`, or not mounted at all, passes those tests and fails
//! here.
//!
//! Two halves. The behaviour of one protected method and one public one, and then the routing
//! table itself: every method this service registers must be classified, because an
//! unclassified method is one nobody thought about, and the default for "nobody thought about
//! it" must not be "public".
//!
//! The protected-method half has two arms, because a verifier fails two different ways. A
//! credential the verifier refused is indistinguishable from no credential at all — that is the
//! no-oracle rule, and `crates/identity`'s own tests are what pin its three causes to one
//! answer. A verifier that reached **no** verdict is not a claim about the caller, so it gets its
//! own code and its own message: see [`a_verifier_that_cannot_answer_is_not_a_refusal`].
//!
//! Three lists, not two: reflection answers without a credential and is registered by
//! nothing, so the equality covering service methods cannot hold it. See
//! [`UNAUTHENTICATED_NON_SERVICE`].

use std::collections::BTreeSet;

use api::startup::register_services;
use axum::http::StatusCode;
use buffa::enumeration::EnumValue;
use connectrpc::{ErrorCode, Router};
use proto::r#gen::buffa::counter::v1::WhoamiRequest;
use proto::r#gen::buffa::health::v1::{LiveRequest, Status};
use proto::r#gen::connect::counter::v1::CounterServiceClient;
use proto::r#gen::connect::health::v1::HealthServiceClient;

use crate::helpers::{
    REJECTION, SUBJECT, TOKEN, UNAVAILABLE_CAUSE, authorized_client_config, client_config,
    client_transport, dead_pool, raw_call, raw_call_authorized, raw_call_to_authorized,
    router_with_unavailable_verifier,
};

/// Every method this service registers that answers an anonymous caller.
///
/// Not documentation. `every_registered_method_is_classified` fails when a method appears in
/// neither list, and the two loops below fail when a method does not behave as classified —
/// which is what makes forgetting to think about auth a test failure rather than a silently
/// public endpoint. They live in a test rather than beside the handlers because the assertion
/// is about the *router*, and only the router knows the whole set.
///
/// Names arrive in these lists two ways. `cargo xtask new-feature` registers four methods per
/// feature slice and writes all four into `PROTECTED` in the same run, so a name you did not
/// type is a method a generator mounted — and the handler beside it calls `ctx.principal()?`,
/// which is the only reason a list and a handler can be trusted to say the same thing.
const PUBLIC: &[&str] = &[
    "counter.v1.CounterService/Get",
    "counter.v1.CounterService/Increment",
    "counter.v1.CounterService/Decrement",
    "counter.v1.CounterService/Reset",
    "health.v1.HealthService/Live",
    "health.v1.HealthService/Ready",
];

/// Every method that requires an identity.
///
/// `Whoami` is the example's, and it exists to be here: see the comment on the RPC in
/// `proto/counter/v1/counter.proto`. Replacing it means moving these three tests onto the
/// replacement in the same commit, not deleting them.
const PROTECTED: &[&str] = &["counter.v1.CounterService/Whoami"];

/// Every method served without a credential that **no service registers** — reflection, and
/// so far only reflection.
///
/// A third list rather than entries in `PUBLIC`, because the `assert_eq!` below is an
/// *equality* against the routing table: `build_router` installs reflection around
/// `register_services`, so these names never reach that set, and loosening the equality to
/// admit them would delete the half that catches a classified method nobody registers.
///
/// Held to the same standard two ways instead: the equality's neighbouring assert refuses a
/// registered method here, and
/// `the_unauthenticated_non_service_methods_are_the_ones_the_router_mounts` pins this list to
/// what the router mounts. Unauthenticated *when served* — whether it is served is
/// configuration (`tests/api/reflection.rs`, `docs/adr/0019`).
const UNAUTHENTICATED_NON_SERVICE: &[&str] = &[
    "grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
    "grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo",
];

/// The path a raw call uses for a Connect method.
fn path(method: &str) -> String {
    format!("{}/{method}", api::API_PREFIX)
}

// ---------------------------------------------------------------------------
// One protected method, one public one
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_authenticated_call_reaches_the_handler() {
    // Through the generated client rather than a raw request, so the assertion is about what
    // a caller sees: a typed response, not a JSON encoding.
    let client = CounterServiceClient::new(client_transport(), authorized_client_config());

    let response = client
        .whoami(WhoamiRequest::default())
        .await
        .expect("a credential the verifier accepts must reach the handler");

    assert_eq!(
        response.into_owned().subject,
        SUBJECT,
        "the handler must see the principal the verifier named, not the token it arrived as"
    );
}

#[tokio::test]
async fn an_anonymous_call_to_a_protected_method_is_unauthenticated() {
    let client = CounterServiceClient::new(client_transport(), client_config());

    let error = client
        .whoami(WhoamiRequest::default())
        .await
        .expect_err("no credential means no identity means no answer");

    assert_eq!(error.code, ErrorCode::Unauthenticated);
    assert_eq!(error.message.as_deref(), Some("authentication required"));
}

#[tokio::test]
async fn a_public_method_still_answers_an_anonymous_caller() {
    // The case people omit, and the one that catches a layer that rejects too eagerly: a seam
    // that authenticates everything is not a seam, it is a wall, and the four counter methods
    // every shell calls would be behind it. `Live` because it touches no database, so this
    // asserts on the gate rather than on a store CI does not have.
    let client = HealthServiceClient::new(client_transport(), client_config());

    let response = client
        .live(LiveRequest::default())
        .await
        .expect("liveness must not need an identity");

    assert_eq!(
        response.into_owned().status,
        EnumValue::Known(Status::Serving)
    );
}

// ---------------------------------------------------------------------------
// What the wire says, and what it does not
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_rejected_credential_is_indistinguishable_from_no_credential() {
    let whoami = path("counter.v1.CounterService/Whoami");

    let (absent_status, absent_body) = raw_call(&whoami, "{}").await;
    let (rejected_status, rejected_body) =
        raw_call_authorized(&whoami, "{}", "Bearer a-token-somebody-guessed").await;

    // One answer for both histories. The difference is an oracle for whoever is guessing
    // tokens, and the cause is already in the log where it belongs.
    assert_eq!(rejected_status, absent_status);
    assert_eq!(rejected_body, absent_body);

    assert_eq!(rejected_status, StatusCode::UNAUTHORIZED);
    assert_eq!(rejected_body["code"], "unauthenticated");
    assert_eq!(rejected_body["message"], "authentication required");
}

#[tokio::test]
async fn the_response_carries_neither_the_credential_nor_why_it_failed() {
    // Mechanical, not discretionary: `telemetry::http_layer` reads the Connect error body and
    // exports its message as `exception.message`, so anything in this message lands in
    // whatever trace backend a product runs.
    let whoami = path("counter.v1.CounterService/Whoami");
    let guessed = "a-token-somebody-guessed";

    let (_status, body) = raw_call_authorized(&whoami, "{}", &format!("Bearer {guessed}")).await;
    let rendered = body.to_string();

    assert!(
        !rendered.contains(guessed),
        "the credential reached the wire: {rendered}"
    );
    assert!(
        !rendered.contains(REJECTION),
        "the verifier's complaint reached the wire: {rendered}"
    );
    assert!(
        !rendered.contains(TOKEN),
        "the token the tests do accept reached the wire: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// The fourth case: a verifier that reached no verdict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_verifier_that_cannot_answer_is_not_a_refusal() {
    // The shape of `the_response_carries_neither_the_credential_nor_why_it_failed`, for the case
    // that case cannot cover: a verifier that did I/O and got nothing back. Collapsed into
    // `Anonymous`, that is a 401 telling a caller with a good credential that their credential is
    // bad — and a client that discards its token on 401 re-authenticates against the same dead
    // dependency.
    //
    // A credential is presented deliberately. The layer does not wake a verifier with nothing to
    // ask about, so an anonymous call to this router is indistinguishable from an anonymous call
    // to the healthy one — which is correct, and is why this test has to send something.
    let whoami = path("counter.v1.CounterService/Whoami");
    let presented = "a-token-somebody-guessed";

    let (status, body) = raw_call_to_authorized(
        router_with_unavailable_verifier(),
        &whoami,
        "{}",
        &format!("Bearer {presented}"),
    )
    .await;
    let rendered = body.to_string();

    // A different answer, so a dashboard can tell an outage of our own dependency apart from a
    // caller guessing tokens. `internal` rather than `unavailable`: `apps/counter/client` maps
    // `Unavailable` to "cannot reach the API at <endpoint>", and this request did reach the API
    // (ADR 0012).
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{rendered}");
    assert_eq!(body["code"], "internal", "{rendered}");
    assert_eq!(
        body["message"], "the service could not check that credential",
        "{rendered}: one message for this case too, and none of the caller's"
    );

    // ...and the same thing it must not carry: neither the credential nor the reason the
    // verifier gave for not answering, which is exactly what a `warn` log is for.
    assert!(
        !rendered.contains(presented),
        "the credential reached the wire: {rendered}"
    );
    assert!(
        !rendered.contains(UNAVAILABLE_CAUSE),
        "the verifier's reason for not answering reached the wire: {rendered}"
    );
    assert!(
        !rendered.contains(REJECTION),
        "the other arm's complaint leaked into this one: {rendered}"
    );
    assert!(
        !rendered.contains(TOKEN),
        "the token the tests do accept reached the wire: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// The routing table
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_registered_method_is_classified() {
    // A runtime, though nothing here awaits: `dead_pool` builds a sqlx pool, and constructing
    // one needs a Tokio context even though it never connects.
    //
    // `Router::methods()` is the real registration, so this covers a method added to the
    // contract and wired up by somebody who never opened this file.
    let registered: BTreeSet<String> = register_services(Router::new(), dead_pool())
        .methods()
        .map(ToOwned::to_owned)
        .collect();
    let classified: BTreeSet<String> = PUBLIC
        .iter()
        .chain(PROTECTED)
        .map(|method| (*method).to_owned())
        .collect();

    assert!(
        PUBLIC.iter().all(|method| !PROTECTED.contains(method)),
        "a method is classified both public and protected"
    );
    // Without this, deleting the only protected method makes both loops below iterate zero
    // items and pass — the seam would be left with no positive coverage at all.
    assert!(
        !PROTECTED.is_empty(),
        "no method requires an identity, so nothing here exercises the seam; if a real method \
         replaced the example one, classify it in the same commit"
    );
    assert_eq!(
        registered, classified,
        "a registered RPC is not classified — decide whether it needs an identity and add it \
         to PUBLIC or PROTECTED. An unclassified method is one nobody thought about, and the \
         default must not be public."
    );
    // `UNAUTHENTICATED_NON_SERVICE` is not a way out of that equality: a *registered* method
    // moved into it would be excused from the assertion, which is the one failure mode a
    // third list must not have.
    assert!(
        UNAUTHENTICATED_NON_SERVICE
            .iter()
            .all(|method| !registered.contains(*method)),
        "a method this service registers is listed in UNAUTHENTICATED_NON_SERVICE, which \
         exempts it from the classification above — classify it in PUBLIC or PROTECTED \
         instead; that list is for endpoints no service registers"
    );
}

#[tokio::test]
async fn the_unauthenticated_non_service_methods_are_the_ones_the_router_mounts() {
    // A decision recorded about the wrong names records nothing, so the list is compared with
    // what `build_router` routes. `reflection_methods` is what the router mounts from rather
    // than a second copy, so this fails if reflection gains or loses a name.
    let mounted: BTreeSet<String> = api::startup::reflection_methods().into_iter().collect();
    let classified: BTreeSet<String> = UNAUTHENTICATED_NON_SERVICE
        .iter()
        .map(|method| (*method).to_owned())
        .collect();

    assert_eq!(
        mounted, classified,
        "build_router mounts an unauthenticated endpoint that is not classified here, or \
         this list names one it does not mount"
    );
}

#[tokio::test]
async fn no_public_method_refuses_an_anonymous_caller() {
    // Concurrently, and the concurrency is worth the `JoinSet`: every method here is let
    // through the gate and then waits out a deadline against a pool pointed at a port with
    // nothing behind it — `timeouts::DB_ACQUIRE` for the four counter methods,
    // `READINESS_PROBE` for `Ready`, nothing for `Live`. Sequentially that is their sum, and
    // it grows with the list; overlapped it is the longest single wait.
    //
    // The waiting is the assertion working: a method that answered instantly would be one
    // that never reached a handler. So the fix for a slow run is to overlap it, never to drop
    // a method from `PUBLIC` — an entry removed here is an entry nothing proves is public.
    let mut calls = tokio::task::JoinSet::new();
    for method in PUBLIC {
        let path = path(method);
        calls.spawn(async move { (*method, raw_call(&path, "{}").await.1) });
    }

    while let Some(call) = calls.join_next().await {
        let (method, body) = call.expect("a call to a public method should not panic");

        // The gate, not the handler: `Ready` and the four counter methods reach a pool that
        // points at a port with nothing behind it, so they answer `internal`. That is the
        // handler failing after being let in, and it is the distinction this asserts.
        assert_ne!(
            body["code"], "unauthenticated",
            "{method} refused an anonymous caller but is classified PUBLIC"
        );
    }
}

#[tokio::test]
async fn every_protected_method_refuses_an_anonymous_caller() {
    for method in PROTECTED {
        let (status, body) = raw_call(&path(method), "{}").await;

        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method}");
        assert_eq!(body["code"], "unauthenticated", "{method}");
        assert_eq!(
            body["message"], "authentication required",
            "{method}: one message, whatever the reason"
        );
    }
}
