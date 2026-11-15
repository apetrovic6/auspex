# The NixOS module every service gets, generated from its own manifest and configuration.
#
# One module, called once per discovered service. Everything that differs between services is
# an argument here — the name, the binary, the committed configuration, the database facts and
# the package — so there is no service-specific code in the template and a service scaffolded
# with `cli add service` gets a module with no edit to this file or to any flake.
#
# The module owns the *deployment* shape and nothing else: it decides where the binary comes
# from, which user it runs as, what it may touch, which secrets it can read and how it reaches
# its database. What the service *is* stays in the service's own configuration file and in its
# code — this module never names a route, a table or a config key the service did not already
# document.
#
# Everything it creates on a host is namespaced by the PRODUCT, not by the service: the options
# are `services.<product>.<service>`, the unit is `<product>-<service>.service`, the
# configuration is `/etc/<product>/<service>.toml`, and the default account is
# `<product>-<service>`. Every scaffolded service is called `api` (that is what
# `just add service api` makes), so two products whose modules are imported into one system
# would otherwise define `services.api`, `api.service` and `/etc/api` twice — a conflict at
# evaluation where the definitions differ, and one product silently running the other's unit
# where they agree. The product name is `[publish].project` in `versions.toml`: the value
# `cli new` fills in, the one the images are pushed under, and the only name a host knows the
# product by.
{ lib }:
{
  # The product this module belongs to (`[publish].project`). Every host-level name below is
  # derived from it, so it is an argument rather than a constant.
  product,
  # The service's identity, from `nix/pipeline/discovery.nix`.
  name,
  # The crate, and therefore the binary: `<package>/bin/<pname>`.
  pname,
  # The committed `[application] port` — null only for a service whose file has no port.
  port,
  # Whether the service ships a web bundle (from a `web-shell` trunk build or the `spa`
  # fallback).
  hasWeb,
  # Whether the service owns a schema at all, from its own configuration (`[database]`
  # present). This is the default for `database.enable`, and it is a *fact about the service*,
  # not the development location: a deployment supplies where the database is.
  hasDatabase,
  # The one build everything derives from.
  package,
  # The committed configuration: parsed (to write a file a deployment can override) and
  # verbatim (to write it unchanged, comments and all, when nothing was overridden).
  settings,
  settingsText,
  host,
}:
{ config, lib, pkgs, ... }:
let
  inherit (lib)
    mkOption mkIf types optional optionalAttrs recursiveUpdate;

  cfg = config.services.${product}.${name};

  # The three spellings an operator types on the host: the unit, the account, the directory.
  # Derived once, here, because every collision between two products was one of these being
  # derived from the service crate alone.
  unit = "${product}-${name}";
  configDir = "/etc/${product}";

  # `pkgs.formats.toml` on the module's own pkgs, so `generate` is the serialiser the module
  # writes with: it escapes strings (newlines, control characters, quotes) because it is
  # nixpkgs' own format, and it is the same one a module author would reach for.
  toml = pkgs.formats.toml { };

  # --- the configuration file -------------------------------------------------
  #
  # `settings` is an OVERLAY on the committed file, not a replacement for it. A module option
  # with a default is *replaced* by any definition, so `settings.database.url = …` spelled as
  # the whole file would drop `application`, `telemetry` and everything else the service needs
  # — a deployment that changed one line would break the service in a way nothing reports.
  # Overlay semantics are the only ones a `settings` option can have and stay usable.
  configured = recursiveUpdate settings cfg.settings;

  # A URL and the structured fields are mutually exclusive (the service refuses both, at load).
  # Since the base file carries NO location (`service-db-url`), nothing here has to be taken out
  # for a URL deployment — this only matters when a deployment's own `settings` names a
  # location AND a `database.url`/`urlFile`. The URL wins: it is the deployment's explicit
  # statement about where the database is, and the file is reduced to the pool size so the
  # process does not refuse to start over two shapes in one configuration.
  urlMode = cfg.database.enable && (cfg.database.url != null || cfg.database.urlFile != null);
  effective =
    if urlMode then
      configured
      // {
        database = {
          max_connections = configured.database.max_connections or 10;
        };
      }
    else
      configured;

  # The file: the committed bytes when nothing is overridden (comments and all), and a
  # generated one otherwise. `toml.generate` is what escapes a multi-line string correctly —
  # the hand-written serialiser this replaced did not, so a `settings` value containing a
  # newline wrote a file the service could not parse.
  configFile =
    if cfg.settings == { } && !urlMode then
      { text = settingsText; }
    else
      { source = toml.generate "${name}.toml" effective; };

  # --- secrets ----------------------------------------------------------------
  #
  # A credential's key is the environment variable it feeds, without the `_FILE` suffix:
  # `credentials.APP__DATABASE__PASSWORD = "/run/secrets/db-password"` loads the file as a
  # systemd credential and sets `APP__DATABASE__PASSWORD_FILE` to the path systemd put it at.
  # The suffix is the whole mechanism (`crates/settings` resolves it), so the mapping is
  # mechanical and there is no second naming convention to learn.
  credentials =
    (optionalAttrs (cfg.database.enable && cfg.database.urlFile != null) {
      APP__DATABASE__URL = cfg.database.urlFile;
    })
    // (optionalAttrs (cfg.database.enable && cfg.database.passwordFile != null) {
      APP__DATABASE__PASSWORD = cfg.database.passwordFile;
    })
    // cfg.credentials;
  credentialsDirectory = "/run/credentials/${unit}.service";
  # `LoadCredential=<id>:<source>`. A source is a path or a secret's `path` attribute, which is
  # a string by the time it gets here; either is accepted.
  loadCredentials = lib.mapAttrsToList (id: source: "${id}:${toString source}") credentials;
  # What the process actually sees: a path into the unit's credential directory, never the
  # value. A `urlFile`/`passwordFile` option is this mechanism spelled with a shorter name.
  credentialEnvironment = lib.mapAttrs' (id: _:
    lib.nameValuePair "${id}_FILE" "${credentialsDirectory}/${id}"
  ) credentials;

  # --- systemd ----------------------------------------------------------------
  #
  # The hardening defaults. Every one of these is a setting a service does not choose for
  # itself; `hardening` exists because a service that legitimately needs one back (a
  # Unix-socket peer-auth connection needs AF_UNIX; a debug build may want `MemoryDenyWriteExecute
  # = false`) should change one line of its deployment, not fork this module.
  hardeningDefaults = {
    # Nothing is written outside the state directory, /tmp and the runtime directory.
    ProtectSystem = "strict";
    ProtectHome = true;
    PrivateTmp = true;
    PrivateDevices = true;
    ProtectClock = true;
    ProtectHostname = true;
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectControlGroups = true;
    ProtectProc = "invisible";
    ProcSubset = "pid";
    # No privilege to drop: the process is not allowed to have any, and cannot gain one.
    NoNewPrivileges = true;
    CapabilityBoundingSet = "";
    AmbientCapabilities = [ ];
    # The service's own IPC surface is a TCP port and, for a peer-auth database, a Unix
    # socket. AF_UNIX is not optional here: leaving it out is the difference between a
    # working socket connection and a service that reports "not ready" for a reason nothing
    # names.
    RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
    RestrictNamespaces = true;
    RestrictRealtime = true;
    RestrictSUIDSGID = true;
    LockPersonality = true;
    MemoryDenyWriteExecute = true;
    SystemCallFilter = [ "@system-service" "~@privileged" ];
    UMask = "0077";
  };

  serviceEnvironment =
    {
      APP_CONFIG_DIR = configDir;
      APP__APPLICATION__HOST = cfg.host;
      APP__APPLICATION__PORT = toString cfg.port;
    }
    // optionalAttrs (cfg.otlp.endpoint != null) {
      APP__TELEMETRY__OTLP__ENDPOINT = cfg.otlp.endpoint;
    }
    // optionalAttrs (cfg.otlp.serviceName != null) {
      APP__TELEMETRY__OTLP__SERVICE_NAME = cfg.otlp.serviceName;
    }
    // optionalAttrs (cfg.web.enable && cfg.web.dir != null) {
      APP__WEB__DIR = toString cfg.web.dir;
    }
    # The URL shape, when the deployment names one. `urlFile`/`passwordFile` are credentials and
    # arrive through `credentialEnvironment`; a plain `url` is configuration here — which is why
    # the option's documentation says a URL that carries a password belongs in a file.
    // optionalAttrs (cfg.database.enable && cfg.database.url != null) {
      APP__DATABASE__URL = cfg.database.url;
    }
    // credentialEnvironment
    // cfg.environment;

  # What `ExecStartPre = … migrate` needs to exist before it runs.
  #
  # `postgresql-setup.service` is where nixpkgs creates `ensureDatabases` and `ensureUsers` — not
  # in `postgresql.service`, which the setup unit only requires and runs *after*. Ordering
  # against the server alone lets this unit start while the role it migrates as does not exist
  # yet: `migrate` fails, `Restart = on-failure` with `RestartSec = 2` spends the default start
  # limit (five starts in ten seconds) in ten seconds, and the unit ends in `start-limit-hit`.
  # The window is short, which is how a test passes on timing alone — so the module VM holds the
  # setup back on purpose and asserts the service still comes up.
  #
  # Named only when nixpkgs provides it: a deployment whose database is elsewhere (a managed
  # cluster, a socket another module owns) has no such unit, and a `Wants=`/`After=` for a unit
  # that does not exist would be a dangling dependency pretending to be an ordering guarantee.
  postgresUnits =
    optional cfg.database.enable "postgresql.service"
    ++ optional (cfg.database.enable && config.systemd.services ? postgresql-setup)
      "postgresql-setup.service";

in
{
  options.services.${product}.${name} = {
    enable = mkOption {
      type = types.bool;
      default = false;
      description = "Run ${product}'s ${name} as a systemd service (`${unit}.service`).";
    };

    package = mkOption {
      type = types.package;
      default = package;
      description = ''
        The service's package. The default is the one `nix/pipeline` builds — the same
        derivation the image is made from — so a deployment does not choose a second build
        and cannot accidentally run different bytes from the container.
      '';
    };

    settings = mkOption {
      # A freeform submodule over the TOML type, not `types.attrs`: two modules that each set one
      # key under `database` then MERGE, where an attrset option would let the later definition
      # replace `database` wholesale and silently drop the other key. The type also gives the
      # values a shape (strings, integers, booleans, arrays, nested tables), so a wrong one is
      # refused at evaluation instead of written and discovered by a service that will not start.
      type = types.submodule { freeformType = toml.type; };
      default = { };
      example = lib.literalExpression ''
        {
          application.port = 8080;
          database.url = "postgres:///counter?host=/run/postgresql";
        }
      '';
      description = ''
        Overrides for the committed `services/${name}/config/${name}.toml`, deep-merged over
        it. Empty (the default) writes the committed file byte for byte, comments included;
        anything else is written as a fresh file, so only the keys named here (plus everything
        the committed file already had) are in it. The result is written to
        `${configDir}/${name}.toml`, the file the service reads through `APP_CONFIG_DIR`.
      '';
    };

    host = mkOption {
      type = types.str;
      default = host;
      description = "Address the service binds. Set as `APP__APPLICATION__HOST`.";
    };

    port = mkOption {
      type = types.port;
      default = if port == null then 0 else port;
      description = "Port the service binds, and opens when `openFirewall` is set.";
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = "Open `port` in the host firewall.";
    };

    environment = mkOption {
      type = types.attrsOf types.str;
      default = { };
      example = { APP__REFLECTION__ENABLED = "false"; };
      description = ''
        Extra environment variables, merged over the module's own. A service that verifies
        credentials (`cargo xtask add-auth-oidc`) refuses to start without its provider:
        set `APP__AUTH__ISSUER` here, or as a `credentials` entry to keep it in a file.
      '';
    };

    credentials = mkOption {
      type = types.attrsOf (types.either types.path types.str);
      default = { };
      example = { APP__DATABASE__PASSWORD = "/run/secrets/db-password"; };
      description = ''
        Files loaded as systemd credentials. The attribute name is the variable the file
        feeds, without `_FILE`: the key `APP__DATABASE__PASSWORD` loads the file and sets
        `APP__DATABASE__PASSWORD_FILE` to the credential's path, which `crates/settings`
        resolves to the file's contents. The value is never in the environment or the store.
      '';
    };

    database = {
      enable = mkOption {
        type = types.bool;
        default = hasDatabase;
        description = ''
          Whether this service talks to a database. Discovered from its configuration by
          default: a service whose `config/<name>.toml` has a `[database]` section has one.
        '';
      };

      url = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "postgres:///counter?host=/run/postgresql";
        description = ''
          A complete connection URL, for a database whose parts the deployment does not own
          (a managed cluster's `<cluster>-app` secret, a peer-auth socket). It replaces the
          committed `[database]` section's structured fields, and the service refuses a
          configuration that sets both.

          A URL that carries a password belongs in `urlFile`: this option's value ends up in
          the Nix store.
        '';
      };

      urlFile = mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = null;
        description = ''
          A file holding the connection URL — a sops secret, a systemd credential, a mounted
          Kubernetes secret. Loaded as a credential and exposed as `APP__DATABASE__URL_FILE`.
        '';
      };

      passwordFile = mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = null;
        description = ''
          A file holding the password for the structured connection fields. Loaded as a
          credential and exposed as `APP__DATABASE__PASSWORD_FILE`.
        '';
      };

      migrate = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Run `<service> migrate` as `ExecStartPre`, before the server binds. The service
          takes a Postgres advisory lock, so a rollout with several replicas has one
          migrating and the rest waiting rather than racing.
        '';
      };
    };

    otlp = {
      endpoint = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "OTLP/HTTP endpoint, as `APP__TELEMETRY__OTLP__ENDPOINT`.";
      };
      serviceName = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Service name for telemetry, as `APP__TELEMETRY__OTLP__SERVICE_NAME`.";
      };
    };

    web = {
      enable = mkOption {
        type = types.bool;
        default = hasWeb;
        description = ''
          Serve the built single-page application. Defaults to on when the package ships one
          (`[package.metadata.app] web-shell` or `spa` fallback), and off for a service with
          no front end.
        '';
      };
      dir = mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = if hasWeb then "${cfg.package}/share/${name}/web" else null;
        defaultText = lib.literalExpression ''"''${config.services.${product}.${name}.package}/share/${name}/web"'';
        description = ''
          The directory holding `index.html` and its assets, as `APP__WEB__DIR`. The default
          is the bundle the package installed; a deployment serving its own build points this
          at that path.
        '';
      };
    };

    dynamicUser = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Run under systemd's `DynamicUser`. Off means a fixed `user`/`group`, which is what a
        peer-auth database connection needs: Postgres compares the OS user name with the role
        name, and a dynamic uid's name changes with the allocation.
      '';
    };

    user = mkOption {
      type = types.str;
      default = unit;
      description = ''
        Account to create and run as when `dynamicUser` is false. Defaults to `${unit}`: two
        products' modules on one host must not both want `api`, and peer auth compares this
        name with the Postgres role's, so a deployment that keeps the default creates the
        role under the same name.
      '';
    };

    group = mkOption {
      type = types.str;
      default = unit;
      description = "Group to create and run as when `dynamicUser` is false. Defaults to `${unit}`.";
    };

    hardening = mkOption {
      type = types.attrs;
      default = { };
      example = { MemoryDenyWriteExecute = false; };
      description = ''
        systemd settings merged over the module's hardening defaults. One key relaxes or
        tightens one setting; the defaults stay otherwise, which is the point of the option
        existing rather than a second `serviceConfig`.
      '';
    };

    extraServiceConfig = mkOption {
      type = types.attrs;
      default = { };
      description = "systemd settings merged last, for anything the options above do not cover.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = !(cfg.web.enable && cfg.web.dir == null);
        message = "services.${product}.${name}.web.enable is set but web.dir is null.";
      }
      {
        assertion = !(cfg.database.enable && cfg.database.url != null && cfg.database.urlFile != null);
        message = "services.${product}.${name}.database: set one of `url` and `urlFile`, not both.";
      }
    ];

    users.users = mkIf (!cfg.dynamicUser) {
      ${cfg.user} = {
        isSystemUser = true;
        group = cfg.group;
        home = "/var/lib/${unit}";
        description = "${unit} service";
      };
    };
    users.groups = mkIf (!cfg.dynamicUser) { ${cfg.group} = { }; };

    # The product's own directory, the service's own file: two products both shipping an `api`
    # write `/etc/<product>/api.toml`, and `APP_CONFIG_DIR` is the directory above, which is
    # all the loader joins the service's name onto.
    environment.etc."${product}/${name}.toml" = configFile;

    networking.firewall.allowedTCPPorts = optional cfg.openFirewall cfg.port;

    systemd.services.${unit} = {
      description = "${unit} — a service from the rust-app-template pipeline";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ] ++ postgresUnits;
      after = [ "network-online.target" ] ++ postgresUnits;
      environment = serviceEnvironment;

      serviceConfig = hardeningDefaults
        // cfg.hardening
        // {
          Type = "simple";
          ExecStart = "${cfg.package}/bin/${pname} serve";
          # `migrate` is a separate process, run before the server binds: a migration that
          # fails fails the unit rather than leaving a serving process against a schema it
          # does not understand.
          ExecStartPre = optional (cfg.database.enable && cfg.database.migrate)
            "${cfg.package}/bin/${pname} migrate";
          Restart = "on-failure";
          RestartSec = 2;
          DynamicUser = cfg.dynamicUser;
          LoadCredential = loadCredentials;
        }
        // optionalAttrs (!cfg.dynamicUser) {
          User = cfg.user;
          Group = cfg.group;
        }
        // cfg.extraServiceConfig;
    };
  };
}
