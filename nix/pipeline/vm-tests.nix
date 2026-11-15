# The checks that prove a service's delivery artefacts RUN, not merely build.
#
# Two per service, because a service ships two ways and they fail differently: the NixOS module
# (a systemd unit, a fixed or dynamic account, a database reached over a socket or a URL file)
# and the container image (an entrypoint, a baked configuration, a uid that exists, a
# filesystem with no shell). A build that produced a binary nothing can start passes every
# other gate in this repository; these two are the ones that notice.
#
# Everything here is derived from the service's own facts — name, binary, port, package, image
# — so a scaffolded service gets both checks with no edit. The only service-shaped knowledge in
# this file is the *mechanism*: a database on the VM and the two ways to reach it.
{ lib, pkgs }:
{
  name,
  # The crate and therefore the binary: `<package>/bin/<pname>`.
  pname,
  # The committed `[application] port`, from the same file the module reads.
  port,
  # Whether the service verifies credentials (`[auth]` in its base configuration). It then
  # refuses to start without an issuer, so both tests have to give it one.
  hasAuth ? false,
  # Whether the service enables the OIDC BFF; it additionally requires these web settings.
  hasWebAuth ? false,
  # Whether the image bakes a real web shell (a Trunk build) rather than the fallback demo
  # bundle. A product with no web shell — `--drop-example`, or a terminal-only Crux app — serves
  # the fallback, which has no hashed `.wasm` to find; asserting one there fails a product that
  # did nothing wrong.
  servesWebShell ? true,
  package,
  image,
  # The service's NixOS module, imported by the two nodes that run it under systemd.
  module,
  # The product this service belongs to (`[publish].project`). Everything the module names on a
  # host carries it: the unit, the account and the configuration directory. The tests address
  # the unit by that name rather than by the crate's, so a module that stopped prefixing would
  # fail here rather than pass under a name the deployment never sees.
  product,
}:
let
  inherit (lib) optionalAttrs;
  # The database server the VMs run. Named once: both tests must put the service in front of
  # the same Postgres the dev shell and the image use, or "the migration works" means two
  # different things.
  postgres = pkgs.postgresql_17;
  # The service's own binary, for the assertions that need it (none today — kept out of the
  # test scripts so a broken entrypoint fails as a failed unit rather than as a command the
  # test ran itself).
  servicePort = toString port;
  # The names the module creates: `<product>-<service>` for the unit and the account,
  # `/etc/<product>` for the configuration directory, holding `<service>.toml`.
  unit = "${product}-${name}";
  etcDir = "/etc/${product}";

  # The issuer a service with `[auth]` is given. It refuses to start without one (fail closed,
  # ADR 0027) and the committed file cannot carry it, because it is the deployment's. Nothing
  # listens on port 1, and that is the point: the verifier fetches its keys lazily, on the first
  # token, and these tests never present one, so the service starts and answers its probes
  # without ever reaching an identity provider. Loopback `http` is the one plain-text issuer the
  # verifier accepts. The rejected alternative was a stub provider on the VM: a second server to
  # keep honest, to prove nothing these tests are about.
  authIssuer = "http://127.0.0.1:1";
  authPublicUrl = "http://127.0.0.1:${servicePort}";
  # Public, test-only base64 of 32 bytes, shared with the generated tests. NEVER a deployment
  # value: deployments use APP__AUTH__COOKIE_KEY_FILE or the module's credentials.
  authCookieKey = "MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=";
  authEnvironment = optionalAttrs hasAuth { APP__AUTH__ISSUER = authIssuer; }
    // optionalAttrs hasWebAuth {
      APP__AUTH__PUBLIC_URL = authPublicUrl;
      APP__AUTH__COOKIE_KEY = authCookieKey;
    };
  # The hand-run migration loads the same settings as the units, outside their environment.
  authCommandEnvironment = lib.concatStrings (lib.mapAttrsToList
    (key: value: " ${key}=${lib.escapeShellArg value}") authEnvironment);

  # `postgresql-setup.service` is where nixpkgs runs `initialScript`, `ensureDatabases` and
  # `ensureUsers` — the role and the database a service migrates into. Holding it back on
  # purpose is what makes the ordering the test depends on rather than a race it happened to
  # win: `Restart = on-failure` with `RestartSec = 2` gives up after five starts in ten
  # seconds, so a module ordered after `postgresql.service` alone fails this boot instead of
  # passing on a fast machine. Twelve seconds is longer than that budget with room for the
  # five attempts to fail fast.
  #
  # The two module nodes only. The image node runs the container under the deployment's own
  # unit with podman's restart policy, which is not the module's start limit — a different
  # question, and not this one.
  heldSetup = {
    systemd.services.postgresql-setup.serviceConfig.ExecStartPre = "${pkgs.coreutils}/bin/sleep 12";
  };

  # The cluster every test node runs. `ensureUsers` creates the role with LOGIN and
  # `ensureDBOwnership` makes it the database's owner, which is what a service that creates
  # its own tables needs. The role's name is the service's, because that is what the committed
  # configuration and the dynamic node's URL use; the PEER node overrides it with the account
  # the module creates by default, which is where peer auth's name equality lives.
  postgresNode = {
    services.postgresql = {
      enable = true;
      package = postgres;
      ensureDatabases = [ name ];
      ensureUsers = [
        {
          name = name;
          ensureDBOwnership = true;
        }
      ];
    };
  };
in
{
  # --- the module ------------------------------------------------------------
  #
  # One test, two machines, because the two database modes are two *deployments* of the same
  # module and running them side by side is what proves the module did not quietly assume one:
  #
  #   dynamic — `DynamicUser` and a URL from a file. What a managed database hands a workload:
  #             the connection string is a secret the process reads, not configuration.
  #   peer    — a fixed account and Postgres peer auth over the Unix socket. What a NixOS host
  #             whose roles it manages itself does: no password anywhere, and the OS user name
  #             is the credential. Its schema is PRE-CREATED (see the seed unit), which makes it
  #             the adoption case: the first migration must record itself as applied and leave
  #             the tables alone rather than run its DDL.
  #
  # The three adoption states are all proved here, against Postgres, because no fixture can:
  # empty (dynamic: the DDL runs), every table present (peer: adopted, columns untouched) and
  # one of two present (the refusal, run from the test script against a second database).
  #
  # Each node is 1 GiB: the service is a few tens of megabytes and Postgres a few hundred, and
  # the runners run up to four jobs at once (decision 6).
  module = pkgs.testers.runNixOSTest {
    name = "${unit}-module-vm";
    nodes.dynamic = lib.recursiveUpdate (lib.recursiveUpdate postgresNode heldSetup) {
      imports = [ module ];
      virtualisation.memorySize = 1024;
      # A role with a password, which is what the URL in the file carries. NixOS's pg_hba is
      # `scram-sha-256` for 127.0.0.1 by default, so the password is load-bearing: a URL without
      # one fails to authenticate, which is how the harness noticed the first version of this
      # test was not exercising the file at all.
      # `initialScript` is a path (NixOS runs `psql -f` on first start), so the statement is a
      # file. It runs before `ensureUsers`, which then finds the role and only assigns the
      # database's ownership to it.
      services.postgresql.initialScript = pkgs.writeText "${name}-role.sql" ''
        CREATE ROLE ${name} LOGIN PASSWORD 'test-password';
      '';
      # The URL as a file, which is the shape a sops secret or a systemd credential has. The
      # module loads it as a credential and sets APP__DATABASE__URL_FILE, so the value never
      # appears in the unit's environment or in the store.
      environment.etc."${product}/${name}-database-url".text =
        "postgres://${name}:test-password@127.0.0.1:5432/${name}";
      services.${product}.${name} = {
        enable = true;
        environment = authEnvironment;
        database = {
          enable = true;
          urlFile = "${etcDir}/${name}-database-url";
        };
      };
    };
    nodes.peer = lib.recursiveUpdate (lib.recursiveUpdate postgresNode heldSetup) {
      imports = [ module ];
      virtualisation.memorySize = 1024;
      # Peer auth compares the kernel's user with the role's name, and the kernel's user is the
      # account the module creates by default: `<product>-<service>`. The role and the database
      # have to carry that name, not the bare service name the dynamic node's URL uses —
      # `ensureDBOwnership` names one database per role, and two products on one host would
      # collide on a database name exactly as they would on a unit name. The module does not
      # create either; the deployment does, and this is what a deployment on a shared host does.
      services.postgresql = {
        ensureDatabases = [ unit ];
        ensureUsers = [
          {
            name = unit;
            ensureDBOwnership = true;
          }
        ];
      };
      # The schema this service's first migration would create, made by "something else" — a
      # restored dump, the Go service being replaced and its `gorm.AutoMigrate`. `deleted_at`
      # is the column the migration's own DDL does not have, so its survival is what proves the
      # migration adopted the tables instead of recreating them.
      #
      # A oneshot unit rather than something in the test script: the module's unit has an
      # `ExecStartPre = … migrate` and is wanted by multi-user, so it would migrate on boot
      # before the script could seed anything. A unit ordered after the seed is what makes the
      # ordering a property of the system rather than of the harness' timing.
      systemd.services.seed-legacy-schema = {
        wantedBy = [ "multi-user.target" ];
        after = [ "postgresql-setup.service" ];
        requires = [ "postgresql-setup.service" ];
        before = [ "${unit}.service" ];
        serviceConfig = {
          Type = "oneshot";
          # As the SERVICE's account, not as postgres: `has_table` reads
          # `information_schema.tables`, which shows only tables the current user has a privilege
          # on. A schema owned by someone else is invisible to the adoption decision, and the
          # migration then tries to create it and fails with "already exists" — which is exactly
          # what this test caught when the seed ran as postgres. A real legacy schema must grant
          # the service's role access for adoption to see it.
          User = unit;
          RemainAfterExit = true;
        };
        script = ''
          ${postgres}/bin/psql -d ${unit} -v ON_ERROR_STOP=1 -c '
            CREATE TABLE counters (
              id text PRIMARY KEY,
              count bigint NOT NULL DEFAULT 0,
              created_at timestamptz,
              updated_at timestamptz,
              deleted_at timestamptz
            )' -c '
            CREATE TABLE counter_events (
              id text PRIMARY KEY,
              counter_id text NOT NULL,
              count bigint NOT NULL,
              recorded_at timestamptz NOT NULL DEFAULT now(),
              migrated_by text
            )'
        '';
      };
      services.${product}.${name} = {
        enable = true;
        # A fixed account is what peer auth needs: it compares the kernel's idea of the user
        # with the role name, and a dynamic uid's synthesised name is not one. Its NAME is the
        # module's default (`<product>-<service>`), not a choice made here — `user`/`group` are
        # left out on purpose, so the account a real deployment gets by default is the one this
        # test proves, and the role below is that same name.
        dynamicUser = false;
        environment = authEnvironment;
        database = {
          enable = true;
          url = "postgres:///${unit}?host=/run/postgresql";
        };
      };
    };
    testScript = ''
      start_all()

      # --- dynamic user, URL from a credential file -------------------------
      dynamic.wait_for_unit("postgresql.service")
      dynamic.wait_for_unit("${unit}.service")

      # The unit depends on the one that creates the role and the database, not only on the
      # server: `ensureUsers`/`initialScript` run in `postgresql-setup.service`, and `heldSetup`
      # makes that unit slow so the ordering is load-bearing. Without it the `migrate` below
      # would hit a missing role and this unit would be in `start-limit-hit` by now.
      dynamic.succeed(
          "systemctl show -p After --value ${unit}.service | tr ' ' '\\n' | grep -qx postgresql-setup.service"
      )
      # No failed start attempt behind the green unit: a module that ordered itself after the
      # server alone shows up here as a restart count even on a boot where the retry won.
      dynamic.succeed("systemctl show -p NRestarts --value ${unit}.service | grep -qx 0")
      ${lib.optionalString hasAuth ''
        # The issuer reached the running unit, on both nodes. A regression that dropped it
        # would otherwise show only as a unit that fails at config load, which reads as a broken
        # service rather than a test that stopped supplying the one setting it must.
        assert "APP__AUTH__ISSUER=${authIssuer}" in dynamic.succeed(
            "systemctl show -p Environment --value ${unit}.service"
        ).split(), "${unit} (dynamic node): APP__AUTH__ISSUER is not in the unit's environment"
      ''}
      ${lib.optionalString hasWebAuth ''
        assert "APP__AUTH__PUBLIC_URL=${authPublicUrl}" in dynamic.succeed(
            "systemctl show -p Environment --value ${unit}.service"
        ).split(), "${unit} (dynamic node): APP__AUTH__PUBLIC_URL is not in the unit's environment"
        assert "APP__AUTH__COOKIE_KEY=${authCookieKey}" in dynamic.succeed(
            "systemctl show -p Environment --value ${unit}.service"
        ).split(), "${unit} (dynamic node): APP__AUTH__COOKIE_KEY is not in the unit's environment"
      ''}

      # The configuration sits under the product's directory. Two products both shipping an
      # `api` write `/etc/<product>/api.toml`, and the bare path is the collision this module
      # no longer has — asserted absent, because a half-migration would leave both.
      dynamic.succeed("test -f ${etcDir}/${name}.toml")
      dynamic.succeed("test ! -e /etc/${name}/${name}.toml")

      # Ready means the database answers, so this is also the assertion that the credential
      # file was read: without it the service cannot build a connection at all.
      dynamic.wait_until_succeeds("curl -fsS http://127.0.0.1:${servicePort}/readyz", timeout=60)
      dynamic.succeed("curl -fsS http://127.0.0.1:${servicePort}/healthz")

      # The migrations ran, as the service, before the server bound: the table exists and is
      # empty. A `migrate` that silently did nothing leaves a service that starts and then
      # fails every request, which no readiness probe sees.
      #
      # `sudo -u postgres psql` over the socket: `psql` with a TCP URL would ask for the role's
      # password, and a test driver has no stdin to give it — the first version of this hung
      # there for the 900-second timeout, which is a failure that looks like a hang.
      dynamic.succeed(
          "sudo -u postgres ${postgres}/bin/psql -d ${name} -c 'select count(*) from counters'"
      )
      # The first migration owns two tables, and on an empty database BOTH are created — the
      # set, not just the first one, which is what the adoption decision is about.
      dynamic.succeed(
          "sudo -u postgres ${postgres}/bin/psql -d ${name} -c 'select count(*) from counter_events'"
      )

      # Liveness does not depend on the database; readiness does. The whole reason there are
      # two probes is this difference, and a module that wired them to the same handler would
      # pass everything above.
      dynamic.succeed("systemctl stop postgresql.service")
      dynamic.wait_until_fails("curl -fsS http://127.0.0.1:${servicePort}/readyz", timeout=60)
      dynamic.succeed("curl -fsS http://127.0.0.1:${servicePort}/healthz")

      # --- fixed user, peer auth over the socket ----------------------------
      peer.wait_for_unit("postgresql.service")
      peer.wait_for_unit("${unit}.service")
      ${lib.optionalString hasAuth ''
        assert "APP__AUTH__ISSUER=${authIssuer}" in peer.succeed(
            "systemctl show -p Environment --value ${unit}.service"
        ).split(), "${unit} (peer node): APP__AUTH__ISSUER is not in the unit's environment"
      ''}
      ${lib.optionalString hasWebAuth ''
        assert "APP__AUTH__PUBLIC_URL=${authPublicUrl}" in peer.succeed(
            "systemctl show -p Environment --value ${unit}.service"
        ).split(), "${unit} (peer node): APP__AUTH__PUBLIC_URL is not in the unit's environment"
        assert "APP__AUTH__COOKIE_KEY=${authCookieKey}" in peer.succeed(
            "systemctl show -p Environment --value ${unit}.service"
        ).split(), "${unit} (peer node): APP__AUTH__COOKIE_KEY is not in the unit's environment"
      ''}
      peer.wait_until_succeeds("curl -fsS http://127.0.0.1:${servicePort}/readyz", timeout=60)
      peer.succeed("curl -fsS http://127.0.0.1:${servicePort}/healthz")
      peer.succeed(
          "sudo -u postgres ${postgres}/bin/psql -d ${unit} -c 'select count(*) from counters'"
      )

      # --- adoption: the schema was already there --------------------------
      #
      # `seed-legacy-schema.service` created both tables with columns this migration does not
      # describe (`deleted_at`, `migrated_by`), which is what a database from the system being
      # replaced looks like. The migration must have ADOPTED them: recorded itself as applied
      # and left the columns exactly as they were. Recreating would either fail (no
      # `if_not_exists`) or silently produce tables without those columns.
      peer.succeed(
          "sudo -u postgres ${postgres}/bin/psql -d ${unit} -Atc"
          " \"select column_name from information_schema.columns"
          " where table_name = 'counters' and column_name = 'deleted_at'\" | grep -qx deleted_at"
      )
      peer.succeed(
          "sudo -u postgres ${postgres}/bin/psql -d ${unit} -Atc"
          " \"select column_name from information_schema.columns"
          " where table_name = 'counter_events' and column_name = 'migrated_by'\" | grep -qx migrated_by"
      )
      peer.succeed(
          "sudo -u postgres ${postgres}/bin/psql -d ${unit} -Atc"
          " \"select count(*) from seaql_migrations\" | grep -qx 1"
      )

      # --- adoption: half the schema is there -------------------------------
      #
      # Only reachable with a migration that owns more than one table, and not something a
      # fixture can show: the decision reads the live schema. The service must refuse rather
      # than guess, and name both sets. Run as the service's own account over the socket, with
      # a second database seeded with one of the two tables. This runs the binary by hand, outside
      # the unit, so it does not inherit the unit's environment: `migrate` loads the whole
      # configuration, including any auth settings, before reaching the schema — otherwise
      # the output has no "half there" in it, which reads as a broken refusal.
      peer.succeed("sudo -u postgres ${postgres}/bin/psql -c 'CREATE DATABASE ${name}_half OWNER \"${unit}\"'")
      peer.succeed(
          "sudo -u ${unit} ${postgres}/bin/psql -d ${name}_half -c 'CREATE TABLE counters (id text PRIMARY KEY)'"
      )
      peer.succeed(
          "( sudo -u ${unit} env APP_CONFIG_DIR=${etcDir}"
          " APP__DATABASE__URL='postgres:///${name}_half?host=/run/postgresql'"
          "${authCommandEnvironment}"
          " ${package}/bin/${pname} migrate 2>&1 || true ) > /tmp/adopt-refusal"
      )
      peer.succeed("grep -q 'half there' /tmp/adopt-refusal")
      peer.succeed("grep -q 'counters' /tmp/adopt-refusal")
      peer.succeed("grep -q 'counter_events' /tmp/adopt-refusal")

      # The account is the one the module created, not a dynamic uid: that is what makes peer
      # auth work, and it is the assertion that would fail if `dynamicUser` were ignored. Its
      # name is the product prefix plus the service, which is what a host running two products
      # needs — and what the role above is named.
      peer.succeed("systemctl show -p User --value ${unit}.service | grep -qx ${unit}")
    '';
  };

  # --- the image -------------------------------------------------------------
  #
  # The podman-on-a-VPS path: the image the pipeline builds, loaded through
  # `streamLayeredImage` into `oci-containers`, in front of a database on the host. What this
  # proves and a build does not: the entrypoint exists and is executable, the baked
  # configuration is found through APP_CONFIG_DIR, the uid in `/etc/passwd` is the one the
  # process runs as, and the bundle baked at /usr/share is served with the SPA fallback.
  #
  # `--network=host` rather than a published port: the image's committed configuration points
  # at 127.0.0.1, which inside a podman bridge is the container's own loopback. A deployment
  # with a published port sets APP__DATABASE__URL; this test is about the image, not about
  # the network mode, and host networking is the smallest thing that isolates that.
  image = pkgs.testers.runNixOSTest {
    name = "${name}-image-vm";
    nodes.machine = lib.recursiveUpdate postgresNode {
      virtualisation = {
        memorySize = 2048;
        # Podman's storage plus a ~110 MB image does not fit the 1 GiB default.
        diskSize = 4096;
        podman.enable = true;
        oci-containers = {
          backend = "podman";
          containers.${name} = {
            image = "${name}:latest";
            imageStream = image;
            extraOptions = [ "--network=host" ];
            # The image bakes a configuration with NO database location (that is the fix this
            # test exists to keep true): a deployment supplies one shape, and this one is the
            # file a Kubernetes secret or a sops decrypted credential becomes. The value never
            # enters the image or its environment.
            volumes = [ "/etc/${name}-database-url:/run/${name}/database-url:ro" ];
            environment = {
              APP__DATABASE__URL_FILE = "/run/${name}/database-url";
            } // authEnvironment;
          };
        };
      };
      environment.etc."${name}-database-url".text =
        "postgres://counter:test-password@127.0.0.1:5432/counter";
      # The role and database the URL names, with the password it carries (NixOS's pg_hba asks
      # for `scram-sha-256` on 127.0.0.1).
      services.postgresql.initialScript = pkgs.writeText "counter-role.sql" ''
        CREATE ROLE counter LOGIN PASSWORD 'test-password';
      '';
      services.postgresql.ensureDatabases = [ "counter" ];
      services.postgresql.ensureUsers = [
        {
          name = "counter";
          ensureDBOwnership = true;
        }
      ];
    };
    testScript = ''
      machine.start()
      machine.wait_for_unit("postgresql.service")
      machine.wait_for_unit("podman-${name}.service")
      ${lib.optionalString hasAuth ''
        # `podman inspect` shows the environment the container was created with, so this is
        # the container's own view, not the unit's.
        assert "APP__AUTH__ISSUER=${authIssuer}" in machine.succeed(
            "podman inspect --format '{{range .Config.Env}}{{println .}}{{end}}' ${name}"
        ).split(), "${name}: APP__AUTH__ISSUER is not in the container's environment"
      ''}
      ${lib.optionalString hasWebAuth ''
        assert "APP__AUTH__PUBLIC_URL=${authPublicUrl}" in machine.succeed(
            "podman inspect --format '{{range .Config.Env}}{{println .}}{{end}}' ${name}"
        ).split(), "${name}: APP__AUTH__PUBLIC_URL is not in the container's environment"
        assert "APP__AUTH__COOKIE_KEY=${authCookieKey}" in machine.succeed(
            "podman inspect --format '{{range .Config.Env}}{{println .}}{{end}}' ${name}"
        ).split(), "${name}: APP__AUTH__COOKIE_KEY is not in the container's environment"
      ''}

      # The container's own `serve` applies the migrations on boot, so readiness is the whole
      # chain: entrypoint, config, uid, database.
      machine.wait_until_succeeds(
          "curl -fsS http://127.0.0.1:${servicePort}/readyz", timeout=180
      )
      machine.succeed("curl -fsS http://127.0.0.1:${servicePort}/healthz")
      machine.succeed(
          "sudo -u postgres ${postgres}/bin/psql -d counter -c 'select count(*) from counters'"
      )

      # The bundle, and the client-side fallback: a real file is served as itself, and any
      # other path is the entry point so a reload at a client route survives. Without the
      # fallback a scraper or a shared link gets the Connect 404 instead of the app.
      #
      ${if servesWebShell then ''
        # The index.html is the Trunk build's output (not the demo bundle), so it references a
        # hashed `.wasm` file — the signature of a real web shell.
        machine.succeed(
            "curl -fsS http://127.0.0.1:${servicePort}/ | grep -q '_bg.wasm'"
        )
        # The wasm binary is served with the correct MIME type — browsers refuse streaming
        # compile when it is not `application/wasm`.
        machine.succeed(
            "wasm=$(curl -fsS http://127.0.0.1:${servicePort}/ | sed -n 's/.*href=\"\\([^\" ]*_bg\\.wasm\\)\".*/\\1/p' | head -1) && " +
            "curl -fsS -o /dev/null -w '%{content_type}' \"http://127.0.0.1:${servicePort}/$wasm\" | grep -q 'application/wasm'"
        )
      '' else ''
        # No web shell is declared, so the bundle is the committed fallback: the entry point is
        # still served, and the Trunk-specific assertions do not apply.
        machine.succeed("curl -fsS http://127.0.0.1:${servicePort}/ | grep -qi 'html'")
      ''}
      machine.succeed(
          "curl -fsS http://127.0.0.1:${servicePort}/counter/42 | grep -qi 'html'"
      )
      # A missing asset is a 404, not the entry point: a stale hashed file answered with HTML
      # and a 200 is a parse error in a file the network tab reports as loaded.
      machine.succeed(
          "test \"$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:${servicePort}/assets/nonexistent.4f2c9d.js)\" = 404"
      )

      # The uid the image promises, on the process the container runs.
      machine.succeed(
          "podman inspect --format '{{.Config.User}}' ${name} | grep -qx '10001:10001'"
      )
    '';
  };
}
