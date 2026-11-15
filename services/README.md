# `services/` — long-running binaries

Backend processes: HTTP/RPC APIs, queue consumers, schedulers. Each is independently
deployable and owns its own configuration, migrations and container image.

Every direct child of this directory is a Cargo workspace member, picked up by the
`services/*` glob in the root `Cargo.toml`. Create a directory with a `Cargo.toml` and it
is a member — there is nothing to register.

    just add service <name>
    just add worker <name>

A service is created **stateless** — no `db.rs`, no migrations, no entities — because a
service owns its own schema or has none at all, and the template must not assume every one
wants Postgres. What a service grows afterwards is generated too:

    cargo xtask add-database  <service>                 # once, to give it a schema
    cargo xtask add-auth-oidc <service>                 # once, to verify OIDC access tokens
    cargo xtask new-migration <service> <name> --table T --field name:string
    cargo xtask new-feature   <service> <name>          # a whole vertical slice
    cargo xtask new-rpc       <service> <feature> <name> --field reason:string?
    cargo xtask new-domain    <service> <feature> <name>

The order matters and the tools enforce it — see the `scaffolding` skill.

> This README is not decoration. A `members` glob that matches **nothing** is a hard error
> ("failed to load manifest for workspace member `services/*`"), so while this directory
> holds no crates yet, this file is what the glob matches. Cargo silently skips matched
> *files* and only requires a `Cargo.toml` from matched *directories*. Do not delete it.

## Delivering one

A service is built **once**, as a Nix package, and everything a deployment needs derives
from that build. None of it is listed anywhere: `mkPipeline` discovers the service from its
own `[package.metadata.app] kind = "service"` and produces

    packages.<system>.<name>          the package (craneLib.buildPackage, the shared deps layer)
    packages.<system>.<name>-image    a streamLayeredImage derived from it
    nixosModules.<name>               services.<product>.<name> = { … } — see `nix/pipeline/module.nix`
    checks.<system>.<name>-module-vm  Postgres + the module + migrations + /readyz
    checks.<system>.<name>-image-vm   the image via podman + Postgres + /readyz

so `just add service <name>` is the whole registration step. The two VM checks are in
`packages` as well, which is how `ci-pure` runs them; `ci-archetypes` proves a *scaffolded*
service is offered the same outputs.

### Several products on one host

Every service is named after its crate, and every product scaffolds an `api`, so the module
namespaces what it creates by the **product** — `[publish].project` in `versions.toml`, which
`cli new` fills in from the product's own name:

    options        services.<product>.<name>
    unit           <product>-<name>.service
    account        <product>-<name>          (the default `user`/`group`; peer auth wants it)
    configuration  /etc/<product>/<name>.toml (APP_CONFIG_DIR is /etc/<product>)

Two products' modules can be imported into one system without meeting: each gets its own unit,
its own account and its own directory. `two-products-one-system` is the check that holds it —
it evaluates a system with two different product names and asserts both units, both files and
both accounts exist.

Two things stay the deployment's to separate, because no module can guess them: the **port**
(`openFirewall` then opens it) and the product's own `services.postgresql` database. Give the
second product a different `port` and its own `ensureDatabases`/`ensureUsers`.

One caveat in the other direction: a product whose own name is a nixpkgs service (`postgres`,
`nginx`) nests its options inside that service's tree and conflicts with it. Pick another name.

### Databases

`config/<name>.toml` deliberately carries no database location, so a deployment supplies
one shape — the structured fields, or one `url` (the service refuses both). Two are common.

A managed database hands over the whole URL; keep it in a file (CNPG's `<cluster>-app`
secret's `uri`, a sops secret, a systemd credential), because a password in the Nix store
is a password in the build log:

    services.<product>.api.database.urlFile = config.sops.secrets.database-url.path;

For **peer auth**, the deployment leaves `user` at the module's default — `<product>-<name>`,
which is the service's own name appended to the product's — and the role must carry that same
account, because peer auth compares the kernel's user with the role name. `ensureDBOwnership`
names one database per role, and nixpkgs asserts that such a database exists, so the database
takes the name too:

    services.postgresql.ensureDatabases = [ "<product>-api" ];
    services.postgresql.ensureUsers = [ { name = "<product>-api"; ensureDBOwnership = true; } ];
    services.<product>.api = { enable = true; dynamicUser = false; };
    services.<product>.api.database.url = "postgres:///<product>-api?host=/run/postgresql";

A service that runs a migration (`database.migrate`, the default) is ordered after
`postgresql-setup.service`, because that — not `postgresql.service` — is where nixpkgs
creates `ensureDatabases` and `ensureUsers`. A unit ordered after the server alone can
start before its role exists: `migrate` fails, `Restart = on-failure` with `RestartSec = 2`
burns the default start limit in ten seconds, and the unit ends in `start-limit-hit`. The
module VM holds `postgresql-setup.service` back on purpose so that ordering is what it
tests rather than a race it won.

### Credentials from an identity provider

A service that has run `cargo xtask add-auth-oidc` refuses to start without an issuer, and the
committed file cannot name one: an issuer names a deployment. A deployment sets it where it
sets the rest of its environment:

    services.<product>.api.environment.APP__AUTH__ISSUER = "https://id.example.com";

or, to keep it out of the Nix store, as a credential (the key is the variable's name, without
`_FILE`; see `credentials`): `services.<product>.api.credentials.APP__AUTH__ISSUER = <path>`.
A container sets `APP__AUTH__ISSUER` (or `APP__AUTH__ISSUER_FILE`) in its environment. The
module and image VM tests give such a service `http://127.0.0.1:1`, which nothing listens on:
keys are fetched on the first token and those tests present none (ADR 0027).

### Publishing

The template never pushes its own images. Every push validates each service's archive — the
`<svc>-image-validate` derivation, which ci-pure builds — and the push itself is a
master-only step that skips with a notice when the registry credentials are unset:

    HARBOR_USERNAME=… HARBOR_PASSWORD=… just publish::push

A product sets `[publish].project` in `versions.toml` (`cli new` fills it from the product's
own name), and `registry` if it is not `harbor.noosphere.uk`. The image tag is the git
revision being built. `just publish::validate` is the local spelling of the archive check —
entrypoint, user, exposed port and the environment the binary needs.
