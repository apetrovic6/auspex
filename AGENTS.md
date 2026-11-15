# Working in this repository

A monorepo for Rust applications and services: Crux apps with native and web shells,
standalone web apps, CLIs, workers and backend services, in one Cargo workspace, one
toolchain, one command set. It is also a template — it gets instantiated per product, so
`apps/counter` is *the example app*, not the product.

Read this before changing anything. The conventions here are enforced by layout and by
`cargo xtask check`, not by convention alone, and several of them fail in ways that do not
name the real cause.

## The one gate

Everything below runs inside the dev shell. `cargo`, `just` and `typos` are **not** on the
bare PATH — outside the shell they fail with `command not found`, which reads like a broken
checkout rather than a missing shell. Enter it once with `devenv shell`, or prefix a single
command with `devenv shell -- `.

```sh
just ci        # what CI runs: check-repo, check, build, test
```

If that passes, the change is structurally sound. If you change only Rust,
`just rust-check && just rust-test` is the fast inner loop.

`just ci` does **not** cover: the Apple build (needs macOS), the Android platform build
(`just apps/<app>/android`), the archetype skeletons and the api generators (compiled by
`just check-template`, which scaffolds probes of `service`, `worker`, `lib-crate`, `cli-app`,
`crux-app` and `web-app` inside a generated product, and lints and runs the code
`new-migration`, `add-database`, `add-auth-oidc`, `new-feature`, `new-domain` and `new-rpc` write into a
service — the converted probe's own `check`, hence clippy with `-D warnings`), the
template round trip (`just check-template`, which generates a product from this repository,
inits it, and checks the result — structurally: nothing in the generated product is compiled
there. The compiled version is `just check-template yes`, and it runs on the schedule, not per
push, measured at +654 s against the 17 s structure path), or anything on a device. The
template gates run on every push: the CI `template` job runs `check-template`, and the
`cli round trip` job runs `check-cli-round-trip` (a product's `just add` fetches the CLI from
the forge at the pinned revision). See `verifying-changes`.

## Layout, and the rule that makes it work

```
apps/<app>/core          Crux core — portable logic, no I/O
apps/<app>/client        shell-side code the app's shells share (optional)
apps/<app>/shells/*      Rust shells: tui, web (Leptos/wasm), desktop (GPUI)
apps/<app>/platforms/*   Gradle and Xcode projects — NOT Cargo members
apps/<app>/app           single-crate app (CLI or web), when there is no core
crates/*                 reusable libraries
services/*               long-running binaries
tools/*                  repository tooling: xtask (checks, scaffolding), ci (the pipeline)
templates/*              skeletons for `just new`, and the platform ones `xtask
                         add-platform` falls back to — excluded from the workspace
proto/                   the wire contract (a buf module)
just/*.just              shared Justfile modules
nix/*.nix                the development environment, one file per concern
versions.toml            versions shared by Cargo, Nix, Gradle, buf and just
```

Workspace membership is **glob-driven**, so creating a directory in the right place is the
only registration step — no editing `Cargo.toml`.

Two consequences that bite:

- A glob matching a directory **without** a `Cargo.toml` is a hard error, and `exclude`
  does not accept globs to undo it. That is why `platforms/` sits outside every glob.
- A glob matching **nothing** is also a hard error. `apps/*/app` and `apps/*/client` are
  therefore conditional: present because something matches them today. **Deleting the last
  crate of that kind stops the whole workspace loading** — including `xtask`, so no check
  in this repository can warn you. Remove the glob from `Cargo.toml` and it loads again.

## Decisions that are settled

Do not relitigate these without a reason that is new. The reasoning is in `git log`, in the
commit that made each one.

- **The core stays domain-level; shells speak Connect.** A Crux core asks for
  `CounterApi::Increment { by: 1 }`, never `POST /counter.v1.CounterService/Increment`.
  Turning that into an RPC is each shell's job, using a client generated from the same
  `.proto` the server is built from. The core carries no protocol code into a mobile binary,
  and no two shells can disagree about the contract because none of them writes it.
- **The template ships an identity seam, not an authentication system.** `crates/identity`
  holds the boundary and nothing else: an identity type, a tower layer that parses a credential
  and attaches it (never rejects), a `VerifyCredential` trait that is the product's one
  insertion point, and a handler-side `ctx.principal()?`. Nothing that names a provider, mints
  a token or stores a password — the shipped verifier authenticates nobody, so a protected
  method refuses every caller until a product supplies one, and that is the point rather than a
  gap. Which methods are public is decided in a test that enumerates the routing table, because
  the default for a method nobody classified must not be "public". ADR 0012 records why,
  including the committed dev token that lost.
- **Entities live in the service that owns them**, not in a shared crate.
  `impl ActiveModelBehavior for ActiveModel` is two foreign types from anywhere else and the
  orphan rule rejects it.
- **Migrations live in the owning service**, as a module, applied at boot under a Postgres
  advisory lock keyed on the service name — so a rollout with N replicas has one migrating
  and the rest waiting.
- **Generated code is committed** (`crates/proto/src/gen`, `proto/gen/{kotlin,swift}`), so a
  plain `cargo build` needs no buf, no protoc and no network.
- **The org prefix is adopted on use.** Templates carry `com.example`; scaffolds and
  platform copies reconcile with the app's own `boltffi.toml`, and an invariant checks the
  agreement. `docs/adr/0010` records why adoption beats rewriting templates at `init`.
- **`versions.toml` is the single source of truth** for anything Cargo, Nix, Gradle, buf and
  just must agree on. Cargo and Gradle cannot read it, so `cargo xtask check` asserts they
  match. Change the value there first.
- **Lints live in `[workspace.lints]`**; every crate opts in with `[lints] workspace = true`.
  Never put lint flags on a command line.
- **Crate names are prefixed by their app** (`counter-core`, `counter-tui`), because two
  apps in one workspace must not both want to be called `core`.

## Traps

**Regenerating for Android is two steps, and half of it is worse than none.**
`just apps/<app>/generate-android` runs typegen (Kotlin types) *then* boltffi pack (the
`.so`). If the first succeeds and the second fails — it needs the NDK, so it fails outside
`devenv shell` — the Kotlin types and the native library disagree, and the app crashes at
launch with a bincode "Input is not large enough" that names nothing. Re-run it in
`devenv shell` until both steps pass.

**The toolchain is inside `devenv shell`, not on your PATH** — and not just the sharp-edged
parts. `cargo`, `just` and `typos` themselves are only there, so outside the shell every
command in this file fails with `command not found`. Inside it, the Android NDK linker
variables, `buf` and its plugins, `trunk` and `boltffi` are configured as well; those are
the ones that fail *confusingly* rather than obviously, which is why this entry used to
name only them.

**`just proto::generate` runs offline.** The five shell-language plugins that used to
 be remote (and hence rate-limited CI) are now local, built by the dev shell (RT-50).

**Do not commit build output.** `apps/*/platforms/*/generated`, `target/`, `dist/`,
`.devenv/` are all generated. An Android AVD under `platforms/android/.android` can be
several gigabytes.

**A git worktree inside the repository is a second copy of it, not a directory in it.** The
multi-agent harness puts them under `.claude/worktrees/` by default, so this happens on its
own. Every traversal rooted at the repository root skips nested checkouts through
`repo::is_nested_checkout` — `.git` present as a *file* or a directory, because `is_dir()`
misses every worktree — and `.claude/worktrees/` is gitignored so `git add -A` cannot stage a
copy. A new root-rooted walker has to call that predicate: `.git` sitting in a walker's skip
list hides the metadata directory by name, not the tree around it. When it was missing from
one walker, `just ci` reported over a hundred findings, all of them about files that were
already correct in the copy that counts.

**The Apple project cannot be built on Linux.** `xcodebuild` needs macOS, and every CI runner
is Linux. `just apps/<app>/apple` is a manual check.

## Style

Match the surrounding code. Specific to this repository:

- Comments explain **why**, especially where the obvious thing is wrong. If a line exists
  because something else failed confusingly, say so — that comment is the artefact.
- Decisions that would otherwise be re-argued are in `docs/adr/`, each recording what was
  rejected and why. Read those before proposing a different architecture.
- Commit messages carry the reasoning for everything smaller, including the option that
  lost.
- Justfiles are thin delegations to `just/*.just`. Shared logic goes in the module.
- Prefer a check in `tools/xtask` over a sentence in a README when a rule can be enforced.
