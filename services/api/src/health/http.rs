//! The plain HTTP probes: `GET /healthz` and `GET /readyz`.
//!
//! The same two questions [`super::service::v1`] answers over Connect, spelled the way every
//! orchestrator already asks them. A Kubernetes probe cannot speak Connect, a container
//! healthcheck is a `curl`, and a load balancer's health check is a status code — so a
//! service that only answered the RPC protocol would be deployed with a probe that has to
//! know its contract.
//!
//! Both paths decide nothing themselves: they call the same functions the RPC handlers call,
//! so the two surfaces cannot disagree about whether this replica is ready. That is the
//! reason this module is a router rather than two more handlers — the decision lives once.
//!
//! `/healthz` never touches a dependency. Liveness answers "should this process be
//! restarted?", and a database outage is not a reason to restart every replica into the same
//! outage ([`super::service::v1::Health::live`] says why at length). `/readyz` asks the
//! database, and answers `503` when it cannot, which is the only signal that takes a replica
//! out of rotation.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use proto::r#gen::buffa::health::v1::Status;
use sea_orm::DatabaseConnection;

/// Liveness. Always `200` while the process can serve a request at all.
pub const LIVE_PATH: &str = "/healthz";

/// Readiness. `200` when every dependency answers, `503` when one does not.
pub const READY_PATH: &str = "/readyz";

/// The probe routes, with the pool they check.
///
/// Returned as a router so the caller merges it into the stack it assembles: the paths sit at
/// the server root, beside the reflection routes and outside the API prefix, and a fallback
/// mounted for a web bundle must not swallow them.
pub fn probes(db: DatabaseConnection) -> Router {
    Router::new()
        .route(LIVE_PATH, get(live))
        .route(READY_PATH, get(ready))
        .with_state(db)
}

/// `200 OK`, with a body a human reading `curl` output can act on.
async fn live() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// The readiness answer, as a status code.
///
/// The body is deliberately empty, including on failure. `/readyz` is reachable from outside
/// through whatever reverse proxy fronts the service, and the component detail — a database
/// host, a driver's error text — is diagnostic material for an operator, not something to hand
/// an unauthenticated caller. The detail is logged here instead, at the moment the probe
/// fails, which is where it is useful.
async fn ready(State(db): State<DatabaseConnection>) -> Response {
    let (status, components) = super::service::v1::probe(&db).await;
    if status == Status::Serving {
        return (StatusCode::OK, "ok").into_response();
    }

    for component in components {
        tracing::warn!(
            component = %component.name,
            status = ?component.status,
            message = %component.message,
            "readiness: not serving"
        );
    }
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}
