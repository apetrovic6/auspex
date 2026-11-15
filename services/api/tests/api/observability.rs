//! The RPC layer, wired into the real router.
//!
//! `telemetry`'s own tests drive the layer over a synthetic tower service; these drive it
//! over `build_router` — the real axum nest, the real `ConnectRpcService`, the real health
//! handler — which is what proves the layer is actually mounted where a request hits it,
//! and that a Connect method name (not an HTTP path) comes out the other side. A layer
//! mounted inside the nest, or not mounted at all, passes the synthetic test and fails
//! here.

// Holding `RECORDING_TRACER` across the whole async test body is the mechanism, not an
// oversight — see its doc comment. The lint's real target is a blocked runtime thread or a
// deadlock, and neither is reachable here: `#[tokio::test]` gives each test a
// current-thread runtime of its own, so no other task is waiting on the thread that holds
// the guard. Scoped to this file rather than per test because every test here that records
// spans must hold it, so the exemption is the rule in this module.
#![expect(
    clippy::await_holding_lock,
    reason = "the guard serialises the tests that install a global tracer provider"
)]

use std::sync::{Mutex, MutexGuard, PoisonError};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use opentelemetry::global;
use opentelemetry::trace::{SpanId, Status, TraceId};
use opentelemetry_sdk::trace::{
    InMemorySpanExporter, InMemorySpanExporterBuilder, SdkTracerProvider,
};
use opentelemetry_semantic_conventions::attribute;
use tower::ServiceExt as _;

use crate::helpers::router;

/// The W3C example identifiers, standing in for a caller's already-open trace.
const PARENT_TRACE_ID: TraceId =
    TraceId::from_bytes(0x4bf9_2f35_77b3_4da6_a3ce_929d_0e0e_4736_u128.to_be_bytes());
const PARENT_SPAN_ID: SpanId = SpanId::from_bytes(0x00f0_67aa_0ba9_02b7_u64.to_be_bytes());

/// A second trace id, for the refusal test, so each test here can find its own span and
/// only its own. One id per test, never a shared one: the exporter is global, so a test
/// that filters on something another test also produces is asserting about whichever span
/// arrived first. See the comment at its one use.
const REFUSAL_TRACE_ID: TraceId =
    TraceId::from_bytes(0x4bf9_2f35_77b3_4da6_a3ce_929d_0e0e_4737_u128.to_be_bytes());

/// Serialises the tests in this file.
///
/// **Every test here that reads spans must hold this.** `set_tracer_provider` is
/// process-wide and last-writer-wins, and the RPC layer resolves `global::tracer` per
/// request, so two tests installing their own exporter at once leave the loser reading an
/// exporter the layer no longer writes to. It then fails on an empty `Vec`, and which test
/// loses depends on scheduling — the failure moves between them.
///
/// nextest gives every test its own process and hides this entirely, so `just rust-test`
/// never sees it. `cargo test -p api` does, which is what someone iterating on one test
/// runs and what an IDE's "run test" button does.
///
/// `crates/telemetry` carries its own lock for its own tests. A lock cannot be shared
/// across them because a test binary is a process: this one serialises this binary, which
/// is the whole requirement.
static RECORDING_TRACER: Mutex<()> = Mutex::new(());

/// Install a recording tracer provider as the global, and hold [`RECORDING_TRACER`] until
/// the returned guard is dropped.
///
/// The RPC layer resolves `global::tracer` per request, so this is all the wiring a test
/// needs — no `telemetry::init`, no OTLP endpoint, no subscriber.
fn install_recording_tracer() -> (InMemorySpanExporter, MutexGuard<'static, ()>) {
    // A test that fails while holding this must not turn its neighbour into a poison error
    // that names nothing about why it failed.
    let guard = RECORDING_TRACER
        .lock()
        .unwrap_or_else(PoisonError::into_inner);

    let exporter = InMemorySpanExporterBuilder::new().build();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    global::set_tracer_provider(provider);

    (exporter, guard)
}

#[tokio::test]
async fn an_inbound_traceparent_parents_the_rpc_span_named_after_the_method() {
    let (exporter, _recording) = install_recording_tracer();

    // Health `Live` touches no database, so the lazy pool pointed at a dead port is fine
    // — the request reaches the handler and answers without anything listening.
    let request = Request::builder()
        .method("POST")
        .uri("/api/health.v1.HealthService/Live")
        .header("content-type", "application/json")
        .header(
            "traceparent",
            format!("00-{PARENT_TRACE_ID}-{PARENT_SPAN_ID}-01"),
        )
        .body(Body::from("{}"))
        .expect("a valid request");

    let response = router().oneshot(request).await.expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
    // Read the body to completion like a real caller; the success span already ended at
    // the head, but a half-read response is not what production does.
    let _ = response.into_body().collect().await;

    let spans = exporter.get_finished_spans().expect("readable");
    let span = spans
        .iter()
        .find(|span| span.span_context.trace_id() == PARENT_TRACE_ID)
        .expect("the request produced a span in the caller's trace");

    // The span says RPC, not HTTP — the whole reason `TraceLayer` was replaced. Mounted
    // inside the nest, the `/api` prefix would be stripped before the layer saw it and
    // the method name would be wrong; not mounted at all, no span would exist.
    assert_eq!(span.name, "health.v1.HealthService/Live");
    // … and it is a child of the caller's trace, not a fresh root.
    assert_eq!(span.parent_span_id, PARENT_SPAN_ID);
    assert!(span.parent_span_is_remote);
}

#[tokio::test]
async fn an_authentication_failure_is_recorded_in_the_rpc_span() {
    // The observable half of the layer-placement argument in `startup.rs`: the identity layer
    // never rejects, a handler does, and a handler is always inside the RPC span — so a
    // refusal carries the Connect code into the trace and into `rpc.server.call.duration`
    // with no instrumentation of its own. An authentication failure that was invisible in
    // exactly the traces and the error-rate dashboard you would look at during one is the
    // outcome this rules out.
    let (exporter, _recording) = install_recording_tracer();

    // `Whoami` requires an identity and this request carries none, so the handler refuses.
    // No credential is sent, because "no credential" and "bad credential" are the same answer
    // by design (see `tests/api/auth.rs`).
    //
    // The traceparent is here to make the span *identifiable*, not because the scenario needs
    // a caller's trace: the exporter is reached through a process-wide global, so every test
    // running concurrently writes into it, and `auth::an_authenticated_call_reaches_the_handler`
    // calls this very method and succeeds. Finding a span by name alone picked that one up and
    // failed with "a refusal is an error, not a success: Unset" — a span from another test, not
    // a defect in the layer. A trace id nothing else sends cannot be satisfied by a stray span.
    let request = Request::builder()
        .method("POST")
        .uri("/api/counter.v1.CounterService/Whoami")
        .header("content-type", "application/json")
        .header(
            "traceparent",
            format!("00-{REFUSAL_TRACE_ID}-{PARENT_SPAN_ID}-01"),
        )
        .body(Body::from("{}"))
        .expect("a valid request");

    let response = router().oneshot(request).await.expect("infallible");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // Read to completion: the error code lives in the body, so the span only ends once the
    // body has been produced.
    let _ = response.into_body().collect().await;

    let spans = exporter.get_finished_spans().expect("readable");
    let span = spans
        .iter()
        .find(|span| span.span_context.trace_id() == REFUSAL_TRACE_ID)
        .expect("a refusal is spanned like any other RPC");
    assert_eq!(span.name, "counter.v1.CounterService/Whoami");

    assert!(
        matches!(&span.status, Status::Error { .. }),
        "a refusal is an error, not a success: {:?}",
        span.status
    );
    assert!(
        span.attributes
            .iter()
            .any(|kv| kv.key.as_str() == attribute::RPC_RESPONSE_STATUS_CODE
                && kv.value.as_str() == "unauthenticated"),
        "the span must name the Connect code: {:?}",
        span.attributes
    );
}
