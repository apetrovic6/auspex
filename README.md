# rust-app-template

A monorepo template for Rust applications and services: Crux apps with native and web
shells, standalone web apps, CLIs, background workers, and backend services — in one
Cargo workspace, one toolchain, one command set.

> **Status:** the structure, workspace conventions and Crux app pipeline are in place.
> The backend service tier and the `just add` scaffolder are still being built out. See
> `docs/` as it lands.

## Layout

```
apps/<app>/
  core/           Crux core — the portable application logic
  client/         the Connect client the shells share (optional)
  shells/         Rust shells: tui, web (Leptos/WASM), desktop (GPUI)
  platforms/      Native projects: android (Gradle), apple (Xcode)
proto/            The wire contract (buf module)
crates/           Shared libraries — config, telemetry, generated proto code
services/         Long-running binaries (APIs, workers)
tools/            Repository tooling
just/             Shared Justfile modules
versions.toml     Versions shared by Cargo, Nix, Gradle and just
devenv.nix        The development environment — a list of the modules under nix/
nix/              Toolchain, SDKs and services, one file per concern
```

### The one rule that makes this work

Workspace membership is glob-driven, so **creating a directory in the right place is the
only registration step** — no editing `Cargo.toml`, no editing the root `Justfile`:

```toml
members = ["crates/*", "services/*", "tools/*", "apps/*/core", "apps/*/shells/*"]
```

Cargo treats a glob that matches a directory without a `Cargo.toml` as a hard error, and
`exclude` does not accept globs to undo it. That is exactly why Gradle and Xcode projects
live in `apps/<app>/platforms/` rather than alongside the Rust shells: they are built by
their own toolchains and must stay outside the globs. It is also why each category
directory keeps a `README.md` — a glob matching *nothing* is an error too, and Cargo
silently skips matched files while requiring a manifest from matched directories.

## Starting a product from this template

```sh
nix run .#new -- acme \
    --repository https://github.com/you/acme \
    --description "A monorepo of Rust applications and services." \
    --license "MIT OR Apache-2.0" \
    --org-domain io.acme
cd acme
```

`cli new` copies the committed template, applies the answers you pass as flags (it prompts
for any you leave out), renames the example app and the reverse-DNS prefix through the
CLI's own rename, drops the template-only machinery, and records the revision it came from
as the product's `template` input. `just add` inside the product then fetches the CLI from
that pinned revision, so an old product's scaffolder is the version its revision pinned —
the same property a lockfile gives a dependency graph. See
[ADR 0026](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0026-the-front-end-is-a-binary-and-products-pin-it.md).

Identifiers stay literal and renaming is code with tests, so a product is renamed the same
way any time afterwards, not only at instantiation:

```sh
cargo cli rename app counter billing   # directory, five crates, Kotlin package, Swift target
cargo cli rename org com.acme          # Gradle namespace, applicationId, bundle prefix
```

It leaves the *contract* alone — `counter.v1`, `CounterService` and the types generated from
them belong to the service that publishes them, not to a caller that happens to share a word
with it.

## Getting started

Everything is driven by [`devenv`](https://devenv.sh), which pins the Rust toolchain,
cross-compilation targets, Android SDK/NDK and web tooling:

```sh
devenv shell
just doctor        # report what is installed and what is missing
just dev           # format, lint, build and test everything
```

## Commands

Aggregate recipes discover modules automatically (any `apps/*/Justfile` or
`services/*/Justfile`), so they keep working as the repo grows:

| Command | Does |
|---|---|
| `just doctor` | check required tools, per module |
| `just check` | workspace lint, then each module's own (wasm, Gradle, swiftlint) |
| `just test` / `just build` | workspace crates, then each module |
| `just dev` / `just ci` | the local and CI workflows |
| `just check-repo` | layout invariants, version single-sourcing, contract drift |
| `just rust-check` / `just rust-test` | fast whole-workspace pass, skipping heavy shells |
| `just fmt` / `just deny` / `just typos` | format, supply-chain policy, spelling |

Scope to one thing with a path — `just apps/counter/check`,
`just apps/counter/shells/web/serve`, `just apps/counter/core/test`.

### The wire contract

`proto/` is a buf module; `crates/proto` holds the Rust code generated from it — messages
via `protoc-gen-buffa`, Connect RPC stubs via `protoc-gen-connect-rust`.

```sh
just proto::lint       # buf lint
just proto::generate   # regenerate messages and stubs
just proto::check      # fail if the committed output drifted from the .proto files
just proto::breaking   # wire-compatibility check against a base ref
```

The generated code is committed, so a plain `cargo build` needs no buf, no protoc and no
network. `proto::check` runs as part of `just ci`, comparing a fresh generation against
the current output, so the committed copy cannot drift.

### Running the stack

```sh
devenv up                 # postgres + api + web shell
devenv up postgres -d     # just the database, in the background
```

The web shell's Trunk proxy forwards `/api` to the API, so a call from the browser reaches
the same URL in development and in production. The TUI talks to it directly:

```sh
devenv up postgres api -d          # database and API, in the background
just apps/counter/shells/tui/run   # COUNTER_API_URL overrides http://127.0.0.1:5000/api
```

`services/api` is a Connect RPC service on axum. Each feature is a vertical slice —
`health/`, `counter/` — owning its own domain types, errors and a versioned service impl
(`service/v1.rs`), with `startup::register_services` as the single place that knows the
whole set. Migrations live in the service that owns the schema
(`services/api/src/migrations/`), not in a repository-wide crate: each service owns its own
database, and a stateless one has none.

```sh
just services/api/run        # serve (expects postgres)
just services/api/migrate    # apply migrations and exit
just services/api/test       # integration tests; needs no database
just services/api/entities   # regenerate SeaORM entities from the live schema
```

Migrations run at startup under a Postgres advisory lock keyed on the service name, so a
rollout with N replicas has exactly one migrating and the rest waiting.

### Working on an app

```sh
just apps/counter/dev                  # all Rust crates for this app
just apps/counter/shells/web/serve     # Leptos dev server on :3002
just apps/counter/shells/tui/run
just apps/counter/android              # typegen + boltffi pack + Gradle build
just apps/counter/apple                # macOS hosts only
```

The default recipes cover the Rust side only — fast, and they work on any machine with a
Rust toolchain. Native platform builds need an Android SDK or Xcode, so they are explicit
and get their own CI jobs.

### Who speaks Connect

The core asks for `CounterApi::Increment { by: 1 }` — a domain operation, not a request.
Turning that into an RPC is the shell's job, because every shell already has a client
generated from the same `.proto` the server is built from: `crates/proto` for Rust,
`proto/gen/kotlin` and `proto/gen/swift` for the native ones. The core therefore carries no
protocol code into a mobile binary, and no two shells can disagree about the contract,
because none of them writes it.

The cost is that each shell implements the call. The Rust shells share one
implementation, `apps/counter/client` — four operations onto four RPCs, and a failure into
a sentence a person can act on. It sits beside the app rather than in `crates/` because it
belongs to this app and no other, and it is generic over the transport because a native
shell dials with hyper and the browser shell cannot: it has no sockets, only `fetch`. That
is what the `native` feature selects.

`apps/*/client` is in `members` but, like `apps/*/app`, conditionally: it is there because
the counter app has one, and removing the last such directory means removing the glob too.

Its tests drive the real client against the generated *server* stubs through
`ServiceTransport`, in-process — no socket, no database, and no `native` feature, so they
cover the same code the wasm shell compiles.

The native shells write the mapping a third and fourth time — `CounterApiClient.kt` under
`platforms/android/shared`, `CounterApiClient.swift` under `platforms/apple/CounterApp` —
because the FFI boundary has two type worlds either side of it: bincode from the core,
protobuf from the contract, and nothing that knows about both. Kotlin has the same tests
the Rust client does, for the same reason: a mapping that sends `Increment` to `Decrement`
compiles cleanly in every one of these languages. Swift has no test target yet.

The browser gets its transport from `crates/connect-web`, because a wasm bundle has
`fetch` rather than sockets. `ClientTransport` requires a `Send` future and browser futures
are not, so the non-`Send` work runs on the local executor and only its result crosses back
through a oneshot — `oneshot::Receiver<T>` is `Send` whenever `T` is. No `unsafe`, and the
`JsValue` never leaves the thread that made it. On any other target the crate compiles to
nothing, so it stays an ordinary workspace member the host-side passes still visit.

### Code generation

A Crux core reaches native shells through two generators, both driven from the app's
Justfile so they share one working directory and one set of paths:

- **typegen** — serde/bincode types (`Event`, `ViewModel`, `Effect`) via `crux_core`
- **pack** — the FFI surface and prebuilt native libraries via `boltffi`

```sh
just apps/counter/generate           # every platform available on this host
just apps/counter/generate-android
```

Output lands in `apps/<app>/platforms/<platform>/generated/` and is **not** committed —
it is build output, regenerated on demand.

## Continuous integration

A stage is a derivation unless it cannot be. `nix/ci.nix` declares one `outputs.ci-<stage>` per
gate, and `.forgejo/workflows/ci.yaml` builds them — one step each, so the run page says which
failed and how long it took. What cannot be a derivation runs on the host as
`devenv shell -- just <recipe>`, so the recipe stays the only definition of the check.

The line between them is one question: does the stage need something a sandbox cannot have?
Three things qualify — `.git` history (`proto::breaking` compares against a base ref), today's
state of the world (`deny` fetches the advisory database, and a cached answer is the wrong
answer), and a network install at run time (Gradle, the CLI fetched from the forge). A file at a
pinned version does *not* qualify, which is why the CLDR slice and the buf codegen are both
derivations: `nix/cldr.nix` and `nix/proto-tools.nix` fetch what they need into the store once.

The same thing a push runs also runs on a laptop:

```sh
just pipeline       # every stage, in workflow order
devenv build outputs.ci-test    # or one stage; nix/ci.nix is the list
```

Three jobs, split by what they need so they run in parallel on different runners rather than
serially on one: `ci-pure` (the derivations), `ci-impure` (history and the advisory database),
`ci-android` (Gradle). Every runner carries the `nix` label, so no stage depends on one host.

Because a stage is a derivation, its cache is the binary cache every machine already
substitutes from — `https://ncps.noosphere.uk` — rather than a directory on whichever runner
happened to build it last. A stage built once for a commit is a store hit everywhere else, and
it is rebuilt exactly when one of its inputs changes, so there is no cache key to get wrong and
nothing to remember to invalidate. Measured: 14.9 min cold, 4.3 min warm.

And there is no Apple job — every runner is Linux and `xcodebuild` needs macOS natively, so
`just apps/<app>/apple` is a manual check.

Advisories run on a schedule as well as per-change, in `.forgejo/workflows/advisories.yaml`,
because RUSTSEC publishing against a dependency you already have is a function of the date
rather than of the commit. The nightly is also the only thing that writes back to the forge: it
turns a bump into a pull request and never merges it (ADR 0023).

Where every gate runs is written down in `gates.toml` and checked, because a recipe nothing
reaches reads exactly like a recipe everything reaches (ADR 0022). [ADR 0024](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0024-the-pipeline-is-derivations-and-a-shell.md) records why the
pipeline has this shape.

## Adding something new

Products commit `.template/recipe.toml`: `cli new` writes its resolved answers (including
authors from git config), and each successful scaffolding/API generator or CLI rename
appends its tool and arguments in order (xtask's check-only `--breakdown` is omitted).
This permanent file is generator-written; failed/refused commands and disposable build
output are not recorded. Concurrent recipe writes fail fast rather than lose history.
If an older product has no recipe, reconstruct it once by hand from its history:
`version = 1`, `[new]` fields `authors`, `repository`, `license`, `description`, `app`,
`org`, `drop_example`, then earlier `[[step]]` entries in order. If recording fails after
a generator succeeds, its error prints the exact entry to append once after fixing the
error; do not rerun the generator.
The template itself has no recipe. See [ADR 0030](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0030-products-update-by-regenerating-and-merging.md).

| `just add …` | Creates | Notes |
|---|---|---|
| `lib-crate <name>` | `crates/<name>` | a reusable library |
| `cli-app <name>` | `apps/<name>/app` | single crate, clap |
| `web-app <name>` | `apps/<name>/` | Leptos + Trunk, dev port allocated |
| `crux-app <name>` | `apps/<name>/` | Crux core + tui, web and desktop shells |
| `service <name>` | `services/<name>` + `proto/<name>/v1/` | Connect on axum, port allocated |
| `worker <name>` | `services/<name>` | scheduling loop, no HTTP |

A Crux app's native projects are added separately, because a Gradle or Xcode project is not
a Cargo crate:

```sh
just add platform-android <app>    # or platform-apple
```

It **derives** from an app that already has that platform, and falls back to the skeleton in
`templates/platform-<platform>` when no app does — `--from-template` forces the skeleton.
The order is the point: a donor is a project CI builds every run, so it works today and
carries whatever that app has grown, while a skeleton is a project nothing builds unless
something makes it. Three things do: `cargo cli check` pins its Gradle versions against
`versions.toml`, a unit test renders it, and `check-template` scaffolds a product and
*builds* from it. See [ADR 0009](docs/adr/0009-platform-projects-derive-first.md).

Either way, binary files — launcher icons, the Gradle wrapper — are copied byte for byte,
and every identifier is renamed through the same tested code path as `xtask rename`. The
app's Justfile needs no edit: it discovers `platforms/*` at runtime, so a product with one
platform has one platform and not two recipes pointing into a directory it will never have.
`native` and `generate` run what is there; `android` and `apple` stay typeable and refuse
before generating anything, with the `add-platform` command in the message (RT-13).

A Rust shell is added by the same shape of command and *not* by the same method:

```sh
just add-shell <app> <tui|web|desktop>
```

It copies the skeleton at `templates/crux-app/apps/__name__/shells/<shell>` and has no
`--from-template`, because deriving is the half of `add-platform`'s design that does not
carry over: a shell skeleton is complete, while a sibling app's shell is that app's own UI
code, its own answers to effects its own core has grown, and its own translation keys — the
exact thing a new shell exists not to inherit. What it does instead is the part a hand copy
gets wrong. It allocates the web shell's trunk port from the pool above, because a copied
`Trunk.toml` keeps the port it was copied from and two shells sharing one dev-server port is
a `just dev` failure nothing here runs; and it splices `shells/<shell>` into the app
Justfile's `rust_modules`, which is what that app's aggregate recipes walk — the half people
forget, and the failure is silent: the crate exists, `cargo build` builds it, and no check
ever runs it. It refuses instead: when the app is not a Crux app (`apps/*/app` has no core to
drive), when the shell is already there, and when the Justfile has no `rust_modules` able to
take the new crate. An app whose core has outgrown the skeleton's single `Effect::Render` — a
`client/` is the marker of one — gets a shell whose own match is not exhaustive, which is a
compile error naming the arm to add rather than a quietly wrong copy.

`crux-app` is the one that needs a caveat: it creates the Rust side — core, three shells,
the typegen and `boltffi.toml` — and not `platforms/`, which is `add-platform`'s job above
and is named in the generated app's own README. Nor does it wire an API, for the same reason
`just add service` is a separate command.

A template is an **overlay on the repository root** — `templates/service/` mirrors the tree
it adds — so one archetype can create a crate *and* its contract without the scaffolder
knowing either path.

Ports are allocated from two pools, one past the highest already in use: services from
5000, web dev servers from 3000. Two processes defaulting to the same port surfaces as a
confusing `AddrInUse`, or worse as one quietly answering the other's client.

Skeletons live in `templates/`, one directory per archetype, with `__name__`, `__snake__`
and `__pascal__` substituted in file contents and in file names. Every Rust skeleton is
compiled by something automatic: `check-template` instantiates a product and scaffolds
`service`, `worker`, `lib-crate`, `cli-app`, `crux-app` and `web-app` probes inside it, and
runs the product's own checks. It also compiles what the api generators write, which is not
a skeleton but has the same problem: `cargo xtask new-feature` and its three siblings build
their output in code rather than from a template, and a conversion that drops a field
compiles fine until something tries to.

This is not `cargo-generate`; it does not need to be. Scaffolding *inside* an existing
monorepo needs work a file-copier cannot do — registering a workspace glob, allocating a
port, adding a database — and it has to run offline and be testable, so the CLI owns it. The
CLI also owns instantiation (`cli new`), so there is one program for both jobs and no
`cargo-generate` to keep in agreement with it.

One consequence worth knowing: `apps/*/app` cannot ship in the manifest, because Cargo
rejects a glob matching nothing and the directory does not exist until the first
single-crate app does. `just add cli-app` adds it. Deleting the last such app means
removing it again, or the workspace stops loading — no check can warn you, because `xtask`
cannot build while the workspace is broken.

## Conventions

- **Lints** live in `[workspace.lints]`; every crate opts in with `[lints] workspace = true`,
  so cargo, rust-analyzer and CI agree without repeating flags on command lines.
- **Crate names** are prefixed by their app (`counter-core`, `counter-tui`), because two
  apps in one workspace must not both want to be called `shared`.
- **Shared Justfile logic** lives in `just/`; per-crate Justfiles are thin delegations.
