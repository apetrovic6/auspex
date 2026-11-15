//! One span and one duration measurement per RPC, for servers speaking Connect.
//!
//! This replaces `tower_http::trace::TraceLayer`, and the difference is the span's
//! content, not its cardinality: a `TraceLayer` span says `POST /api/counter.v1...`,
//! while the span here says `counter.v1.CounterService/Increment` and carries the RPC
//! semantic-convention attributes a backend's RPC views key off. Keeping both is the
//! standard way this goes wrong — two layers, two spans per request, and every dashboard
//! double-counts — so the HTTP-shaped one goes.
//!
//! What one request produces:
//!
//! - a server span named `<package>.<Service>/<Method>`, parented on the inbound W3C
//!   `traceparent` when the caller sent one (which is how a request from the TUI shell
//!   arrives under the client's trace), with `rpc.system.name`, `rpc.method` and
//!   `http.response.status_code`;
//! - on an error response, the Connect error code and message as a span event, the code
//!   as `rpc.response.status_code`, and the span status set to `Error`;
//! - one `rpc.server.call.duration` histogram measurement (seconds — the unit the
//!   semantic conventions prescribe for that name) per RPC, carrying the same
//!   identifying attributes. One instrument, not three: a histogram carries its own
//!   count, so rate and error rate are queries over its attributes, and separate
//!   request/error counters would re-derive what it already reports while tripling the
//!   time series per method.
//!
//! Stated limits, so nobody debugs them twice:
//!
//! - Unary Connect only, which is all this repository speaks. A Connect *streaming*
//!   error rides inside an HTTP 200 envelope and would need stream decoding; a gRPC or
//!   gRPC-Web error rides in trailers. Both are seen here as successes.
//! - The error code comes from the response body — Connect unary error bodies are
//!   spec-required JSON regardless of the request codec — so it is only available once
//!   the body has been produced. The span of an error response therefore ends when the
//!   body ends (or the client gives up on it), not when the head is ready.
//! - Paths that are not Connect RPCs (static files, anything else sharing the router)
//!   pass through uninstrumented: one span per RPC, not per HTTP request.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Instant;

use bytes::{Buf as _, BytesMut};
use http::{HeaderMap, Request, Response};
use http_body::{Body, Frame};
use opentelemetry::metrics::Histogram;
use opentelemetry::propagation::{Extractor, TextMapPropagator as _};
use opentelemetry::trace::{SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue, global};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_semantic_conventions::{attribute, metric};
use tower::{Layer, Service};

/// Instrumentation scope for everything this module creates.
const SCOPE: &str = env!("CARGO_PKG_NAME");

/// The `rpc.system.name` value the semantic-convention registry assigns to Connect RPC.
/// The registry publishes value sets as documentation, not as constants, so this is the
/// one literal in this module — spelled exactly as the registry spells it, because a
/// backend's RPC views filter on it.
const CONNECT_RPC_SYSTEM: &str = "connect_rpc";

/// The unit `rpc.server.call.duration` is defined in. Taken from the metric's semconv
/// entry, not chosen: backends key their RPC dashboards off the name/unit pair, and the
/// name's predecessor (`rpc.server.duration`) is retired *because* the unit moved from
/// milliseconds to seconds.
const DURATION_UNIT: &str = "s";

/// Error bodies are captured only to read the Connect error code out of them, and they
/// are small JSON documents. Anything past this cap cannot change the parse outcome
/// enough to matter; the cap exists so a misdetected firehose cannot grow the buffer.
const CAPTURE_LIMIT: usize = 16 * 1024;

/// Instruments a Connect router with one span and one duration measurement per RPC.
///
/// Mount where `TraceLayer::new_for_http()` would have gone — outside the nest that
/// strips the API prefix is fine; the path is parsed from the right.
#[derive(Debug, Clone, Copy, Default)]
pub struct RpcLayer;

impl RpcLayer {
    /// The layer. Stateless by design: the tracer and meter are resolved from the
    /// OpenTelemetry globals per request, so a router built before `telemetry::init`
    /// still exports once init has run — and a router built with no OTLP endpoint
    /// configured exports nowhere, through the no-op globals, at no cost.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for RpcLayer {
    type Service = RpcService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcService { inner }
    }
}

/// The [`RpcLayer`]'s service.
#[derive(Debug, Clone)]
pub struct RpcService<S> {
    inner: S,
}

/// Split a request path into its Connect service and method.
///
/// Connect dispatches on `/<package>.<Service>/<Method>`, possibly behind a mount prefix
/// (`/api/counter.v1.CounterService/Increment`). Anything that is not shaped that way —
/// a static file, a bare `/health` — is not an RPC and gets no span. The package dot is
/// required because buf mandates a package, and it is what keeps `index.html` from
/// looking like a method.
fn connect_method(path: &str) -> Option<(&str, &str)> {
    let (service_path, method) = path.rsplit_once('/')?;
    let (_, service) = service_path.rsplit_once('/')?;
    if method.is_empty() || !service.contains('.') {
        return None;
    }
    Some((service, method))
}

/// Adapts request headers to the propagator's [`Extractor`].
///
/// Hand-rolled rather than pulled from `opentelemetry-http`: that crate exists to glue
/// the SDK's exporters to HTTP clients, and this is six lines that must not drag a
/// reqwest-dependent crate into every server's graph for a header lookup.
struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(AsRef::as_ref).collect()
    }
}

impl<S, ReqB, ResB> Service<Request<ReqB>> for RpcService<S>
where
    S: Service<Request<ReqB>, Response = Response<ResB>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ReqB: Send + 'static,
    ResB: Body + Send + Unpin + 'static,
    ResB::Data: Send,
    ResB::Error: Send,
{
    type Response = Response<InstrumentedBody<ResB>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqB>) -> Self::Future {
        // Cloned per call, the standard tower pattern: the caller keeps a ready-checked
        // service while the future owns its own copy.
        let mut inner = self.inner.clone();

        let Some((service, method)) = connect_method(request.uri().path()) else {
            return Box::pin(async move {
                inner
                    .call(request)
                    .await
                    .map(|response| response.map(InstrumentedBody::passthrough))
            });
        };

        // Adopt the caller's trace before anything else, so the span below is born a
        // child of it rather than a fresh root.
        let parent = TraceContextPropagator::new()
            .extract_with_context(&Context::current(), &HeaderExtractor(request.headers()));

        let span_name = format!("{service}/{method}");
        let tracer = global::tracer(SCOPE);
        let span = tracer
            .span_builder(span_name.clone())
            .with_kind(SpanKind::Server)
            .with_attributes([
                KeyValue::new(attribute::RPC_SYSTEM_NAME, CONNECT_RPC_SYSTEM),
                // Fully qualified, per the current convention: `rpc.service` is retired
                // in favour of a method name that carries the service itself.
                KeyValue::new(attribute::RPC_METHOD, span_name.clone()),
            ])
            .start_with_context(&tracer, &parent);
        let cx = parent.with_span(span);

        let histogram = global::meter(SCOPE)
            .f64_histogram(metric::RPC_SERVER_CALL_DURATION)
            .with_unit(DURATION_UNIT)
            .build();

        Box::pin(CallFuture {
            inner: Box::pin(inner.call(request)),
            state: Some(RpcState {
                cx,
                start: Instant::now(),
                histogram,
                span_name,
            }),
            body: PhantomData,
        })
    }
}

/// Everything the span and the measurement share, travelling from the request head to
/// whichever side finishes the RPC.
struct RpcState {
    /// Holds the span. It ends when the last context holding it drops — on the success
    /// path that is inside [`CallFuture`], on the error path inside the body wrapper,
    /// which lives until the error document has been produced.
    cx: Context,
    start: Instant,
    histogram: Histogram<f64>,
    span_name: String,
}

/// Awaits the inner service with the RPC span attached as the current context, so
/// `tracing` spans opened by the handler nest under it through the
/// `tracing-opentelemetry` bridge.
struct CallFuture<F, ResB> {
    inner: Pin<Box<F>>,
    /// `None` once the response head has been handled and the state moved on.
    state: Option<RpcState>,
    body: PhantomData<ResB>,
}

impl<F, ResB, E> Future for CallFuture<F, ResB>
where
    F: Future<Output = Result<Response<ResB>, E>>,
    ResB: Body + Send + Unpin + 'static,
    ResB::Data: Send,
    ResB::Error: Send,
    E: Send + 'static,
{
    type Output = Result<Response<InstrumentedBody<ResB>>, E>;

    fn poll(mut self: Pin<&mut Self>, tcx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        // Attached per poll rather than held across awaits: a future can resume on
        // another thread, and a context guard that outlives a poll would restore the
        // wrong thread's context when it finally drops.
        let attached = self.state.as_ref().map(|state| state.cx.clone().attach());
        let poll = self.inner.as_mut().poll(tcx);
        drop(attached);

        let ready = match poll {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(ready) => ready,
        };
        // Taken rather than borrowed: both arms below move parts of it onward.
        let Some(state) = self.state.take() else {
            // Unreachable — `state` is only ever taken in the same poll that returns.
            return Poll::Ready(ready.map(|response| response.map(InstrumentedBody::passthrough)));
        };

        match ready {
            Err(error) => {
                // No response, so no status code and no measurement — the request never
                // became an RPC answer. The span still ends (with the context below) and
                // says why.
                state.cx.span().set_status(Status::error(
                    "the service failed before producing a response",
                ));
                Poll::Ready(Err(error))
            }
            Ok(response) => Poll::Ready(Ok(handle_head(response, state))),
        }
    }
}

/// Finish the head: attribute, then either end the span (success) or hand it to the body
/// wrapper (error, where the Connect code lives).
fn handle_head<ResB>(response: Response<ResB>, state: RpcState) -> Response<InstrumentedBody<ResB>>
where
    ResB: Body + Send + Unpin + 'static,
    ResB::Data: Send,
    ResB::Error: Send,
{
    let RpcState {
        cx,
        start,
        histogram,
        span_name,
    } = state;
    let status = response.status();
    // Scoped so the borrow of `cx` ends before it is moved below.
    {
        cx.span().set_attribute(KeyValue::new(
            attribute::HTTP_RESPONSE_STATUS_CODE,
            i64::from(status.as_u16()),
        ));
    }
    let base_attributes = vec![
        KeyValue::new(attribute::RPC_SYSTEM_NAME, CONNECT_RPC_SYSTEM),
        KeyValue::new(attribute::RPC_METHOD, span_name),
        KeyValue::new(
            attribute::HTTP_RESPONSE_STATUS_CODE,
            i64::from(status.as_u16()),
        ),
    ];

    let (parts, body) = response.into_parts();
    if status.is_success() {
        histogram.record(start.elapsed().as_secs_f64(), &base_attributes);
        // Ends the span: `cx` held the last context referencing it.
        drop(cx);
        return Response::from_parts(parts, InstrumentedBody::passthrough(body));
    }

    Response::from_parts(
        parts,
        InstrumentedBody::capturing(
            body,
            ErrorCapture {
                cx,
                start,
                histogram,
                attributes: base_attributes,
                status,
                captured: BytesMut::new(),
            },
        ),
    )
}

/// The Connect error code and message, read out of a captured unary error body.
///
/// Connect unary error bodies are JSON by specification regardless of the request codec,
/// shaped `{"code":"not_found","message":"..."}` — which is what makes this parse
/// well-defined rather than a heuristic. A truncated or absent body yields `None`, and
/// the span still says `Error` with the HTTP status.
fn connect_error(body: &[u8]) -> Option<(String, Option<String>)> {
    let document: serde_json::Value = serde_json::from_slice(body).ok()?;
    let code = document.get("code")?.as_str()?.to_owned();
    let message = document
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    Some((code, message))
}

/// The error-path bookkeeping the body wrapper finishes.
struct ErrorCapture {
    cx: Context,
    start: Instant,
    histogram: Histogram<f64>,
    attributes: Vec<KeyValue>,
    status: http::StatusCode,
    captured: BytesMut,
}

impl ErrorCapture {
    /// End the span and record the measurement. Runs exactly once: when the body ends,
    /// or from the wrapper's `Drop` when the client vanished mid-error — a span that
    /// waited for a body that never finished would otherwise never end at all.
    fn finish(self) {
        let span = self.cx.span();
        let mut attributes = self.attributes;
        match connect_error(&self.captured) {
            Some((code, message)) => {
                span.set_attribute(KeyValue::new(
                    attribute::RPC_RESPONSE_STATUS_CODE,
                    code.clone(),
                ));
                let mut event = vec![KeyValue::new(
                    attribute::RPC_RESPONSE_STATUS_CODE,
                    code.clone(),
                )];
                if let Some(message) = message {
                    event.push(KeyValue::new(attribute::EXCEPTION_MESSAGE, message));
                }
                span.add_event("connect error", event);
                span.set_status(Status::error(code.clone()));
                attributes.push(KeyValue::new(attribute::RPC_RESPONSE_STATUS_CODE, code));
            }
            None => {
                // No parsable code (truncated body, or a protocol whose errors this
                // layer does not decode). The HTTP status still says it failed.
                span.set_status(Status::error(format!("HTTP {}", self.status)));
            }
        }
        self.histogram
            .record(self.start.elapsed().as_secs_f64(), &attributes);
        // Ends the span: this held the last context referencing it.
    }
}

/// A response body that forwards untouched — and, on the error path only, captures
/// enough of itself to read the Connect error code before the span ends.
///
/// Success responses are never copied: the span ended at the head, and there is nothing
/// left to learn from the body.
pub struct InstrumentedBody<B> {
    inner: B,
    capture: Option<ErrorCapture>,
}

impl<B> InstrumentedBody<B> {
    const fn passthrough(inner: B) -> Self {
        Self {
            inner,
            capture: None,
        }
    }

    const fn capturing(inner: B, capture: ErrorCapture) -> Self {
        Self {
            inner,
            capture: Some(capture),
        }
    }

    /// Take and finish the capture, if it is still pending.
    fn finish_capture(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.finish();
        }
    }
}

impl<B: Body + Unpin> Body for InstrumentedBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        tcx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // `Self: Unpin` follows from `B: Unpin`, so field access through the pin is
        // sound without structural pinning — and unsafe is forbidden workspace-wide.
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_frame(tcx);

        if let Some(capture) = &mut this.capture {
            match &poll {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Some(chunk) = frame.data_ref()
                        && capture.captured.len() < CAPTURE_LIMIT
                    {
                        let remaining = CAPTURE_LIMIT - capture.captured.len();
                        let chunk = chunk.chunk();
                        capture
                            .captured
                            .extend_from_slice(&chunk[..remaining.min(chunk.len())]);
                    }
                    // Trailers end the stream; so does `Ready(None)` below. A data
                    // frame never does: more of the error document may follow.
                    if frame.is_trailers() {
                        this.finish_capture();
                    }
                }
                // The stream ended without an explicit end-of-stream frame.
                Poll::Ready(None) => this.finish_capture(),
                Poll::Pending | Poll::Ready(Some(Err(_))) => {}
            }
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for InstrumentedBody<B> {
    fn drop(&mut self) {
        // The client disconnected before the error body finished, or the body was never
        // polled at all. Finish with whatever was captured — a span that never ends is
        // worse than one whose error code is missing.
        self.finish_capture();
    }
}

#[cfg(test)]
// Holding `RECORDING_GLOBALS` across the whole async test body is the mechanism, not an
// oversight — see its doc comment. The lint's real target is a blocked runtime thread or a
// deadlock, and neither is reachable here: `#[tokio::test]` gives each test a
// current-thread runtime of its own, so no other task is waiting on the thread that holds
// the guard. Scoped to this module rather than per test because every test that records
// through the globals must hold it, so the exemption is the rule here.
#[expect(
    clippy::await_holding_lock,
    reason = "the guard serialises the tests that install global providers"
)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use bytes::Bytes;
    use opentelemetry::trace::{SpanId, TraceId};
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, Histogram as HistogramData, HistogramDataPoint, MetricData,
        ResourceMetrics, ScopeMetrics,
    };
    use opentelemetry_sdk::metrics::{
        InMemoryMetricExporter, InMemoryMetricExporterBuilder, PeriodicReader, SdkMeterProvider,
    };
    use opentelemetry_sdk::trace::{
        InMemorySpanExporter, InMemorySpanExporterBuilder, SdkTracerProvider, SpanData,
    };
    use tower::ServiceExt as _;

    use super::*;

    /// The W3C example identifiers: a caller's trace this test's request must land in.
    const PARENT_TRACE_ID: TraceId =
        TraceId::from_bytes(0x4bf9_2f35_77b3_4da6_a3ce_929d_0e0e_4736_u128.to_be_bytes());
    const PARENT_SPAN_ID: SpanId = SpanId::from_bytes(0x00f0_67aa_0ba9_02b7_u64.to_be_bytes());

    /// Serialises the tests that record through the global providers.
    ///
    /// `set_tracer_provider` and `set_meter_provider` are process-wide and
    /// last-writer-wins, and the layer resolves `global::tracer` per request. So two tests
    /// installing their own recorders at once leave the loser holding an exporter the
    /// layer no longer writes to: it records nothing and fails on an empty `Vec`, and
    /// *which* test loses depends on scheduling.
    ///
    /// nextest gives every test its own process, which hides this completely — `just
    /// rust-test` is green either way. Plain `cargo test -p telemetry --lib --features
    /// server` is where it bites, and that is what someone iterating on one test runs, so
    /// the suite has to work there too.
    ///
    /// A test binary is one process, so this only ever serialises the tests in this
    /// module. That is all it needs to do, and it is why `services/api` carries a lock of
    /// its own rather than sharing this one.
    static RECORDING_GLOBALS: Mutex<()> = Mutex::new(());

    /// Install global providers that keep what they are given, for the duration of one
    /// test, and hold [`RECORDING_GLOBALS`] until the returned guard is dropped.
    ///
    /// The guard is the last element rather than a field of a struct so that a caller
    /// which ignores it cannot: binding it to `_` would drop it immediately and release
    /// the lock while the test is still running, so each test names it `_globals`.
    fn install_recording_globals() -> (
        InMemorySpanExporter,
        InMemoryMetricExporter,
        SdkMeterProvider,
        MutexGuard<'static, ()>,
    ) {
        // A test that fails while holding this must not turn its neighbours into poison
        // errors: that would replace one honest failure with several that name nothing
        // about why they failed.
        let guard = RECORDING_GLOBALS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        let spans = InMemorySpanExporterBuilder::new().build();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();
        global::set_tracer_provider(tracer_provider);

        let metrics = InMemoryMetricExporterBuilder::new().build();
        let reader = PeriodicReader::builder(metrics.clone())
            // Never fires on its own: the tests force-flush, and a live timer would
            // make them race themselves.
            .with_interval(std::time::Duration::from_secs(3600))
            .build();
        let meter_provider = SdkMeterProvider::builder().with_reader(reader).build();
        global::set_meter_provider(meter_provider.clone());

        (spans, metrics, meter_provider, guard)
    }

    fn span_in_trace(spans: &[SpanData]) -> Option<&SpanData> {
        spans
            .iter()
            .find(|span| span.span_context.trace_id() == PARENT_TRACE_ID)
    }

    fn attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| &kv.value)
    }

    /// A service that answers every request with the given status and JSON body.
    ///
    /// A named struct rather than `tower::service_fn`: an opaque `impl Service`
    /// return type hides its `Future` associated type, and the layer's `Future: Send`
    /// bound then cannot be checked at the call site.
    #[derive(Clone)]
    struct Echo {
        status: http::StatusCode,
        body: &'static str,
    }

    impl Service<Request<Bytes>> for Echo {
        type Response = Response<FullBody>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

        fn poll_ready(&mut self, _tcx: &mut TaskContext<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: Request<Bytes>) -> Self::Future {
            let response = Response::builder()
                .status(self.status)
                .header("content-type", "application/json")
                .body(FullBody(Bytes::from_static(self.body.as_bytes())))
                .expect("a valid response");
            Box::pin(std::future::ready(Ok(response)))
        }
    }

    /// The minimal concrete body the tests drive: one data frame, then the end.
    /// Hand-rolled because the layer must not depend on `http-body-util` for anything
    /// but a test.
    struct FullBody(Bytes);

    impl Body for FullBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _tcx: &mut TaskContext<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            if self.0.is_empty() {
                return Poll::Ready(None);
            }
            Poll::Ready(Some(Ok(Frame::data(std::mem::take(&mut self.0)))))
        }

        fn is_end_stream(&self) -> bool {
            self.0.is_empty()
        }
    }

    /// Read a body to completion, the way a client would.
    async fn drain<B: Body<Error = Infallible> + Unpin>(mut body: B) {
        loop {
            let frame = std::future::poll_fn(|tcx| Pin::new(&mut body).poll_frame(tcx)).await;
            match frame {
                None => break,
                Some(Ok(_)) => {}
                Some(Err(infallible)) => match infallible {},
            }
        }
    }

    /// A unary Connect request to `Live`, carrying the caller's `traceparent`.
    fn live_request() -> Request<Bytes> {
        Request::builder()
            .method("POST")
            .uri("/api/health.v1.HealthService/Live")
            .header("content-type", "application/json")
            .header(
                "traceparent",
                format!("00-{PARENT_TRACE_ID}-{PARENT_SPAN_ID}-01"),
            )
            .body(Bytes::from_static(b"{}"))
            .expect("a valid request")
    }

    #[test]
    fn connect_paths_split_into_service_and_method() {
        assert_eq!(
            connect_method("/api/counter.v1.CounterService/Increment"),
            Some(("counter.v1.CounterService", "Increment"))
        );
        // Mounted at the root: the nest strips the prefix before the service sees it.
        assert_eq!(
            connect_method("/health.v1.HealthService/Live"),
            Some(("health.v1.HealthService", "Live"))
        );
        assert_eq!(connect_method("/index.html"), None);
        assert_eq!(connect_method("/api/static/app.js"), None);
        assert_eq!(connect_method("/api/"), None);
        assert_eq!(connect_method("/"), None);
    }

    #[tokio::test]
    async fn a_connect_request_becomes_one_span_named_after_the_method() {
        let (spans, metrics, meter_provider, _globals) = install_recording_globals();

        let service = RpcLayer.layer(Echo {
            status: http::StatusCode::OK,
            body: "{}",
        });
        let response = service.oneshot(live_request()).await.expect("infallible");
        assert_eq!(response.status(), http::StatusCode::OK);
        drain(response.into_body()).await;

        let finished = spans.get_finished_spans().expect("readable");
        let span = span_in_trace(&finished).expect("the request produced a span");

        // The span says RPC, not HTTP — the whole reason TraceLayer was replaced.
        assert_eq!(span.name, "health.v1.HealthService/Live");
        // … and it is a child of the caller's trace, not a fresh root.
        assert_eq!(span.parent_span_id, PARENT_SPAN_ID);
        assert!(span.parent_span_is_remote);
        assert_eq!(
            attribute(span, attribute::RPC_SYSTEM_NAME),
            Some(&opentelemetry::Value::from(CONNECT_RPC_SYSTEM))
        );
        assert_eq!(
            attribute(span, attribute::RPC_METHOD),
            Some(&opentelemetry::Value::from("health.v1.HealthService/Live"))
        );
        assert_eq!(
            attribute(span, attribute::HTTP_RESPONSE_STATUS_CODE),
            Some(&opentelemetry::Value::from(200_i64))
        );

        meter_provider.force_flush().expect("flush works");
        let recorded = metrics.get_finished_metrics().expect("readable");
        let points = duration_points(&recorded);
        assert_eq!(points.len(), 1, "one RPC, one measurement");
        assert_eq!(points[0].count(), 1);
        assert!(
            points[0]
                .attributes()
                .any(|kv| kv.key.as_str() == attribute::RPC_METHOD)
        );
    }

    #[tokio::test]
    async fn an_error_response_ends_the_span_with_the_connect_code() {
        let (spans, metrics, meter_provider, _globals) = install_recording_globals();

        let service = RpcLayer.layer(Echo {
            status: http::StatusCode::NOT_FOUND,
            body: r#"{"code":"unimplemented","message":"unknown method"}"#,
        });
        let response = service.oneshot(live_request()).await.expect("infallible");
        assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
        // The code lives in the body: a client reads it, so the test must too — the
        // span cannot end before the document it is read from exists.
        drain(response.into_body()).await;

        let finished = spans.get_finished_spans().expect("readable");
        let span = span_in_trace(&finished).expect("the request produced a span");
        assert!(
            matches!(&span.status, Status::Error { .. }),
            "the span status is Error: {:?}",
            span.status
        );
        assert_eq!(
            attribute(span, attribute::RPC_RESPONSE_STATUS_CODE),
            Some(&opentelemetry::Value::from("unimplemented"))
        );
        assert!(
            span.events.iter().any(|event| event.name == "connect error"
                && event
                    .attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == attribute::EXCEPTION_MESSAGE
                        && kv.value.as_str() == "unknown method")),
            "the error is also an event with the message: {:?}",
            span.events
        );

        meter_provider.force_flush().expect("flush works");
        let recorded = metrics.get_finished_metrics().expect("readable");
        let points = duration_points(&recorded);
        assert_eq!(points.len(), 1);
        // The error code rides on the measurement too: error *rate* by code is a query
        // over this one instrument.
        assert!(
            points[0]
                .attributes()
                .any(|kv| kv.key.as_str() == attribute::RPC_RESPONSE_STATUS_CODE
                    && kv.value.as_str() == "unimplemented")
        );
    }

    #[tokio::test]
    async fn an_error_body_nobody_reads_still_ends_its_span() {
        let (spans, _metrics, _meter_provider, _globals) = install_recording_globals();

        let service = RpcLayer.layer(Echo {
            status: http::StatusCode::INTERNAL_SERVER_ERROR,
            body: r#"{"code":"internal","message":"boom"}"#,
        });
        let response = service.oneshot(live_request()).await.expect("infallible");
        // The client vanished: the body is dropped unread. The span must still end —
        // with the HTTP status, since the code was never produced — or it never ends
        // at all and the trace hangs open forever.
        drop(response);

        let finished = spans.get_finished_spans().expect("readable");
        let span = span_in_trace(&finished).expect("the span ended even unread");
        assert!(matches!(&span.status, Status::Error { .. }));
        assert_eq!(attribute(span, attribute::RPC_RESPONSE_STATUS_CODE), None);
    }

    #[tokio::test]
    async fn a_path_that_is_not_an_rpc_gets_no_span() {
        let (spans, _metrics, _meter_provider, _globals) = install_recording_globals();

        let service = RpcLayer.layer(Echo {
            status: http::StatusCode::OK,
            body: "not json",
        });
        let request = Request::builder()
            .uri("/index.html")
            .header(
                "traceparent",
                format!("00-{PARENT_TRACE_ID}-{PARENT_SPAN_ID}-01"),
            )
            .body(Bytes::new())
            .expect("a valid request");
        let response = service.oneshot(request).await.expect("infallible");
        drain(response.into_body()).await;

        let finished = spans.get_finished_spans().expect("readable");
        assert!(
            span_in_trace(&finished).is_none(),
            "static files are HTTP requests, not RPCs: one span per RPC, not per request"
        );
    }

    /// The data points of every `rpc.server.call.duration` histogram in the export.
    fn duration_points(recorded: &[ResourceMetrics]) -> Vec<&HistogramDataPoint<f64>> {
        recorded
            .iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .filter(|metric| metric.name() == metric::RPC_SERVER_CALL_DURATION)
            .filter_map(|metric| match metric.data() {
                AggregatedMetrics::F64(MetricData::Histogram(histogram)) => Some(histogram),
                _ => None,
            })
            .flat_map(HistogramData::data_points)
            .collect()
    }
}
