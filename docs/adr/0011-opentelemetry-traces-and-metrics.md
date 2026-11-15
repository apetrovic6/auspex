# 0011. OpenTelemetry traces and metrics, off unless an endpoint is configured

- **Status:** Accepted
- **Date:** 2026-09-25

## Context

`crates/telemetry` was `tracing-subscriber` and nothing else: two log formats, one filter,
a panic hook. No service exported a trace or a metric, so a slow RPC was a log line and a
request that crossed the TUI shell, the API and the database was three unrelated log lines
with nothing joining them. The brief asked for distributed traces and RED metrics — rate,
errors, duration — with two hard constraints: logs stay exactly as they are, and the whole
thing is **off by default**, because a `cargo run` with no collector anywhere must behave
byte-for-byte as it does today and CI must not grow a telemetry dependency it does not use.

One fact shapes everything else. §12 gives `apps/counter/client` an `opentelemetry`
dependency, and that crate is compiled to `wasm32-unknown-unknown` by the web shell — its
own manifest says so, and `deny.toml` lists wasm32 as a first-class target. So the
client's dependency tree must stay wasm-clean. The OpenTelemetry **API** crate compiles for
wasm; the SDK, the OTLP exporter and tonic do not.

## Decision

`crates/telemetry` owns the OpenTelemetry pipeline, and it is inert until an endpoint
appears.

- **`http/protobuf`, never gRPC.** `opentelemetry-otlp` with the `http-proto` transport.
  This keeps the non-wasm-capable half of the tree (SDK, exporter, reqwest) confined to
  `telemetry`'s native side and out of anything the web shell compiles. gRPC/tonic cannot
  compile for wasm at all, so choosing it would put a wall between the client crate and the
  API crate it is allowed to depend on.
- **Off unless configured.** `init` resolves one endpoint — `telemetry.otlp.endpoint`,
  else `OTEL_EXPORTER_OTLP_ENDPOINT` — and with neither set installs only the log layer:
  no SDK, no exporter threads, no global providers. A configured endpoint turns on traces
  and metrics together. Config wins over the standard `OTEL_*` env vars, which are the
  operator's escape hatch, not a second source of truth (§4).
- **Guard-based shutdown.** `init` returns a `#[must_use] TelemetryGuard` whose `Drop`
  flushes **both** the tracer and meter providers. A batch exporter holds the last
  request's spans in a buffer; without the flush, process exit drops exactly the
  interesting ones. The binding is load-bearing — `let _telemetry = …` — and
  `cargo xtask check` scans for the `let _ = …` shape that satisfies `#[must_use]` yet
  flushes nothing.
- **Resource attributes** `service.name`, `service.version`, `deployment.environment.name`
  — the current semconv spelling, not the retired `deployment.environment`. Name and
  version come from each binary's own `CARGO_PKG_*` (the macros expand in the caller's
  compilation unit, so `telemetry` could never fill them in itself); environment falls back
  to `settings::profile()`, which is why `telemetry` depends on `settings` rather than
  duplicating the `APP_PROFILE` concept.
- **One span per RPC, from a custom layer that replaces `TraceLayer`.** `tower-http`'s
  `TraceLayer::new_for_http()` spans `POST /api/…`; the server layer spans
  `counter.v1.CounterService/Increment` with the RPC semconv attributes, adopts the inbound
  W3C `traceparent` as its parent, records the Connect error code from the (unary,
  spec-required-JSON) error body, and emits the same identifying attributes on a single
  `rpc.server.call.duration` histogram — one instrument, because a histogram carries its
  own count, so rate and error-rate are queries over it rather than three time series per
  method. Keeping `TraceLayer` alongside it would double-span every request.
- **Propagation in the native client only.** `TracingTransport` wraps the native transport
  and writes `traceparent` from the current context, using the API crate alone and hand-
  rolling the W3C header — the SDK's propagator would drag the non-wasm half into a client
  crate. The web shell's `FetchTransport` is deliberately untouched (§6b).
- **A collector is the default export target; Jaeger is opt-in.** `nix/observability.nix`
  runs devenv's OpenTelemetry Collector (core, not contrib — 189 MiB against 381 MiB, and
  it lands in the CI toolchain layer that is invalidated roughly never) with a `debug`
  exporter, so `devenv up` prints spans with no backend. The endpoint reaches a service
  through `processes.<name>.env`, never a shell-level `env.*`, so it exists exactly for
  what `devenv up` runs and cannot leak into `devenv shell`, a test or CI. Jaeger
  (`observability.jaeger.enable`, default off) adds the waterfall the debug exporter cannot
  show, behind the same collector seam.

## Consequences

A service that reaches an OTLP endpoint — in practice the process-scoped
`APP__TELEMETRY__OTLP__ENDPOINT` that `devenv up` sets, or the same key from committed
config, or `OTEL_EXPORTER_OTLP_ENDPOINT` in a deployment — exports traces and metrics. A
service with no endpoint behaves exactly as it did before this record, which is what keeps
every existing test and CI job untouched: they run the log-only path by construction, not
by luck, because the endpoint is scoped to a devenv process none of them runs.

The core stays domain-level (ADR 0002): no trace id enters the core's API enum or any other
core type;
propagation lives in the client transport and the server layer. A scaffolded service or
worker is observable under `devenv up` the day it is scaffolded and silent everywhere else.

The honest cost: what ships by default is a span in the collector's log, **not** a
waterfall. The UI is one toggle away, and "nobody ever looks at it" is how instrumentation
rots — so the trade is recorded here rather than buried, and the day-one demo is weaker
than Jaeger-by-default would have been.

Two limits are stated where they live, not hidden. The web shell establishes no OTel
context, so its requests arrive at the server **unparented**; end-to-end traces cover the
TUI and desktop shells only, until a shell exports spans of its own (deferred, §6b/§14).
And the server layer reads the Connect error code from a unary JSON body, so streaming and
gRPC errors — which ride inside HTTP 200 — are seen as successes; unary is all this
repository speaks today.

Verification, not assertion, is what made this record: the end-to-end `devenv up` check
(§13 step 9) caught two defects the in-memory-exporter tests structurally could not, since
they never touch reqwest. The async `reqwest-client` exporter, blocked on a thread-based
processor's own OS thread, panicked with "no reactor running" — so the exporter uses the
**blocking** client, which owns its runtime thread and keeps `init` callable from any
`main`, tokio or not. And a programmatically set endpoint is used verbatim (the crate
appends `/v1/traces` only to the env var), so `init` appends the signal path itself;
without it the collector answers the root with 404 and the spans vanish behind a WARN.

## What was rejected, and why

**gRPC/tonic transport.** Cannot compile for wasm, so it would wall the client crate off
from the one OpenTelemetry crate it is allowed to depend on (§1).

**The OTel logs bridge.** The least settled of the three signals, and `Format::Json`
already exists for log shipping. Traces and metrics are the ask; a third, half-baked signal
is not worth the coupling.

**A `telemetry::shutdown()` free function.** It would work, but it is a second thing to
remember that can be forgotten. A guard that flushes on drop cannot be — the variable
binding is the reminder.

**A tracer-only guard.** Passes every trace test and still silently drops buffered
*metrics* at exit — the exact failure the guard exists to prevent, for half the signals,
invisible until someone compares counts.

**SeaORM query spans.** One span per query is real cardinality for little value when the
RPC span's duration already covers the queries inside it. Deferred; a profile override can
enable the feature if a specific latency investigation needs it (§5).

**Synthesising a root `traceparent` in `FetchTransport`.** The web shell has no context to
propagate, so injecting one would fabricate a root the client never recorded — which reads
in a backend as a broken exporter, worse than an honest absence. The gap is documented
instead (§6b).

**Keeping `TraceLayer` beside the OTel layer.** Two spans per request and every dashboard
double-counts. The HTTP-shaped one goes (§2).

**Honouring only the `OTEL_*` env vars, or only our config.** Env-only throws away the
committed config as a reviewable primary source; config-only throws away the industry-
standard operator escape hatch. Config-wins-with-env-fallback keeps both (§4).

**Jaeger as a nixpkgs package.** It was dropped from nixpkgs — verified against the pinned
devenv-nixpkgs revision — and the v1 `--set` flags no longer exist in v2. This is why the
opt-in UI runs under podman (§10).

**Jaeger as the default local backend.** Podman's measured 7300 MiB closure would sit in
the CI toolchain layer against the core collector's 189 MiB, first `devenv up` would pull a
container image, macOS would need a podman machine VM, and app-exports-straight-to-a-
backend is not the shape products deploy — they export to a collector. Jaeger survives as
the opt-in UI; the weaker day-one demo is the recorded cost (§10, and above).

**The deprecated semconv spellings and the bare `otlp` exporter alias.** `rpc.system`,
`rpc.service`, `rpc.connect.error_code` and `rpc.server.duration` (ms) are retired in
`opentelemetry-semantic-conventions` 0.33, and the collector deprecates `otlp` for
`otlp_grpc`. Using the retired names fails `-D warnings` or drifts from what a backend's
RPC views match on; the whole reason the semconv crate is in the tree is that a hand-typed
attribute that drifts is invisible. The current spellings — `rpc.system.name`,
`rpc.response.status_code`, `rpc.server.call.duration` (s), `otlp_grpc/jaeger` — are used
throughout, each verified against the pinned versions.
