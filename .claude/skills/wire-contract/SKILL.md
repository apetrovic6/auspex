---
name: wire-contract
description: Change the .proto wire contract and regenerate the Rust, Kotlin and Swift code. Use when adding or changing an RPC, message or service, or when generated code looks out of date — the output is committed and there are several ways to get it subtly wrong.
---

# Changing the wire contract

`proto/` is a buf module. The code generated from it is **committed**, so a plain
`cargo build` needs no buf, no protoc and no network. That is the property worth protecting.

```sh
just proto::lint       # buf lint
just proto::fix        # buf format -w
just proto::generate   # regenerate everything (NEEDS NETWORK)
just proto::check      # fail if committed output drifted
just proto::breaking   # wire compatibility against a base ref
```

## The loop

1. Edit the `.proto`.
2. `devenv shell -- just proto::generate` — `buf` and the local plugins are only on PATH there.
3. Build and fix whatever the new shape broke.
4. **Commit the generated output with the change.** `just proto::check` is in `just ci` and
   will fail otherwise.

## Where the output goes, and why it is split

| Plugin | Output | Consumed by |
|---|---|---|
| `protoc-gen-buffa` | `crates/proto/src/gen/buffa` | Rust messages |
| `protoc-gen-connect-rust` | `crates/proto/src/gen/connect` | Rust client + server stubs |
| `protocolbuffers/java` + `kotlin` | `proto/gen/kotlin` | the Android shell |
| `connectrpc/kotlin` | `proto/gen/kotlin` | the Android shell's client |
| `apple/swift` + `connectrpc/swift` | `proto/gen/swift` | the Apple shell |

Messages and stubs **must** go to different directories: both Rust plugins emit a per-package
`<pkg>.mod.rs`, and sharing one `out` fails with "file was generated multiple times".

The shell languages live beside the `.proto` rather than under a platform, because one
contract generates three languages — it is not Android's to own.

## Traps

**Generation runs offline.** The Kotlin and Swift plugins are now local, built by the dev
shell from `versions.toml` pins (RT-50). `cargo xtask check` refuses any remote plugin
line and verifies every local one is known; Gradle's `libs.versions.toml` and the
Apple `project.yml` are still cross-checked against the same file.

**An unpinned plugin is a runtime bug, not a build one.** Generated protobuf checks its own
version against the runtime at class-initialization time, so a plugin ahead of the Gradle
dependency fails as `ExceptionInInitializerError` the first time a message is constructed —
on a device, naming no file. If you bump a plugin, bump its runtime in the same change.

**`proto::check` regenerates rather than asking git.** `git diff` reports nothing for
untracked files, so a git-based check would pass vacuously before the generated tree is
first committed, and would conflate "not yet committed" with "out of date".

**Kotlin needs the Java plugin too.** The Kotlin protobuf output is DSL wrappers over the
Java classes, so `protocolbuffers/java` runs alongside `kotlin`, not instead of it — and the
Gradle module must put that directory on **both** `kotlin.srcDirs` and `java.srcDirs`, or
the client is visible and the messages it takes are not.

## Adding a service's contract

`just add service <name>` creates `proto/<name>/v1/<name>.proto` along with the crate. buf's
STANDARD lint requires a dedicated `<Method>Request`/`<Method>Response` per RPC — that is why
response types are not shared, and why a mapping has one branch per method.
