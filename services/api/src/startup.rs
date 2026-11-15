//! Assembling and running the HTTP stack.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use connectrpc::{ConnectRpcService, Router};
use connectrpc_reflection::{
    Reflector, SERVER_REFLECTION_SERVICE_NAME, SERVER_REFLECTION_V1ALPHA_SERVICE_NAME, install,
};
use identity::{IdentityLayer, VerifyCredential};
use sea_orm::DatabaseConnection;
use telemetry::http_layer::RpcLayer;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tower_http::timeout::TimeoutLayer;

use crate::config::Settings;
use crate::{API_PREFIX, counter, health, timeouts};

pub struct App {
    port: u16,
    // The bound socket is kept rather than handed back as an address for the caller to
    // rebind: binding the same address twice in one process is `AddrInUse`, and with port
    // 0 the caller would get a *different* ephemeral port than `port` reports.
    listener: TcpListener,
    router: axum::Router,
}

impl App {
    /// Bind the socket and assemble the stack.
    ///
    /// `verifier` decides whose a credential is, and is the only part of the identity seam a
    /// product has to supply — see `crates/identity`.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured address cannot be bound.
    pub async fn build(
        settings: &Settings,
        db: DatabaseConnection,
        verifier: Arc<dyn VerifyCredential>,
    ) -> std::io::Result<Self> {
        let address = format!(
            "{}:{}",
            settings.application.host, settings.application.port
        );
        let listener = TcpListener::bind(&address).await?;
        let port = listener.local_addr()?.port();

        Ok(Self {
            port,
            listener,
            router: build_router(
                db,
                verifier,
                Reflection::from_enabled(settings.reflection.enabled),
                Web::from_settings(&settings.web),
            ),
        })
    }

    /// The port actually bound, which equals the configured one only when that was
    /// non-zero. Configure port 0 and read this to run a test server on a free port
    /// without racing another test for it.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Serve until the process is asked to stop.
    ///
    /// # Errors
    ///
    /// Returns an error if the server fails while accepting or serving connections.
    pub async fn run_until_stopped(self) -> std::io::Result<()> {
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(shutdown_signal())
            .await
    }
}

/// Whether this process serves server reflection ([`crate::config::ReflectionSettings`],
/// `docs/adr/0019`).
///
/// Named variants rather than a `bool`: `build_router(db, verifier, true)` reads the same
/// whichever way round it was meant, and the wrong way round publishes the whole schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reflection {
    /// Mounted at both paths, unauthenticated. What the `local` profile asks for.
    Served,
    /// Mounted nowhere, so both paths answer 404. The default.
    Withheld,
}

impl Reflection {
    /// The decision as configuration spells it.
    #[must_use]
    pub const fn from_enabled(enabled: bool) -> Self {
        if enabled {
            Self::Served
        } else {
            Self::Withheld
        }
    }
}

/// Whether this origin serves a built single-page application, and from where.
///
/// A named variant rather than an `Option<PathBuf>`, for the reason [`Reflection`] is: the
/// interesting state is "a directory and the entry point inside it", and a bare optional
/// path leaves the index a second, unstated default somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Web {
    /// No static files. Unknown root paths are 404, which is what an API-only service wants.
    Absent,
    /// Serve `dir`, answering a path that is not a file with `dir/index`.
    Served { dir: PathBuf, index: String },
}

impl Web {
    /// The decision as configuration spells it.
    #[must_use]
    pub fn from_settings(settings: &crate::config::WebSettings) -> Self {
        if settings.is_served() {
            Self::Served {
                dir: PathBuf::from(&settings.dir),
                index: settings.index.clone(),
            }
        } else {
            Self::Absent
        }
    }
}

/// The two reflection methods [`build_router`] mounts when reflection is served — also the
/// root path each is routed at, minus the leading slash.
///
/// Derived from `connectrpc-reflection`'s constants, and `pub` so `tests/api/auth.rs` and
/// `tests/api/reflection.rs` assert against what is actually routed. Retyped there, a drift
/// would look like a passing test.
#[must_use]
pub fn reflection_methods() -> [String; 2] {
    [
        format!("{SERVER_REFLECTION_SERVICE_NAME}/ServerReflectionInfo"),
        format!("{SERVER_REFLECTION_V1ALPHA_SERVICE_NAME}/ServerReflectionInfo"),
    ]
}

/// The schema, from the descriptor pool `protoc-gen-buffa` embeds in the generated code
/// (`reflect_mode=bridge` in `proto/buf.gen.yaml`) — no `FileDescriptorSet` beside the binary
/// and no build script. The pool is the whole image, so naming `health::v1` answers for
/// `counter.v1` too; health, because every product has one.
///
/// Called once per mount: `Reflector` is not `Clone`, and rebuilding it from a shared
/// `Arc<DescriptorPool>` is a refcount bump. A fn rather than a closure because a withheld
/// reflection calls it from nowhere, and an unused closure fails `-D warnings`.
fn reflector() -> Reflector {
    Reflector::from_descriptor_pool(Arc::clone(
        proto::r#gen::buffa::health::v1::descriptor_pool(),
    ))
    .expect("the descriptor pool buffa embedded is well-formed")
}

/// Assemble the HTTP stack: services, then layers.
///
/// Split out of [`App::build`] so a test can build the real stack — real routing, real
/// middleware — and drive it in-process with `tower`'s `oneshot`, without binding a socket
/// or reaching a database. The verifier is a parameter for the same reason: a test supplies
/// one that accepts a known token, and the identity seam is then exercisable with no
/// provider and no secret anywhere. `reflection` is a parameter so a test can name the state
/// it means without building a `Settings` and asserting about the loader too.
///
/// # Panics
///
/// If the descriptor pool `protoc-gen-buffa` embedded in the generated code is not
/// well-formed. That is build-time output rather than input, so it fails identically on
/// every request for every caller — at startup, in CI, before a deploy — and is a
/// regenerate-and-commit problem rather than a runtime condition to carry a `Result` for.
pub fn build_router(
    db: DatabaseConnection,
    verifier: Arc<dyn VerifyCredential>,
    reflection: Reflection,
    web: Web,
) -> axum::Router {
    // Installed AROUND `register_services`, not inside it — which is why the classification
    // in `tests/api/auth.rs` needs a third list. Withheld, nothing dispatches the two names
    // and this mount answers them like any method it does not have.
    let registered = register_services(Router::new(), db.clone());
    let service = ConnectRpcService::new(match reflection {
        Reflection::Served => install(registered, reflector()),
        Reflection::Withheld => registered,
    });

    // Connect dispatches on the full path `/<package>.<Service>/<Method>`, so the services
    // are mounted as a fallback under the API prefix rather than as individual routes.
    // `nest` strips the prefix before the inner service sees the request, which is what
    // makes the path the dispatcher matches independent of where the API is mounted.
    let api = axum::Router::new()
        .fallback_service(service)
        // The identity layer goes INSIDE the nest, and the reason is the opposite of the RPC
        // layer's below: that one reads the path, so the nest would strip what it needs
        // before it sees it; this one reads a header and does not care where the API is
        // mounted. Inside means only `/api/*` requests are resolved — the web bundle this
        // origin will serve in release builds has no identity and no business being asked
        // for one.
        //
        // It is also inside the RPC span and inside the request timeout, and both matter. A
        // verifier does I/O in a real product (a JWKS fetch, a session lookup), so it has to
        // be inside `timeouts::REQUEST` or a hung identity provider hangs the request
        // forever. And the span matters for the rule this placement is really about: any
        // layer that can reject must sit inside `RpcLayer`, because a rejection produced
        // outside it is a response the RPC layer never sees — no span, no
        // `rpc.server.call.duration` measurement, so an authentication failure is invisible
        // in exactly the traces and the error-rate dashboard you would look at during one.
        // This layer never rejects; the handler does, and a handler is always inside.
        //
        // `layer`, never `route_layer`. They are not near-synonyms here: `route_layer` never
        // reaches the fallback router, and the Connect service IS the fallback mounted above.
        // Today axum panics on that — "Adding a route_layer before any routes is a no-op",
        // and this router has no routes of its own — which is the loud version of the trap.
        // The day this origin grows a route (the release-build static files anticipated above
        // are the obvious one), the panic becomes the no-op it describes: the layer applies to
        // the new route, every RPC keeps working, and none of them has an identity.
        .layer(IdentityLayer::new(verifier));

    // The RPC layer stays outside the nest on purpose: it reads the full path
    // (`/api/<package>.<Service>/<Method>`) to name the span after the Connect method,
    // and the nest is what strips the prefix — mount it inside and the method name is
    // gone before the layer sees it. The timeout stays outside too, and this origin will
    // also serve the web shell's static files in release builds, which the RPC layer
    // leaves uninstrumented (they are HTTP requests, not RPCs).
    //
    // The probes are merged here, at the root, beside the nest rather than inside it: an
    // orchestrator asks `/readyz`, not `/api/readyz`, and a path under the prefix would be
    // one more thing every deployment's probe configuration has to know.
    let mut root = axum::Router::new()
        .nest(API_PREFIX, api)
        .merge(health::http::probes(db));

    if reflection == Reflection::Served {
        // Reflection is mounted a second time, at the server root, because tools disagree
        // about where to ask for it. `buf curl` and the generated clients derive it from the
        // base URI and find the copy inside `/api`; gRPC-native tooling — grpcurl, Bruno's
        // gRPC mode — asks at the root whatever URL it was given, because the gRPC protocol
        // defines a path as `/package.Service/Method` and has no notion of a base path.
        // Serving one and not the other is indistinguishable from serving neither, from the
        // client's side — so both go on and off together, under one config key.
        //
        // Routed explicitly rather than as a fallback, so the fallback stays free for the
        // web bundle this origin serves in release builds.
        let service = ConnectRpcService::new(install(Router::new(), reflector()));
        let [v1, v1alpha] = reflection_methods();
        root = root
            .route_service(&format!("/{v1}"), service.clone())
            .route_service(&format!("/{v1alpha}"), service);
    }

    // The bundle, as the fallback — never as a route. Everything routed above (the API nest,
    // the probes, the root reflection mounts) is matched first, so a client router claiming
    // `/api/...` cannot shadow an RPC, and a probe cannot be swallowed by the SPA's own
    // `index.html`.
    //
    // `fallback`, not `not_found_service`: that constructor pins the fallback's response to
    // 404, which is right for a not-found *page* and wrong for this — a client-side route
    // that renders correctly is a 200, and a reload at one must not look like a missing
    // resource to a browser, a crawler or a monitoring check.
    let root = match web {
        Web::Absent => root,
        Web::Served { dir, index } => {
            let spa = ServeDir::new(&dir).fallback(
                axum::Router::new()
                    .fallback(entry_point)
                    .with_state(SpaEntryPoint(Arc::new(dir.join(index)))),
            );
            root.fallback_service(spa)
        }
    };

    root.layer(RpcLayer::new())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            timeouts::REQUEST,
        ))
}

/// The one place that knows every feature.
///
/// Service implementations live with their feature (`health/service/v1.rs`) rather than in
/// a shared `services/` tree, so adding one means adding a module and a line here.
#[must_use]
pub fn register_services(router: Router, db: DatabaseConnection) -> Router {
    router
        .add_service(Arc::new(health::service::v1::Health::new(db.clone())))
        .add_service(Arc::new(counter::service::v1::Counter::new(db)))
}

/// Resolve when the process is asked to stop, so in-flight requests can finish.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        // SIGTERM is what a container runtime sends first; without handling it the process
        // is killed outright and in-flight requests are dropped.
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "could not install SIGTERM handler"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

/// State for the SPA fallback: the bundle's entry point.
#[derive(Clone)]
struct SpaEntryPoint(Arc<PathBuf>);

/// Answer a client-side route with `index.html`, and a missing *file* with `404`.
///
/// `ServeDir` answers the file case — it serves what exists — so this only runs when nothing
/// matched. The distinction that matters is the last path segment: `assets/main.4f2c.js` that
/// is not there is a stale asset (a browser or a scraper holding a previous build's URL, an
/// ES module import that asked for a file), and answering it with the entry point and a 200
/// turns a clean 404 into a parse error inside a file the network tab reports as loaded. A
/// path a client router owns (`/counter/42`, `/settings`) has no extension and gets the entry
/// point, which is what makes a reload at a shared link work.
async fn entry_point(State(entry): State<SpaEntryPoint>, uri: Uri) -> Response {
    let last = uri.path().rsplit('/').next().unwrap_or_default();
    if last.contains('.') {
        tracing::debug!(path = uri.path(), "spa: no such file");
        return StatusCode::NOT_FOUND.into_response();
    }

    match tokio::fs::read(entry.0.as_ref()).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response(),
        Err(error) => {
            tracing::error!(
                %error,
                entry = %entry.0.display(),
                "spa: the bundle's entry point is unreadable"
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
