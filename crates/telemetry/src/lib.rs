//! Process-wide tracing and telemetry setup.
//!
//! One place decides how a process emits diagnostics, so that logs from every service in
//! the monorepo are shaped the same way and a filter learned once works everywhere.
//!
//! Two output formats, chosen by configuration rather than by build profile: `Pretty` for
//! a terminal, `Json` for anything that ships logs to a collector. Build profile is the
//! wrong signal — a release binary run locally still wants readable output, and a debug
//! build in a cluster still wants JSON.
//!
//! On top of logs, this crate exports traces and metrics over OTLP — but only when an
//! endpoint is configured ([`OtlpConfig::endpoint`], or the standard
//! `OTEL_EXPORTER_OTLP_ENDPOINT`). With no endpoint, `init` installs the log layer and
//! nothing else: no OpenTelemetry SDK, no exporter threads, no behavioural change. The transport is
//! `http/protobuf`, never gRPC: this crate's API surface is reachable from crates that
//! compile to wasm32 (the counter app's `client`), and the SDK, the OTLP exporter and
//! tonic do not compile for wasm — ADR 0011 records the decision.
//!
//! `init` returns a [`TelemetryGuard`]. A batch exporter holds unflushed spans in an
//! internal buffer; the guard's `Drop` flushes both pipelines, so the last request's
//! spans — usually the interesting ones — survive process exit. The binding is
//! load-bearing: `let _telemetry = telemetry::init(..)?;`, never `let _ = ..`.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig as _};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::resource::Resource;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use opentelemetry_semantic_conventions::resource::{DEPLOYMENT_ENVIRONMENT_NAME, SERVICE_VERSION};
use serde::Deserialize;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Layer};

#[cfg(feature = "server")]
pub mod http_layer;

/// Environment variable consulted for the filter directive, if the configured one is
/// empty. Named `RUST_LOG` because that is what every Rust operator already tries first.
pub const FILTER_VAR: &str = "RUST_LOG";

/// Filter applied when neither configuration nor [`FILTER_VAR`] says otherwise.
pub const DEFAULT_FILTER: &str = "info";

/// OTLP endpoint, standard OpenTelemetry spelling.
///
/// Consulted only when [`OtlpConfig::endpoint`] is empty: the committed config is the
/// primary source, the env var is the escape hatch for operators whose deployments
/// follow OpenTelemetry conventions rather than this repo's.
pub const OTLP_ENDPOINT_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Service name, standard OpenTelemetry spelling. Consulted only when
/// [`OtlpConfig::service_name`] is empty.
pub const SERVICE_NAME_VAR: &str = "OTEL_SERVICE_NAME";

/// Deployment environment, standard OpenTelemetry spelling. Consulted only when
/// [`OtlpConfig::environment`] is empty.
pub const ENVIRONMENT_VAR: &str = "OTEL_DEPLOYMENT_ENVIRONMENT";

/// Sampler, standard OpenTelemetry spelling. Consulted only when [`OtlpConfig::sampler`] is empty.
pub const SAMPLER_VAR: &str = "OTEL_TRACES_SAMPLER";

/// Ratio argument for `traceidratio` samplers chosen through [`SAMPLER_VAR`].
pub const SAMPLER_ARG_VAR: &str = "OTEL_TRACES_SAMPLER_ARG";

/// The wire protocol every exporter here is built with, stated programmatically rather
/// than left to `OTEL_EXPORTER_OTLP_PROTOCOL`: ADR 0011 chose `http/protobuf` because the
/// gRPC half of the tree cannot compile for wasm, and an env var in some deployment must
/// not silently re-litigate that.
const OTLP_PROTOCOL: Protocol = Protocol::HttpBinary;

/// How diagnostics are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// Human-readable, for a terminal.
    #[default]
    Pretty,
    /// One JSON object per event, for log shipping.
    Json,
}

/// Telemetry settings, normally deserialized as part of a service's configuration.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    /// Output format.
    #[serde(default)]
    pub format: Format,
    /// `tracing-subscriber` filter directive, e.g. `info,api=debug`. Empty falls back to
    /// [`FILTER_VAR`], then [`DEFAULT_FILTER`].
    #[serde(default)]
    pub filter: String,
    /// OTLP export. Off by default: an empty endpoint installs no OpenTelemetry SDK at all.
    #[serde(default)]
    pub otlp: OtlpConfig,
}

/// OTLP export settings.
///
/// Precedence, per field: the configured value (which `settings::load` has already merged from
/// committed TOML, profile TOML and `APP__*` env vars) wins; where it is empty, the
/// standard `OTEL_*` environment variable gets a turn; where that is also empty, the
/// field's documented fallback applies. The `OTEL_*` vars are the industry-standard
/// escape hatch — an operator can redirect a deployment without editing config files —
/// while the committed config stays the primary source.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OtlpConfig {
    /// OTLP exporter endpoint. Empty means disabled — no exporter, no extra threads,
    /// no behavioural change from a logs-only setup. Falls back to
    /// [`OTLP_ENDPOINT_VAR`].
    #[serde(default)]
    pub endpoint: String,
    /// Value for the `service.name` resource attribute. Falls back to
    /// [`SERVICE_NAME_VAR`], then to [`ServiceInfo::name`].
    #[serde(default)]
    pub service_name: String,
    /// Value for the `deployment.environment.name` resource attribute. Falls back to
    /// [`ENVIRONMENT_VAR`], then to the active config profile (`settings::profile()` —
    /// `APP_PROFILE`, default `local`). Declared here because that chain names
    /// `otlp.environment` as its first link, and a precedence chain whose first link is
    /// not a field cannot be honoured.
    #[serde(default)]
    pub environment: String,
    /// Sampler: `"always_on"`, `"always_off"`, or `"trace_id_ratio:<f64>"` (the standard
    /// OpenTelemetry spellings — `traceidratio`, `parentbased_*` — are accepted too). Falls back to
    /// [`SAMPLER_VAR`] (with [`SAMPLER_ARG_VAR`] as the ratio), then to `"always_on"` —
    /// a configured endpoint is an explicit wish to export.
    #[serde(default)]
    pub sampler: String,
}

/// What the process calls itself in exported telemetry.
///
/// Passed by each binary from its *own* `env!("CARGO_PKG_NAME")` /
/// `env!("CARGO_PKG_VERSION")`: the macros expand in the compilation unit that writes
/// them, so telemetry could never fill these in itself — it would always report
/// "telemetry".
#[derive(Debug, Clone, Copy)]
pub struct ServiceInfo {
    /// Crate name of the binary, e.g. `env!("CARGO_PKG_NAME")`.
    pub name: &'static str,
    /// Crate version of the binary, e.g. `env!("CARGO_PKG_VERSION")`.
    pub version: &'static str,
}

/// Flushes buffered spans and metrics when dropped.
///
/// Hold it for the lifetime of the process — `let _telemetry = telemetry::init(..)?;` in
/// `main`. Binding it to the bare wildcard `_` drops it immediately and flushes nothing,
/// which is why `cargo xtask check` scans for exactly that shape.
#[must_use = "the guard flushes buffered spans and metrics on drop; binding it to `_` \
              or discarding it loses the last batch"]
pub struct TelemetryGuard {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
}

impl TelemetryGuard {
    /// A guard with nothing to flush — what `init` returns when OTLP export is off.
    pub const fn disabled() -> Self {
        Self {
            tracer: None,
            meter: None,
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // Metrics first, then traces. The two pipelines are independent — the RED
        // instruments fire during request handling, never during a flush — so either
        // order is correct; the order is fixed so shutdown is deterministic and so
        // nobody re-argues it. Best-effort: log on failure, never panic in Drop.
        //
        // The partial interval is not lost: the SDK's PeriodicReader performs a final
        // collect-and-export on SdkMeterProvider::shutdown(). If a future change swaps in
        // a manual reader, that stops being true and this must call force_flush too.
        if let Some(meter) = self.meter.take()
            && let Err(error) = meter.shutdown()
        {
            eprintln!("telemetry: metrics shutdown failed: {error}");
        }
        if let Some(tracer) = self.tracer.take()
            && let Err(error) = tracer.shutdown()
        {
            eprintln!("telemetry: trace shutdown failed: {error}");
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The filter directive was not valid.
    #[error("invalid tracing filter {filter:?}")]
    Filter {
        filter: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Tracing was already initialised. Initialisation is process-wide and once-only.
    #[error("tracing has already been initialised for this process")]
    AlreadyInitialised,
    /// An OTLP exporter could not be built for the resolved endpoint.
    #[error("could not build the OTLP exporter for {endpoint:?}")]
    Otlp {
        endpoint: String,
        #[source]
        source: opentelemetry_otlp::ExporterBuildError,
    },
    /// A sampler specification was not one this crate understands.
    #[error("invalid sampler specification {spec:?}")]
    Sampler { spec: String },
}

impl Config {
    /// The filter directive actually in force, after fallbacks.
    #[must_use]
    pub fn effective_filter(&self) -> String {
        if !self.filter.trim().is_empty() {
            return self.filter.clone();
        }
        std::env::var(FILTER_VAR).unwrap_or_else(|_| DEFAULT_FILTER.to_owned())
    }
}

/// An environment variable's value, or `None` when unset or blank.
fn env_non_empty(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The first non-blank candidate, else the fallback. Every resource attribute and the
/// endpoint resolve through this one chain, so "config wins, env is the escape hatch"
/// has exactly one implementation.
fn first_non_empty(candidates: &[Option<String>], fallback: impl FnOnce() -> String) -> String {
    candidates
        .iter()
        .flatten()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .map_or_else(fallback, ToOwned::to_owned)
}

/// The base endpoint to export to; empty means OTLP export is disabled.
///
/// This is the *base* (`http://host:4318`), not a per-signal URL — [`signal_endpoint`]
/// appends `/v1/traces` and `/v1/metrics` to it, mirroring `OTEL_EXPORTER_OTLP_ENDPOINT`.
fn resolve_endpoint(config: &Config) -> String {
    first_non_empty(
        &[
            config.otlp.endpoint.clone().into(),
            env_non_empty(OTLP_ENDPOINT_VAR),
        ],
        String::new,
    )
}

/// Append an OTLP signal path to the base endpoint.
///
/// The exporter uses a *programmatically* set endpoint verbatim — it treats `with_endpoint`
/// as a complete per-signal URL and appends nothing — whereas it appends `/v1/<signal>` to
/// the `OTEL_EXPORTER_OTLP_ENDPOINT` env var. Since `init` resolves config-then-env into
/// one base and passes it programmatically (so config can win), it must append the signal
/// path itself. Skipping this is a silent 404: the collector's OTLP/HTTP receiver serves
/// `/v1/traces` and `/v1/metrics`, not the root, and answers the root with 404 — the
/// exporter retries, gives up, and the spans vanish with only a WARN in the log. This
/// replicates the crate's own `build_endpoint_uri`, trailing slash and all.
fn signal_endpoint(base: &str, path: &str) -> String {
    let path = if base.ends_with('/') && path.starts_with('/') {
        &path[1..]
    } else {
        path
    };
    format!("{base}{path}")
}

/// Parse one sampler specification.
///
/// Accepts the spellings documented on [`OtlpConfig::sampler`] plus the standard OpenTelemetry
/// env-var spellings (`traceidratio`, `parentbased_*`), because the same parser serves
/// [`SAMPLER_VAR`]. `arg` is [`SAMPLER_ARG_VAR`], used when the spec carries no inline
/// ratio.
fn parse_sampler(spec: &str, arg: Option<&str>) -> Result<Sampler, Error> {
    let invalid = || Error::Sampler {
        spec: spec.to_owned(),
    };
    let (name, inline) = match spec.split_once(':') {
        Some((name, ratio)) => (name.trim(), Some(ratio.trim())),
        None => (spec.trim(), None),
    };
    let ratio = || {
        inline
            .or(arg)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|p| (0.0..=1.0).contains(p))
            .ok_or_else(invalid)
    };
    match name {
        "always_on" => Ok(Sampler::AlwaysOn),
        "always_off" => Ok(Sampler::AlwaysOff),
        "traceidratio" | "trace_id_ratio" => Ok(Sampler::TraceIdRatioBased(ratio()?)),
        "parentbased_always_on" => Ok(Sampler::ParentBased(Box::new(Sampler::AlwaysOn))),
        "parentbased_always_off" => Ok(Sampler::ParentBased(Box::new(Sampler::AlwaysOff))),
        "parentbased_traceidratio" | "parentbased_trace_id_ratio" => Ok(Sampler::ParentBased(
            Box::new(Sampler::TraceIdRatioBased(ratio()?)),
        )),
        _ => Err(invalid()),
    }
}

/// The sampler in force: configured spec, else [`SAMPLER_VAR`], else `always_on` —
/// called only once an endpoint resolved, and a configured endpoint is an explicit wish
/// to export.
fn resolve_sampler(config: &OtlpConfig) -> Result<Sampler, Error> {
    let configured = config.sampler.trim();
    let spec = if configured.is_empty() {
        env_non_empty(SAMPLER_VAR).unwrap_or_else(|| "always_on".to_owned())
    } else {
        configured.to_owned()
    };
    let arg = env_non_empty(SAMPLER_ARG_VAR);
    parse_sampler(&spec, arg.as_deref())
}

/// The subscriber stack both `init` branches share before the format layer: the registry
/// with the resolved [`EnvFilter`] applied. Naming it keeps the boxed format layer's type
/// from being spelled out twice.
type FilteredRegistry = tracing_subscriber::layer::Layered<EnvFilter, tracing_subscriber::Registry>;

/// Install the global tracing subscriber, and the OTLP pipelines when configured.
///
/// Call once, early, before anything worth tracing. Returns an error rather than panicking
/// so a caller can decide — a subscriber already being installed is normal in tests.
///
/// Bind the returned guard for the lifetime of the process; see [`TelemetryGuard`].
///
/// # Errors
///
/// Returns [`Error::Filter`] if the directive does not parse, [`Error::Otlp`] if an
/// exporter cannot be built for the resolved endpoint, [`Error::Sampler`] if the sampler
/// specification is not understood, or [`Error::AlreadyInitialised`] if a global
/// subscriber is already installed.
pub fn init(config: &Config, service: ServiceInfo) -> Result<TelemetryGuard, Error> {
    let directive = config.effective_filter();
    let filter = EnvFilter::try_new(&directive).map_err(|source| Error::Filter {
        filter: directive,
        source: Box::new(source),
    })?;

    // Boxed so the format choice is one value rather than two parallel stacks; the box
    // is typed against the registry-plus-filter stack both branches share. The OTel and
    // no-OTel branches still end in different concrete types, so each gets its own
    // `try_init`.
    let fmt: Box<dyn Layer<FilteredRegistry> + Send + Sync> = match config.format {
        Format::Pretty => Box::new(
            tracing_subscriber::fmt::layer()
                .pretty()
                .with_span_events(FmtSpan::CLOSE),
        ),
        Format::Json => Box::new(
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_events(FmtSpan::CLOSE),
        ),
    };
    let registry = tracing_subscriber::registry().with(filter);

    let endpoint = resolve_endpoint(config);
    if endpoint.is_empty() {
        // Off unless configured: no OTel SDK, no exporter threads — byte-identical
        // behaviour to a logs-only setup.
        registry
            .with(fmt)
            .try_init()
            .map_err(|_| Error::AlreadyInitialised)?;
        return Ok(TelemetryGuard::disabled());
    }

    let environment = first_non_empty(
        &[
            config.otlp.environment.clone().into(),
            env_non_empty(ENVIRONMENT_VAR),
        ],
        // The profile concept stays single-sourced in `settings` — the APP_PROFILE var
        // name is not duplicated here. Every binary that links telemetry already links
        // settings, so the dependency adds nothing to any consumer's tree.
        settings::profile,
    );
    let service_name = first_non_empty(
        &[
            config.otlp.service_name.clone().into(),
            env_non_empty(SERVICE_NAME_VAR),
        ],
        || service.name.to_owned(),
    );
    // `deployment.environment.name`, the current semconv spelling — the constant, not a
    // literal, because the retired `deployment.environment` is exactly the drift the
    // constants exist to prevent.
    let resource = Resource::builder()
        .with_service_name(service_name)
        .with_attribute(KeyValue::new(SERVICE_VERSION, service.version))
        .with_attribute(KeyValue::new(DEPLOYMENT_ENVIRONMENT_NAME, environment))
        .build();

    let span_exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(signal_endpoint(&endpoint, "/v1/traces"))
        .with_protocol(OTLP_PROTOCOL)
        .build()
        .map_err(|source| Error::Otlp {
            endpoint: endpoint.clone(),
            source,
        })?;
    let tracer_provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_sampler(resolve_sampler(&config.otlp)?)
        .with_batch_exporter(span_exporter)
        .build();

    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(signal_endpoint(&endpoint, "/v1/metrics"))
        .with_protocol(OTLP_PROTOCOL)
        .build()
        .map_err(|source| Error::Otlp { endpoint, source })?;
    let meter_provider = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(PeriodicReader::builder(metric_exporter).build())
        .build();

    let guard = TelemetryGuard {
        tracer: Some(tracer_provider.clone()),
        meter: Some(meter_provider.clone()),
    };

    let otel_layer = tracing_opentelemetry::OpenTelemetryLayer::new(
        tracer_provider.tracer(env!("CARGO_PKG_NAME")),
    );
    // `match` rather than `map_err`: the error arm has to take ownership of the guard to
    // flush the providers it just started, and a closure would move it on every path.
    if registry.with(fmt).with(otel_layer).try_init().is_err() {
        // Flush and stop the processors started above rather than leaking threads
        // into a process that will keep running without OTel.
        drop(guard);
        return Err(Error::AlreadyInitialised);
    }

    // The globals are what the server's RPC layer and the propagating client transports
    // consult. Set only after the subscriber is installed, so a failed init leaves no
    // half-wired global state behind.
    global::set_tracer_provider(tracer_provider);
    global::set_meter_provider(meter_provider);

    Ok(guard)
}

/// Route panics through `tracing` so they land wherever the rest of the diagnostics go.
///
/// Without this a panic in a worker thread reaches stderr and nothing else — invisible to
/// a log collector that only reads structured output. The previous hook still runs, so the
/// usual message and backtrace are preserved.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map_or_else(
            || "unknown".to_owned(),
            |location| format!("{}:{}", location.file(), location.line()),
        );
        tracing::error!(panic.location = %location, panic.payload = %payload(info), "panic");
        previous(info);
    }));
}

/// The panic message, however it was produced.
///
/// `panic!("literal")` stores a `&str` while `panic!("{x}")` stores a `String`, so both
/// have to be tried before giving up.
fn payload(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    payload.downcast_ref::<&str>().map_or_else(
        || {
            payload
                .downcast_ref::<String>()
                .map_or_else(|| "<non-string panic payload>".to_owned(), Clone::clone)
        },
        |s| (*s).to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::{TraceContextExt as _, Tracer as _};
    use opentelemetry_sdk::trace::InMemorySpanExporterBuilder;

    use super::*;

    const TEST_SERVICE: ServiceInfo = ServiceInfo {
        name: "telemetry-test",
        version: "0.0.0-test",
    };

    #[test]
    fn explicit_filter_wins() {
        let config = Config {
            filter: "warn,api=trace".to_owned(),
            ..Config::default()
        };
        assert_eq!(config.effective_filter(), "warn,api=trace");
    }

    /// The exporter's HTTP client must be buildable in **every** feature combination the
    /// workspace produces.
    ///
    /// `opentelemetry-otlp`'s blocking `reqwest` client is built inside `init`, and `reqwest`'s
    /// TLS features are unified across everything one cargo invocation builds. A workspace crate that
    /// asks `reqwest` for TLS therefore gives this crate's client TLS too — and that client then
    /// either panics at construction (no crypto provider: `rustls-no-provider`) or fails to build
    /// (the platform verifier, with no CA bundle in a sandbox or a `FROM scratch` image: `rustls`).
    /// Either way the service dies at startup the moment an OTLP endpoint is set, which `devenv up`
    /// does. A test run per package never unifies, and one on a developer machine has a CA bundle;
    /// `ci-test` runs the whole workspace in a sandbox, which is why this test belongs there.
    ///
    /// Nothing listens on the port: building the exporters connects to nothing, and what is
    /// asserted is that `init` returns instead of panicking.
    #[test]
    fn an_otlp_endpoint_builds_whatever_reqwest_features_the_workspace_unifies() {
        let config = Config {
            otlp: OtlpConfig {
                endpoint: "http://127.0.0.1:4318".to_owned(),
                ..OtlpConfig::default()
            },
            ..Config::default()
        };

        let guard = init(&config, TEST_SERVICE)
            .expect("an OTLP endpoint must build an exporter, not panic or fail");
        drop(guard);
    }

    #[test]
    fn blank_filter_falls_back() {
        let config = Config {
            filter: "   ".to_owned(),
            ..Config::default()
        };
        // Either RUST_LOG from the ambient environment, or the default — never the blank.
        assert_ne!(config.effective_filter().trim(), "");
    }

    #[test]
    fn default_format_is_pretty() {
        assert_eq!(Config::default().format, Format::Pretty);
    }

    #[test]
    fn an_invalid_filter_is_reported_not_silently_ignored() {
        // Note the filter has to be invalid in a way EnvFilter actually rejects. A bare
        // phrase like "not a directive" parses fine — every word is read as a target
        // name. An unknown *level* is what fails.
        let config = Config {
            filter: "api=supertrace".to_owned(),
            ..Config::default()
        };
        assert!(matches!(
            init(&config, TEST_SERVICE),
            Err(Error::Filter { .. })
        ));
    }

    #[test]
    fn the_precedence_chain_stops_at_the_first_non_blank() {
        assert_eq!(
            first_non_empty(
                &[Some("configured".to_owned()), Some("env".to_owned())],
                || { "fallback".to_owned() }
            ),
            "configured"
        );
        // A blank config value is not a choice — the env var gets its turn.
        assert_eq!(
            first_non_empty(&[None, Some("env".to_owned())], || {
                "fallback".to_owned()
            }),
            "env"
        );
        assert_eq!(
            first_non_empty(&[None, None], || { "fallback".to_owned() }),
            "fallback"
        );
    }

    #[test]
    fn the_signal_path_is_appended_to_the_base_endpoint() {
        // The exporter uses a programmatic endpoint verbatim, so `init` appends the path;
        // a base with a trailing slash must not produce a double slash.
        assert_eq!(
            signal_endpoint("http://h:4318", "/v1/traces"),
            "http://h:4318/v1/traces"
        );
        assert_eq!(
            signal_endpoint("http://h:4318/", "/v1/metrics"),
            "http://h:4318/v1/metrics"
        );
    }

    #[test]
    fn sampler_specs_parse() {
        assert!(matches!(
            parse_sampler("always_on", None),
            Ok(Sampler::AlwaysOn)
        ));
        assert!(matches!(
            parse_sampler("always_off", None),
            Ok(Sampler::AlwaysOff)
        ));
        assert!(matches!(
            parse_sampler("trace_id_ratio:0.25", None),
            Ok(Sampler::TraceIdRatioBased(p)) if (p - 0.25).abs() < f64::EPSILON
        ));
        // The standard env-var spelling, with the ratio in the separate argument.
        assert!(matches!(
            parse_sampler("parentbased_traceidratio", Some("0.5")),
            Ok(Sampler::ParentBased(_))
        ));
    }

    #[test]
    fn a_sampler_spec_nobody_can_honour_is_an_error() {
        // A ratio with no argument, and an unknown name. Silently falling back to
        // always_on would export 100% where the operator asked for a fraction.
        assert!(matches!(
            parse_sampler("trace_id_ratio", None),
            Err(Error::Sampler { .. })
        ));
        assert!(matches!(
            parse_sampler("jaeger_remote", None),
            Err(Error::Sampler { .. })
        ));
        assert!(matches!(
            parse_sampler("trace_id_ratio:17", None),
            Err(Error::Sampler { .. })
        ));
    }

    /// The pipeline test: a span created through a provider this test owns lands in the
    /// exporter with the shape it was given. Bypasses `init` — the global subscriber and
    /// the global providers are process-wide and once-only, and a unit test that claims
    /// either decides the answer for every other test in the binary.
    #[test]
    fn an_exported_span_keeps_its_name_and_attributes() {
        let exporter = InMemorySpanExporterBuilder::new().build();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("test");

        tracer.in_span("unit.test", |cx| {
            cx.span().set_attribute(KeyValue::new("test.key", "value"));
        });

        // Read before shutdown: the in-memory exporter resets its store when the
        // processor shuts down, and the flag that would keep the records is
        // crate-private in the SDK.
        let spans = exporter.get_finished_spans().expect("spans are readable");
        assert_eq!(spans.len(), 1, "exactly one span was created");
        assert_eq!(spans[0].name, "unit.test");
        assert!(
            spans[0]
                .attributes
                .iter()
                .any(|kv| kv.key.as_str() == "test.key" && kv.value.as_str() == "value"),
            "the attribute survived the pipeline: {:?}",
            spans[0].attributes
        );

        provider.shutdown().expect("test provider shuts down");
        assert!(exporter.is_shutdown_called());
    }

    /// The reason the guard exists: buffered spans are flushed when it drops, not lost.
    /// A tracer-only guard would pass this test and still silently drop buffered
    /// *metrics* — which is why `init` fills both halves.
    ///
    /// Uses the SDK's channel-based test exporter rather than the in-memory one: the
    /// flush this test proves happens *during* shutdown, and the in-memory exporter
    /// resets its records at the end of that same shutdown — it would erase exactly the
    /// evidence under test.
    #[test]
    fn dropping_the_guard_flushes_buffered_spans() {
        let (test_exporter, mut exported, mut shutdowns) =
            opentelemetry_sdk::testing::trace::new_test_exporter();
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(test_exporter)
            .build();
        let tracer = provider.tracer("test");
        let guard = TelemetryGuard {
            tracer: Some(provider),
            meter: None,
        };

        tracer.in_span("guard.flush", |_cx| {});
        assert!(
            exported.try_recv().is_err(),
            "a batch exporter holds spans until it is flushed"
        );

        drop(guard);
        let span = exported
            .try_recv()
            .expect("the guard's Drop flushed the buffered span");
        assert_eq!(span.name, "guard.flush");
        assert!(shutdowns.try_recv().is_ok(), "the exporter was shut down");
    }
}
