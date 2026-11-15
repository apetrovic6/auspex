# 0005. One file pins every cross-tool version

- **Status:** Accepted
- **Date:** 2026-09-23

## Context

Several versions here are load-bearing in more than one place, and each is read by a tool
that cannot read the others' configuration. `boltffi` was pinned in three files; the Android
API level appears in `nix/android.nix`, in `build.gradle.kts` and in `boltffi.toml`, and the NDK
clang wrapper's *filename* embeds it.

A partial bump fails in ways that name neither cause. A protobuf plugin ahead of its Gradle
runtime surfaces as `ExceptionInInitializerError` the first time a message is constructed —
on a device, naming no file.

## Decision

`versions.toml` at the root. Nix reads it directly with `builtins.fromTOML`; `just` reads it
at recipe runtime. Cargo, Gradle, buf and Xcode cannot read it at all, so
`cargo xtask check-versions` asserts their literals agree — 21 checks, run in CI.

## Consequences

A bump is one edit plus whatever `xtask` then reports as disagreeing. The checks are the
documentation: they name the file, the key, both values and often why they must match.

Every new cross-tool version belongs there with a check, or the arrangement decays into a
file that is merely usually right. The dagger CLI and the `dagger-sdk` crate were added on
exactly that basis — they speak a session protocol versioned together, and a mismatch fails
at connect time naming neither side. Both pins left with the pipeline that needed them (ADR [0024](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0024-the-pipeline-is-derivations-and-a-shell.md)), which is the other half of the rule: a version nothing depends on is a version nothing
can disagree with, and keeping it is a check that can only ever pass.

## What was rejected, and why

**Pinning in each tool's own configuration.** Idiomatic per tool, and the failure mode is
what this repository already had: three copies of a version and a comment explaining that a
partial bump fails confusingly.

**Generating the other files from `versions.toml`.** No drift by construction, at the cost of
generated `Cargo.toml` and `build.gradle.kts` files nobody can edit directly. Checking is
cheaper than generating and leaves both files ordinary.
