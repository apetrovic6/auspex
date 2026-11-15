# The backend(s): their databases, their schema tooling, and the processes `devenv up` runs.
#
# Both the postgres databases and the per-service processes are DISCOVERED from the same
# discovery the pipeline uses (amendment F): a service's `[database]` section in
# `services/<name>/config/<name>.local.toml` names the development database, role and port
# (the base `<name>.toml` carries the pool size only, so a deployment can supply a URL), and
# each service under `services/` becomes a `devenv up` process.
{
  pkgs,
  lib,
  config,
  ...
}: let
  discovery = import ./pipeline/discovery.nix { inherit lib; } ../.;

  # Services that own a schema (their config declares a [database] section).
  dbServices = lib.filter (s: s.database != null) discovery.services;

  pgPort =
    if dbServices == [ ] then 5432 else (lib.head dbServices).database.port;

  initialDatabases = map (s: { name = s.database.name; }) dbServices;

  # A role per database-owning service. The password matches the username, as before.
  initialScript = lib.concatMapStrings (s: ''
    CREATE ROLE ${s.database.username} WITH LOGIN PASSWORD '${s.database.username}' SUPERUSER;
  '') dbServices;

  # One process per service, running from the service's directory with the same
  # APP_CONFIG_DIR its own Justfile exports.
  serviceProcess = s: {
    exec = ''
      cd "$DEVENV_ROOT/${s.dir}" && APP_CONFIG_DIR=config cargo run -p ${s.name} -- serve
    '';
    # The observability opt-in, beside the process it instruments. Gated on the same enable
    # that gates the collector; see nix/observability.nix for the scoping argument.
    env = lib.optionalAttrs config.services.opentelemetry-collector.enable {
      APP__TELEMETRY__OTLP__ENDPOINT = config.observability.endpoint;
    };
  };
in {
  # https://devenv.sh/services/
  services.postgres = {
    enable = dbServices != [ ];
    package = pkgs.postgresql_17;
    # TCP rather than the unix socket devenv defaults to: it is what the deployed service
    # will use, so local and production differ in the host, not in the transport.
    listen_addresses = "127.0.0.1";
    port = pgPort;
    inherit initialDatabases initialScript;
  };

  processes = lib.listToAttrs (map (s: {
    name = s.name;
    value = serviceProcess s;
  }) discovery.services);
}
