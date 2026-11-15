# Local observability: an OpenTelemetry Collector every service exports to, with an opt-in
# Jaeger UI behind it.
#
# One concern per file (see devenv.nix). This one owns the collector process, the OTLP
# endpoint services target, and the opt-in Jaeger the collector can forward to.
#
# The endpoint is process-scoped — nix/services.nix sets it per service process — never a
# shell-level `env.*` and never committed in a service config file. It therefore exists
# exactly when `devenv up` runs a process that opted in, and nowhere else. That scoping is
# load-bearing, not tidiness: the CI workflows run every host step as `devenv shell -- …`, so a
# shell-level variable would leak the endpoint into CI and every test would initialise an
# exporter pointed at a collector that is not there. With the variable on the process, the
# "tests run with no OTel SDK" guarantee is structural rather than lucky.
#
# The default export target is the collector's own `debug` exporter, so `devenv up` proves
# the whole path — spans print in the collector's log — with no backend, no container and
# no image pull. What it does not show is a waterfall. That is the opt-in Jaeger's job
# (`observability.jaeger.enable`), and the trade is deliberate: Jaeger runs under podman,
# whose 7300 MiB closure would otherwise sit permanently in the CI toolchain layer (against
# the core collector's 189 MiB), pull a container image on first start, and need a VM on
# macOS. Default-off keeps all of that out of CI; one toggle buys the UI back.
{
  pkgs,
  lib,
  config,
  ...
}: let
  versions = import ./versions.nix;
  cfg = config.observability;
  # recursiveUpdate replaces lists, so the traces pipeline's exporter list is built here in
  # one place rather than appended to from the collector and Jaeger halves separately.
  # `otlp_grpc/jaeger`, not the bare `otlp/jaeger` alias: the collector deprecates `otlp`
  # for the gRPC exporter and warns on every start, and a devenv-nixpkgs bump that drops
  # the alias would turn that warning into a failure. The current name is verified against
  # the pinned otelcol 0.155.0.
  traceExporters = ["debug"] ++ lib.optional cfg.jaeger.enable "otlp_grpc/jaeger";
in {
  options.observability = {
    # Referenced by each service process (nix/services.nix) rather than duplicated in
    # their env literals, so :4318 lives in this one file, beside the receiver binding it
    # must agree with.
    endpoint = lib.mkOption {
      type = lib.types.str;
      default = "http://127.0.0.1:4318";
      description = "OTLP/HTTP endpoint services export to.";
      readOnly = true;
    };
    jaeger.enable = lib.mkOption {
      type = lib.types.bool;
      # Opt-in: a podman process, a collector exporter, and the UI on :16686. Off by
      # default so the podman closure stays out of CI and `devenv up` pulls no image.
      default = false;
      description = "Run Jaeger under podman behind the collector.";
    };
  };

  # `lib.mkMerge`, never `attrs // lib.mkIf …`: `mkIf` does not return the attrset, it
  # returns `{_type = "if"; condition; content;}`, and updating a normal attrset with that
  # yields a malformed module — the `_type` key wins and the collector half silently
  # disappears, with an error that names neither half.
  config = lib.mkMerge [
    {
      services.opentelemetry-collector = {
        enable = true;

        # Core distribution, not the module's contrib default: `otelcol components` confirms
        # core carries everything used here (otlpreceiver, debugexporter, otlpexporter,
        # healthcheckextension, batchprocessor) at a measured 189 MiB closure against
        # contrib's 381 MiB. That lands in the devenv layer every host step realises, which
        # changes roughly never, so the closure is a permanent cost — half of contrib's is
        # worth the one-line override.
        package = pkgs.opentelemetry-collector;

        settings = {
          # The receiver binding the endpoint option above must agree with. OTLP/HTTP on
          # 4318; what every service targets, and the seam a homelab or production backend
          # sits behind (override this exporter list in devenv.local.nix, not in every
          # service).
          receivers.otlp.protocols.http.endpoint = "localhost:4318";
          exporters = {
            debug.verbosity = "detailed";
          } // lib.optionalAttrs cfg.jaeger.enable {
            # The opt-in forward to Jaeger's OTLP/gRPC port. Jaeger v2 all-in-one listens on
            # 4317 out of the box; the container maps it below.
            "otlp_grpc/jaeger" = {
              endpoint = "localhost:4317";
              tls.insecure = true;
            };
          };
          service.pipelines = {
            traces = {
              receivers = ["otlp"];
              exporters = traceExporters;
            };
            metrics = {
              receivers = ["otlp"];
              exporters = ["debug"];
            };
          };
          # health_check (:13133) and service.extensions come from the devenv module's
          # defaultSettings, merged in by recursiveUpdate. Do NOT re-declare
          # service.extensions here: recursiveUpdate replaces lists, so dropping
          # health_check from it would silently kill the readiness probe, and
          # `processes.api` could then start before the receiver binds — losing the first
          # spans of a `devenv up` with no error anywhere.
        };
      };
    }

    (lib.mkIf cfg.jaeger.enable {
      # The module supplies its own podman rather than trusting host state, matching how
      # every other nix module here packages its tools. Kept inside the mkIf so the 7300
      # MiB closure is realised only when the toggle is on — never in CI, where it is off.
      packages = [pkgs.podman];

      processes.jaeger = {
        exec = ''
          # Self-heal: a `podman run` that was SIGKILLed (laptop sleep, a hard `devenv
          # down`) leaves a stopped container whose name reservation blocks the next start.
          podman rm --force --ignore jaeger
          # exec so the container process replaces the shell: SIGTERM from `devenv down`
          # reaches podman, which forwards it to the container, and --rm deletes it. First
          # start pulls ~100 MB from docker.io — the same class of network need as
          # `just proto::generate`'s remote plugins; later starts reuse the store.
          exec podman run --rm --name jaeger \
            -p 4317:4317 -p 16686:16686 \
            docker.io/jaegertracing/jaeger:${versions.otel.jaeger}
        '';
        # 4317 (OTLP/gRPC, what the collector's otlp/jaeger exporter speaks) and 16686
        # (the UI) — NOT 4318, which belongs to the collector's receiver; mapping it twice
        # would fail at bind.
        ready.http.get = {
          host = "localhost";
          scheme = "http";
          path = "/";
          port = 16686;
        };
        ready.initial_delay = 2;
        ready.probe_timeout = 5;
      };
    })
  ];
}
