# 0004. Generated protobuf code is committed

- **Status:** Accepted
- **Date:** 2026-09-23

## Context

`proto/` is a buf module. Six plugins generate from it: Rust messages and Connect stubs into
`crates/proto/src/gen`, and Java, Kotlin and Swift into `proto/gen/`. Four of those plugins
are remote, fetched from buf.build.

## Decision

The generated output is committed, and `just proto::check` regenerates and diffs to keep the
committed copy honest. That check runs in CI as its own job.

## Consequences

A plain `cargo build` needs no buf, no protoc and no network — which matters for CI, for a
contributor who only touches Rust, and for a Gradle or Xcode build that has no idea buf
exists.

The cost is that changing a `.proto` is two steps: edit, then regenerate and commit both.
`proto::check` fails loudly when only the first happens.

`proto::check` deliberately does not ask git. `git diff` reports nothing for untracked
files, so a git-based check passes vacuously before the generated tree is first committed —
and conflates "not yet committed" with "out of date", which are different problems. It
snapshots, regenerates and compares instead.

## What was rejected, and why

**Generating in `build.rs`.** No committed artefacts and no drift possible, at the price of
protoc or buf on every machine that builds, network access for the remote plugins, and a
build that fails for reasons unrelated to the code being built. It also cannot serve Gradle
or Xcode, which do not run `build.rs`.

**Generating in CI only.** The generated code is a compile-time dependency of `crates/proto`,
so this just moves the failure to whoever builds without running the generator first.
