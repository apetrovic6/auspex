---
name: verifying-changes
description: Check work in this repository — which command covers what, what CI runs, and what cannot be verified on Linux. Use before claiming something works, or when deciding which gate to run for a given change.
---

# Verifying a change

## The gate

Every command here runs inside `devenv shell` — `cargo` and `just` are not on the bare
PATH. Prefix a one-off with `devenv shell -- `. See AGENTS.md.

```sh
just ci          # check-repo, check, build, test — what CI runs
```

Inner loops, in increasing cost:

```sh
just rust-check                  # clippy the whole workspace, one pass
just rust-test                   # nextest the whole workspace
just check-repo                  # layout + version invariants + contract drift
just apps/<app>/dev              # one app: fix, check, build, test
just services/<name>/test        # one service's integration tests (no database needed)
```

Scope anything to a path: `just apps/counter/shells/web/check`.

## What each gate actually covers

- **`cargo xtask check`** — layout invariants and version single-sourcing; the run prints
  how many of each. No count is quoted here: the one that used to be said `~130` while the
  real figure had passed 185, and it moves with every check added.
  Catches: a crate outside every glob, a missing `[lints] workspace = true`, a `Cargo.toml`
  under `platforms/`, a stale Justfile `mod` path, an archetype with no template, a Kotlin
  package split between an app's `boltffi.toml`, its `codegen.rs` and its Gradle namespace,
  an `apps/<app>` path outside `apps/` that no rename list reaches, a Gradle or buf version
  that has drifted from `versions.toml`.
- **`just check`** — workspace clippy, *then* each module's own: wasm32 clippy for the web
  shell, Gradle lint, swiftlint. The second pass exists because cargo alone does not reach
  those.
- **`just proto::check`** — regenerates and diffs. All plugins local since RT-50; no network needed for codegen.
- **`just deny` / `just typos`** — supply chain and spelling. Not in `just ci`; CI runs them
  as their own job, and advisories also run on a schedule because a new advisory is a
  function of the date rather than of the commit.

## What `just ci` does NOT cover

| Not covered | How to check it |
|---|---|
| Android platform build | `devenv shell -- just apps/<app>/android` |
| Android unit tests | `just apps/<app>/platforms/android/test` |
| The platform skeletons | `just check-template` — scaffolds a product and builds a platform inside it |
| The archetype skeletons (`service`, `worker`, `lib-crate`, `cli-app`, `crux-app`, `web-app`) | `just check-template` — scaffolds a probe of each inside a generated product and runs the product's own checks. Codegen offline since RT-50 |
| The api generators (`new-migration`, `add-database`, `add-auth-oidc`, `new-feature`, `new-domain`, `new-rpc`) | the same `just check-template` — a probe walks the documented chain and runs the generated service's own `check` on what it writes (fmt, then clippy with `-D warnings`, since RT-43) and then its suite. The one step it fakes is `just services/<svc>/entities`: sea-orm-cli needs a live schema, so a committed fixture entity stands in, its shape was checked by hand against sea-orm-cli 1.1.20 on 2026-11-03, and the CLI is now pinned to the runtime so `check-versions` catches drift — but no gate runs sea-orm-cli, so a future bump still relies on the instruction beside the pin |
| The template round trip (`cli new`) | `just check-template` — generates a product with a non-default org, scaffolds an app, and asserts the identifiers agree. Tests `HEAD`, not the worktree |
| Anything on a device | `adb install` + `adb shell am start`, or an emulator |
| **The Apple build** | `just apps/<app>/apple` — **macOS only**, cannot run here |

The template gates are not in `just ci`, but they are not manual either: `ci-impure` runs
`check-template` and `check-cli-round-trip`, and `ci-pure` runs the template-only `cargo cli
check` as a derivation, all on every push.

The Apple shell is verified by nobody until someone builds it on a Mac. Do not describe it
as working.

## The pipeline

```sh
just pipeline                              # every stage, in workflow order
devenv build outputs.ci-test               # or one stage; nix/ci.nix is the list
```

`just pipeline` runs `devenv tasks run ci:run`, which is what `.forgejo/workflows/ci.yaml`
runs: every derivation stage in one `devenv build`, then the host steps. So a green
`just pipeline` and a green push mean the same thing.

A stage is a derivation unless it needs something a sandbox cannot have — `.git` history,
today's advisory database, or a network install at run time. Derivations are cached in the
binary cache every machine substitutes from, so the second run of an unchanged stage costs
nothing; host steps always run.

Two things this means for verifying a change:

- **A derivation is built from a fileset, not from the working tree.** `nix/ci.nix` names what
  goes in. A file the check reads at run time and the fileset does not include is a check that
  silently sees less than your checkout does — which is how `ci-proto-check` once passed while
  comparing a tree against a copy of itself. Verify derivation stages from a fresh clone.
- **A store path is read-only inside the sandbox.** A stage that regenerates into the source
  must copy it to a writable directory first, with `--no-preserve=mode` or the copy is
  read-only too.

No Apple job: every runner is Linux.

## Verifying honestly

This repository's history contains several checks that passed vacuously. Prefer evidence
over a green tick:

- **Negative-test a new check.** Break the thing it is meant to catch and confirm it fails,
  then restore. A test that has never failed has not been tested.
- **Drive the real thing where you can.** A TUI can be driven through a pty; a web shell
  through headless Chromium with `--dump-dom`; an Android app through `adb shell input tap`
  plus `uiautomator dump`. All of these have been used here and they beat reasoning.
- **Read the exit code you mean.** Piping through `tail` reports `tail`'s status, not the
  command's.
- **Say what you did not check.** "Compiles" is not "works", and "works" is not "works on a
  device".

## Long or backgrounded work

Gradle, emulator boots and a cold `devenv build` take minutes. Run them with generous timeouts
and inspect the log rather than polling. Stop anything you started — and scope a kill by PID:
a `pkill -f 'target/debug/api'` here once matched an unrelated process of the same name.
