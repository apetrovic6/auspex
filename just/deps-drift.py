#!/usr/bin/env python3
"""Report how far the live Cargo.lock has drifted from the nix/deps-base snapshot.

Registry drift is the set difference of (name, version) pairs whose checksum differs
between the two locks (changed + added). It is a lower bound, not an estimate: one
changed package recompiles itself plus every crate that depends on it, so the actual
recompile count is what the delta deps layer (L1 in nix/ci.nix) prints in CI
("layered deps: L1 compiled N registry crates"). The rebase policy in [ADR 0025](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/8aaabf5c1f90d6a725b2f82526b55cf057a7d3da/docs/adr/0025-dependency-artifacts-are-a-frozen-layer-and-a-delta-layer.md) is
written against that count; this script is the cheap, build-free "is the snapshot
stale at all" check.

Git sources are reported separately, because a moved rev is the one thing the fixed
vendor path cannot represent: crane names a git vendor dir by hash of the URL WITHOUT
the locked rev, so `nix/ci.nix` disables the remap entirely when the lock has any git
source (correct, slower). Their drift is keyed by the full `source` string, which
includes the locked rev.

Run from the repository root. Advisory only, never a gate: drift is expected after
every lock bump until someone rebases.
"""

import sys
import tomllib


def packages(path):
    with open(path, "rb") as f:
        lock = tomllib.load(f)
    registry = {}  # (name, version) -> checksum
    git = {}  # (name, version) -> full source (includes the locked rev)
    for p in lock.get("package", []):
        src = p.get("source", "")
        key = (p["name"], p["version"])
        if src.startswith("registry+"):
            registry[key] = p.get("checksum")
        elif src.startswith("git+"):
            git[key] = src
    return registry, git


def diff(live, base):
    changed = sorted(k for k in live if k in base and live[k] != base[k])
    added = sorted(k for k in live if k not in base)
    removed = sorted(k for k in base if k not in live)
    return changed, added, removed


def main():
    live_reg, live_git = packages("Cargo.lock")
    base_reg, base_git = packages("nix/deps-base/Cargo.lock")

    reg_changed, reg_added, reg_removed = diff(live_reg, base_reg)
    git_changed, git_added, git_removed = diff(live_git, base_git)

    drift = len(reg_changed) + len(reg_added) + len(git_changed) + len(git_added)

    print(
        f"registry packages: live={len(live_reg)} snapshot={len(base_reg)} "
        f"(changed={len(reg_changed)}, added={len(reg_added)}, removed={len(reg_removed)})"
    )
    if live_git or base_git:
        print(
            f"git packages: live={len(live_git)} snapshot={len(base_git)} "
            f"(changed={len(git_changed)}, added={len(git_added)}, removed={len(git_removed)})"
        )
        if git_changed or git_added:
            print(
                "NOTE: a git rev moved or a git source appeared, so nix/ci.nix disables the "
                "fixed vendor path — the layering falls back to the per-lock path."
            )
    print(f"drift: {drift}")

    if drift == 0:
        print("snapshot matches the live lock; L1 recompiles nothing on a no-change commit.")
    else:
        print(
            "at least this many crates recompile in L1, plus their dependents; "
            "the authoritative count is the 'layered deps: L1 compiled N registry crates' "
            "line in CI. See [ADR 0025](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/8aaabf5c1f90d6a725b2f82526b55cf057a7d3da/docs/adr/0025-dependency-artifacts-are-a-frozen-layer-and-a-delta-layer.md) for when to run just deps::rebase-deps."
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
