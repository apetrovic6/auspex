use buffa::enumeration::EnumValue;
use connectrpc::{RequestContext, Response, ServiceRequest, ServiceResult};
use proto::r#gen::buffa::health::v1::{
    Component, LiveRequest, LiveResponse, ReadyRequest, ReadyResponse, Status,
};
use proto::r#gen::connect::health::v1::HealthService;
use sea_orm::DatabaseConnection;

use crate::timeouts;

/// Health checks.
#[derive(Debug, Clone)]
pub struct Health {
    db: DatabaseConnection,
}

impl Health {
    #[must_use]
    pub const fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl HealthService for Health {
    /// Deliberately checks nothing.
    ///
    /// Liveness answers "should this process be restarted?", and the answer to a database
    /// outage is no — restarting every replica into the same outage turns a degraded
    /// service into a crash loop, and removes the process that would have recovered when
    /// the dependency came back. Dependency checks belong in [`Self::ready`].
    #[tracing::instrument(name = "health.live", skip_all)]
    async fn live(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, LiveRequest>,
    ) -> ServiceResult<LiveResponse> {
        Response::ok(LiveResponse {
            status: EnumValue::Known(Status::Serving),
            ..Default::default()
        })
    }

    /// Answers "should this replica receive traffic?" — which is what a load balancer
    /// should gate on, and where dependency checks belong.
    #[tracing::instrument(name = "health.ready", skip_all)]
    async fn ready(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ReadyRequest>,
    ) -> ServiceResult<ReadyResponse> {
        let (status, components) = probe(&self.db).await;

        Response::ok(ReadyResponse {
            status: EnumValue::Known(status),
            components,
            ..Default::default()
        })
    }
}

/// The readiness decision, shared by the Connect handler and the plain `/readyz` probe.
///
/// One function rather than two copies, because the two surfaces answer the same question and
/// an orchestrator that sees `200` from one while the RPC says `NOT_SERVING` has no way to
/// tell which is wrong.
///
/// `ping` issues a trivial query, so this reports whether the pool can actually reach the
/// database — not merely whether it was configured. The probe has its own deadline
/// ([`timeouts::READINESS_PROBE`]), shorter than the pool's acquire timeout, so a failure
/// names the component instead of surfacing as an empty request timeout.
pub async fn probe(db: &DatabaseConnection) -> (Status, Vec<Component>) {
    match tokio::time::timeout(timeouts::READINESS_PROBE, db.ping()).await {
        Ok(Ok(())) => (
            Status::Serving,
            vec![component("database", Status::Serving, String::new())],
        ),
        Ok(Err(error)) => {
            tracing::warn!(%error, "readiness: database unreachable");
            (
                Status::NotServing,
                vec![component("database", Status::NotServing, error.to_string())],
            )
        }
        Err(_elapsed) => {
            tracing::warn!(timeout = ?timeouts::READINESS_PROBE, "readiness: database probe timed out");
            (
                Status::NotServing,
                vec![component(
                    "database",
                    Status::NotServing,
                    format!("probe timed out after {:?}", timeouts::READINESS_PROBE),
                )],
            )
        }
    }
}

fn component(name: &str, status: Status, message: String) -> Component {
    Component {
        name: name.to_owned(),
        status: EnumValue::Known(status),
        message,
        ..Default::default()
    }
}
