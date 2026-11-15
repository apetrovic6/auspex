---
name: scaffolding
description: Add a new app, service, worker, library or CLI to this monorepo. Use when asked to create a crate, add an app or service, or wire something new into the workspace — and before hand-rolling directories, because membership is glob-driven and the globs have sharp edges.
---

# Adding something to the monorepo

Always `just new`, never by hand. The scaffolder does work a file copy cannot: it registers
a workspace glob, allocates a development port, and formats what it writes.

```sh
just add <archetype> <name>
```

| Archetype | Creates | Notes |
|---|---|---|
| `lib-crate` | `crates/<name>` | a reusable library |
| `cli-app` | `apps/<name>/app` | single crate, clap |
| `web-app` | `apps/<name>/` | Leptos + Trunk, dev port allocated |
| `crux-app` | `apps/<name>/` | core + tui/web/desktop shells, typegen, dev port; `--shells tui[,web,desktop]` creates only those (a terminal-first app), `just add-shell <app> <shell>` adds one later |
| `service` | `services/<name>` + `proto/<name>/v1/` | Connect on axum, port allocated |
| `worker` | `services/<name>` | scheduling loop, no HTTP |

Names are lower-case ASCII, digits and dashes, not starting with a digit. The scaffolder
refuses to overwrite, so a partial re-run is safe.

## After scaffolding

```sh
just check-repo                  # layout and version invariants
just apps/<name>/dev             # multi-crate apps
cargo check -p <name>            # single crates
```

A `service` also needs `just proto::generate` before it compiles — it ships a `.proto` and
the generated code is committed, not built on demand.

## Growing a service from the inside

`just add service` creates a service. These create what it grows afterwards — and every one
of them does most of its work *editing* files that already exist, which is the half people
forget: a migration nothing lists is inert, a slice nothing mounts answers 404, a method
nothing classifies fails the auth suite.

```sh
cargo xtask add-database  <service>                      # once, if it has no schema yet
cargo xtask add-auth-oidc <service> [--issuer URL] [--audience A]   # once, to verify OIDC tokens
cargo xtask new-migration <service> <name> [--table T] [--field name:type]...
cargo xtask new-feature   <service> <name> [--table T]
cargo xtask new-rpc       <service> <feature> <name> [--field name:type]... \
                          [--response-field name:type]...   # one more method, later
cargo xtask new-domain    <service> <feature> <name>     # one more DTO, later
```

`add-auth-oidc` replaces the shipped verifier (`NoIdentities`, which accepts nobody) with one that
checks an OIDC provider's RS256 access tokens, and is independent of `add-database`. It writes
`crates/oidc` once (a second service only gets wired), an `[auth]` section, and a test that drives
`Whoami` through the real router. The service then refuses to start without `APP__AUTH__ISSUER`.
It does not touch readiness, on purpose. `docs/adr/0027` has the reasons.

`new-rpc` is the one to reach for when the method is new: it writes the `rpc` line and its
two messages into the contract, the DTO and both conversions, the handler, and the
classification. `new-domain` writes only the DTO, for a payload the contract already
declares — running it twice is not `new-rpc`, because the method stays undeclared and
unimplemented.

A method's fields come from the command line rather than from the entity, unlike
`new-feature`'s: a request is not a row. The handler it writes is a `todo!` — everything
mechanical about the method is generated, and what it *does* is not.

**The order is not negotiable**, because `just services/<svc>/migrate` runs the service
binary and so needs the crate to compile:

```sh
cargo xtask new-migration api create_lakes --table lakes --field name:string --field note:string?
just services/api/migrate
just services/api/entities        # sea-orm-cli reads the live schema
cargo xtask new-feature api lake  # fields come from src/entities/lakes.rs
just proto::generate              # the slice will not compile until you do
```

Generate the slice first and it names an entity that does not exist, which breaks the
`migrate` that would have created it. `new-feature` refuses rather than letting you.

`--field` takes `name:type` — `string`, `bool`, `int32`, `int64`, `double`, `bytes` — with
`?` for nullable. `id`, `created_at` and `updated_at` are written for you and are not
fields.

### What a feature slice is

```
proto/<name>/v1/<name>.proto        Create/Get/List/Update, buf STANDARD-shaped
services/<svc>/src/<name>/
  domain/mod.rs   <Pascal>, <Pascal>Dto, and the conversions to and from the contract
  models.rs       ActiveModelBehavior, Dto -> ActiveModel, Model -> domain, apply_update
  errors.rs       <Pascal>Error, mapped to ConnectError (NotFound is not internal)
  service/v1.rs   the four handlers
```

The conversions are the point: `Request -> Dto -> ActiveModel` in, `Model -> domain ->
Response` out. Written by hand that is four impls, each one a place to drop a field
silently — the conversion still compiles and the column is simply never written.
Generating them from one field list is what makes all four agree.

`services/api/src/counter/` is the worked example in-repo. It has no `From<Model>`, and the
comment there says why: its reads and writes are single upserts so concurrent increments
cannot be lost, so no `Model` is ever materialised.

### After generating a feature

Two things the generator deliberately leaves:

- **Classify the new methods** in `services/<svc>/tests/<svc>/auth.rs`. The test enumerates
  the routing table and fails until every method is named, because the default for a method
  nobody thought about must not be public.
- **`list` is unbounded.** Pagination is a contract decision — a page token in the request,
  a next token in the response — so it is left to be made.

`cargo xtask check` has two invariants behind this: every migration file is in its
`Migrator`, and every slice with a `service/v1.rs` is mounted in `register_services`.

## Native platform projects

Not created by `crux-app`, because a Gradle or Xcode project is not a Cargo crate. Added
separately:

```sh
just add platform-android <app>    # or platform-apple
```

Two sources, and it picks: **an app that already has that platform**, by preference, because
that project is built by CI and carries whatever it has grown; **the skeleton** in
`templates/platform-<platform>`, when no app has one. `--from-template` forces the skeleton.

Build output is skipped, binaries are copied byte for byte, and identifiers — the Gradle
namespace, `applicationId`, the Kotlin package and its source tree, the Swift target and
bundle prefix — are substituted as they are copied. From a donor that is the `xtask rename`
code path; from the skeleton it is the sentinels. Either way the copy then reconciles with
the organisation prefix the app's own `boltffi.toml` declares — the same adoption
`just add crux-app` performs on the core it writes, and an invariant in `cargo xtask check`
fails if the two ever disagree ([ADR 0010](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0010-the-org-prefix-is-adopted-on-use.md)). The app's Justfile discovers `platforms/*` at
runtime, so it needs no edit.

Either source can be the wrong shape for the core, and it says which way round: a donor
whose core has an API effect brings a client naming types your core never defines; the
skeleton fulfils only `Render`, so a core that asks for more gets a project that cannot
answer. The exhaustiveness check in `Core` names every site either way.

After changing a skeleton, run `just check-template` — it scaffolds a product, generates a
platform from the skeleton inside it and builds it. Nothing else compiles that Kotlin.

**A way to reach a server.** The generated core's only effect is `Render`. Adding an API
effect is additive; see the `crux-shells` skill and `apps/counter` for the worked example.

## Workspace globs — the part that bites

`crates/*`, `services/*`, `tools/*`, `apps/*/core` and `apps/*/shells/*` ship in
`Cargo.toml`. `apps/*/app` and `apps/*/client` are **conditional**: Cargo rejects a glob
matching nothing, so they may only be present once something matches.

`just new` adds a glob on first use. The corollary it prints, and that you should believe:
**deleting the last crate matching a conditional glob stops the workspace loading** — which
takes `xtask` with it, so no check here can ever warn you. Remove the glob from `Cargo.toml`
and it loads again.

Never put a `Cargo.toml` under `apps/*/platforms/` — no glob reaches it, so it would be
silently excluded from every build, lint and test. An invariant checks this.

## Adding an archetype

A directory under `templates/`, a variant in `Archetype` in `tools/xtask/src/scaffold.rs`,
and an invariant keeps the two in step. Templates are **overlays on the repository root**:
`templates/service/` mirrors the tree it adds, which is how one archetype creates a crate
*and* its contract without the tool knowing either path.

Sentinels substituted in file contents and in file and directory names: `__name__`
(kebab), `__snake__`, `__pascal__`, `__port__`. Generated `.rs` is run through rustfmt
afterwards, because import order depends on the substituted name and no fixed order in a
template is right for every one.
