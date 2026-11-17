# 0100. Dependency artifacts are a frozen layer and a delta layer

- **Status:** Accepted
- **Date:** 2026-11-17

## Context

This ports rust-app-template 8aaabf5 (RT-76, AUS-28). The template-only [ADR 0025](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/8aaabf5c1f90d6a725b2f82526b55cf057a7d3da/docs/adr/0025-dependency-artifacts-are-a-frozen-layer-and-a-delta-layer.md)
was not inherited by auspex; its amendment is retained below as a product record.
The existing product snapshot is rebased deliberately, so this port does not claim
byte-identical L0 reuse with the template. Product fixtures, test tools and extra
stages remain product-owned. The pinned external CLI is unchanged; its generators
need the upstream CLI revision to acquire the same lock refresh behavior.
The plain flake keeps its full toolchain; only devenv CI opts into `depsToolchain`.


## Amendment (2026-11-15, RT-76): the bottom layer is shared across products

**Context.** A new product compiled its whole dependency graph on its first `ci-pure` although
the template had built and pushed nearly the same crates. Three things made a product's L0 a
different derivation from the template's, each on its own enough:

- `cli new` rewrote `nix/deps-base` for the product's workspace (`deps::rebase-deps`);
- L0's cargo command (`--exclude …`, the native-off `-p …` passes) and `version` were read from
  the LIVE tree's discovery, not from the snapshot L0 builds;
- the toolchain's derivation carries the cross targets discovery derives from the tree (wasm32
  for a web shell, the Android triples), so a terminal-only product had a different toolchain
  even with the same pins.

Separately, auspex's first `ci-pure` FAILED: `add crux-app` wrote a member whose dependencies the
committed `Cargo.lock` lacked, and the delta layer, vendored from that lock, stopped at "no
matching package named `boltffi` found". Reproduced: a stale snapshot alone builds (L1 compiled
130 crates); a stale live lock fails, whatever the snapshot is. This section's "a stale snapshot is
slower, never wrong" holds. The thing that broke was the lock.

**Decision.**

- **L0 is keyed by the snapshot, not by the product's tree.** `depsL0` reads its cargo arguments
  and version from `discovery.nix` over `nix/deps-base`, and is built with the host-only toolchain
  (`depsToolchain`, `nix/toolchain.nix` `hostOnly`). L0 compiles for the host and nothing else.
  Three inputs remain the product's, and each one makes L0 product-private. That is correct, just
  not shared. They are the pins (`flake.lock`/`devenv.lock`, which `new` copies), `pname` in
  `nix/ci.nix` (`new` keeps `rust-app-template`), and the git-source gate. When the LIVE lock pins
  any git crate, the fixed vendor path is dropped for every layer (above), and L0 changes with it.
  Keying that gate on the snapshot instead would gain nothing, because L1 cannot reuse an L0
  vendored at another path. The flake's `packages` call `mkPipeline` without `depsToolchain`;
  CI builds through devenv, so only CI's L0 is the shared one.
  The layers above keep the full toolchain. The rustc and host std are the same components either
  way, and cargo keys a unit by `rustc -vV`, not by the sysroot's store path. Measured: the
  template's L1 over the host-only L0 compiles 4 registry crates, the same count as master's L1
  over its own L0 (the lock's drift of 3, plus a dependent).
- **`cli new` keeps the template's snapshot byte for byte.** It is captured right after the copy
  and restored after every rewrite (`init`, the example drop and the ADR-link rewrite would
  otherwise touch its manifests), so a new product's L0 is the derivation the template's CI
  already pushed. The snapshot names template members the product lacks. That is harmless: L0
  builds the snapshot's workspace on its own terms, and L1 builds the product against its own
  lock. `rebase-deps` remains the fallback when the source has no snapshot (the `ci-archetypes`
  sandbox slices it out).
- **Generators leave `Cargo.lock` covering the workspace** (`xtask::cargo_lock::refresh`, after
  `cli add`/`add-shell`/`add-client`/`rename app` and the xtask generators). Before resolving,
  the live lock is seeded with the snapshot lock's registry entries it lacks. Cargo prefers a
  locked version for a package it needs and prunes the rest, so new dependencies resolve to the
  versions L0 already compiled. On `new --drop-example` + `add crux-app --shells tui`, 26 of 609
  registry crates fell outside the template's lock when resolved cold, against 0 of 603 seeded.
  L1 also checks `cargo metadata --locked --offline` first, so a lock committed stale anyway
  fails with a message about the lock, not about a crate.

**Measured** (phalanx, 32 cores, fresh `new --drop-example` + `add crux-app scout --shells tui`,
the template's layers in the local store):

| | L0 | L1 | registry crates compiled | deps-layer wall |
| --- | --- | --- | --- | --- |
| before | built: 325 crates, 410 MB | built: 130 crates | 455 | 82 s |
| after | **substituted** (the template's, 1.3 GB) | built: 201 crates | 201 (402 of 603 reused) | 49 s |

**The floor, and why it is not zero.** The 201 are not version drift; `deps::drift` reports 0.
They are feature unification. Resolver 2 unifies features per dependency kind across the
workspace's members, and a product has a different member set from the template's. Example: the
product's host-side `serde_core` unit lacks `rc`, which some template build dependency turns on,
so `serde_core` and everything above it is a different unit. A shared layer can only hold the
template's feature sets. Per-crate derivations would key on the same features and hit the same
floor, at hundreds of derivations of evaluation. An L0 pass shaped like a typical product
(`-p` over the crates `--drop-example` keeps plus a terminal app) could close part of the gap; it
is not done, because it hard-codes a product shape into the template's layer.

**Rejected.**

- **A synthetic registry-only bottom layer keyed by crates, features and profile.** It would hold
  someone else's feature sets, exactly as the template's L0 does, at the cost of a generator for
  synthetic workspaces. Keeping the template's snapshot gets the same reuse with no new machinery.
- **Per-crate derivations (crate2nix).** Several hundred derivations to evaluate per product,
  either through import-from-derivation (the ~390 s evaluation this ADR removed) or through a
  committed `Cargo.nix` that every lock change must regenerate. They reach the same feature floor.
- **sccache.** [ADR 0024](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0024-the-pipeline-is-derivations-and-a-shell.md)'s reasons stand: a second cache with its own invalidation story, over a
  content-addressed store every runner trusts.
- **Keeping the template's snapshot only when the product's dependencies are a subset.** At `new`
  they always are (the product's lock is the template's, pruned). Later, a superset product still
  gains, because L1 recompiles only the delta. The condition could only ever choose the slower path.

**Consequences.**

- The template's L0 rebuilt once when this landed (new toolchain input). Every product generated
  afterwards substitutes it until it runs `deps::rebase-deps`.
- **The rebase policy, for a product on the template's snapshot.** Such a product's L1 starts at
  the feature-unification floor (201 crates in the measurement above), well over the ~107-crate
  bar, with `deps::drift` at 0. That count is the floor, not drift, and rebasing it away buys a
  product-private L0 that every cold runner then builds. The bar for such a product is growth
  beyond the count its L1 printed on its first build, or `deps::drift` passing ~15% of the
  registry graph, whichever comes first. A product that rebases has its own snapshot from then on,
  and the original policy applies to it unchanged. A template update that regenerates
  `nix/deps-base` ([ADR 0030](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0030-products-update-by-regenerating-and-merging.md), step 4) brings it back onto the template's.
- **New dependencies take the snapshot's versions, not the newest.** That is the point of the
  seeding: those are the versions the template's CI builds and audits. They lag a cold resolve
  (25 of 603 crates were older in the measurement) and lag further as the snapshot ages. A
  seeded entry counts as a previous-lock entry, so cargo will accept a version that was yanked
  after the snapshot was taken; `deny.toml`'s `yanked = "deny"` in the advisories job is what
  catches that, and `cargo update -p <crate>` moves one forward as usual.
- **Two gates hold this.** `check-template` evaluates `outputs.deps-l0.drvPath` in the template
  and in the product it generates, and fails when they differ. `check-archetypes` runs every `cargo xtask`
  between its generators as `cargo --locked xtask`, and `cargo metadata --locked --offline` after
  the last one. A cargo command without `--locked` re-resolves a stale lock silently, so one plain
  `cargo xtask` after a generator would repair the lock and mask the generator.
- Products generated before this (pilot, auspex) carry their own `nix/pipeline` and their own
  snapshot, and build exactly as before until a template update brings this code.
- `cli rename app` over a name the template's snapshot also uses rewrites the snapshot and makes
  L0 product-specific again. That is correct and slower, the same as a rebase.
- The product invariant against the template-only `cargo cli` alias now skips the snapshot's
  `.cargo/config.toml` while the snapshot carries `tools/cli` itself. There the alias names a
  crate the snapshot contains.
