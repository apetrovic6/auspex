# 0001. One Cargo workspace, membership by glob

- **Status:** Accepted
- **Date:** 2026-09-23

## Context

The repository hosts many things at once: Crux cores with several shells each, standalone
web apps, CLIs, background workers, backend services and shared libraries. Adding one of
those must not be an archaeology exercise across half a dozen coupled files.

The previous layout assumed exactly one app. The root `Justfile` hardcoded a shell list
that had drifted — it named four shells that did not exist and omitted one that did — and
every crate was named for its role in a single-app world (`shared`, `web`, `tui`).

## Decision

A single root workspace whose `members` are globs, and a directory layout narrow enough for
those globs to be unambiguous:

```toml
members = ["crates/*", "services/*", "tools/*", "apps/*/core", "apps/*/shells/*", "apps/*/client"]
```

Creating a directory in the right place is then the only registration step. Aggregate
`just` recipes discover modules the same way, by globbing for `Justfile`.

Gradle and Xcode projects live in `apps/<app>/platforms/`, which no glob touches.

## Consequences

Adding an app, a shell, a service or a library requires no edit to `Cargo.toml` and no edit
to the root `Justfile`. A crate in the wrong place is silently not a member, so
`cargo xtask check` asserts the layout instead.

Two of Cargo's rules bite, and both are load-bearing:

- A glob matching a directory **without** a `Cargo.toml` is a hard error, and `exclude` does
  not accept globs to undo it. This is why `platforms/` sits outside every glob — which is
  honest anyway, since those are built by another toolchain entirely.
- A glob matching **nothing** is also a hard error. `apps/*/app` and `apps/*/client` are
  therefore conditional, added by `just new` on first use. Deleting the last crate of such a
  kind stops the workspace loading — including `xtask`, so no check here can ever warn about
  it. The scaffolder says so at the moment it adds one.

## What was rejected, and why

**Multiple workspaces, one per app.** Independent dependency resolution per app, at the cost
of one `Cargo.lock` per workspace, no shared `target/`, and cross-app libraries becoming
path dependencies across workspace boundaries. The sharing is the point here.

**An explicit `members` list.** Unambiguous, and wrong every time someone forgets to edit
it — which is exactly how the previous `Justfile` came to name four shells that did not
exist.
