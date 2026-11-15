//! Liveness and readiness.
//!
//! Two surfaces over one decision: the `health.v1` Connect service
//! ([`service::v1`]) and the plain `/healthz` + `/readyz` probes ([`http`]).

pub mod http;
pub mod service;
