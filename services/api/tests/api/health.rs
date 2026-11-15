use axum::http::StatusCode;
use buffa::enumeration::EnumValue;
use proto::r#gen::buffa::health::v1::{LiveRequest, Status};
use proto::r#gen::connect::health::v1::HealthServiceClient;

use crate::helpers::{client_config, client_transport, get_call, raw_call, router};

#[tokio::test]
async fn the_plain_liveness_probe_answers_without_a_database() {
    // The two probes exist because nothing that runs a container speaks Connect: a
    // Kubernetes probe, a compose healthcheck and a `curl` all ask a plain GET. Liveness must
    // answer while the database is down, for the reason `liveness_does_not_depend_on_the_database`
    // gives — restarting every replica into the same outage is how a degraded service becomes
    // a crash loop.
    let (status, body) = get_call(router(), "/healthz").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn the_plain_readiness_probe_is_503_with_an_empty_body() {
    // The pool points at port 1, so the probe fails the way a replica with no database does.
    // The status is the signal an orchestrator acts on; the body is deliberately empty. This
    // endpoint is reachable from outside through whatever reverse proxy fronts the service, so
    // the component detail (a database host, a driver's error text) is logged at the moment the
    // probe fails rather than handed to an unauthenticated caller.
    let (status, body) = get_call(router(), "/readyz").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body.is_empty(),
        "the failure detail belongs in the log, not in a proxied response, got: {body}"
    );
}

#[tokio::test]
async fn the_probes_are_at_the_root_and_not_under_the_api_prefix() {
    // A deployment's probe is configured once and must not be a Connect path: `/api/healthz`
    // is not a method, so it answers the way an unknown method does.
    let (status, _) = get_call(router(), "/api/healthz").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn liveness_does_not_depend_on_the_database() {
    // The pool points at a port with nothing behind it. Liveness must still answer, because
    // its question is "should this process be restarted?" and the answer to a database
    // outage is no — restarting every replica into the same outage turns a degraded
    // service into a crash loop.
    let client = HealthServiceClient::new(client_transport(), client_config());

    let response = client
        .live(LiveRequest::default())
        .await
        .expect("liveness must not fail when a dependency is down");

    assert_eq!(
        response.into_owned().status,
        EnumValue::Known(Status::Serving)
    );
}

#[tokio::test]
async fn an_unknown_method_is_a_connect_error_not_an_html_404() {
    // Raw, not the generated client: the client has no function for a method that does not
    // exist, which is precisely the case under test.
    let (status, body) = raw_call("/api/health.v1.HealthService/NoSuchMethod", "{}").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["code"], "unimplemented",
        "clients branch on the Connect code, so the body matters as much as the status"
    );
}

#[tokio::test]
async fn requests_outside_the_api_prefix_do_not_reach_the_services() {
    // Raw for the same reason: the client derives its path from API_PREFIX and so cannot
    // send to the wrong one. The fallback outside /api stays free for the web shell.
    let (status, _) = raw_call("/health.v1.HealthService/Live", "{}").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}
