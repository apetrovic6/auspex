# Maintenance recipes for the pipeline's derivations. Nothing here is a gate: these rewrite
# committed inputs that `nix/ci.nix` reads, so running one is always followed by a diff and
# a commit.
#
# Import with: mod deps '<relative-path-to-repo-root>/just/deps.just'

# The repository root. `source_directory()` is this file's directory even when the module is
# imported from the root Justfile, which is what makes it the root from here — unlike
# `justfile_directory()`, and unlike a script recipe's working directory, which is the
# directory of the file that DEFINES the recipe.
root := source_directory() / ".."

# The dependency snapshot `nix/deps-base` is the frozen input of the bottom crane layer
# (`depsL0` in nix/ci.nix). Rewriting it is called a REBASE, and it is deliberate: every
# dependency above the snapshot rebuilds on every Cargo.lock change, and the snapshot is what
# does not. Run this when `just deps::drift` reports more than ~15% of the registry graph has
# moved, or after a bump that reaches deep (syn, serde, libc, the gpui or sqlx closure) — the
# policy is [ADR 0025](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0025-dependency-artifacts-are-a-frozen-layer-and-a-delta-layer.md).
#
# Only the files a dependency build reads are copied. Source bodies are NOT: `mkDummySrc`
# replaces every target with a stub, so copying them would rebuild the bottom layer on every
# ordinary edit, which is the thing the split exists to prevent. The empty `src/*.rs` markers
# are here because `mkDummySrc` decides whether to emit a `src/lib.rs` stub by looking for the
# file, and a member with no stub is a manifest with no targets — a hard cargo error.
[script('bash')]
rebase-deps:
    set -euo pipefail
    cd '{{ root }}'

    dest="nix/deps-base"
    rm -rf "$dest"
    mkdir -p "$dest/.cargo"
    cp Cargo.toml Cargo.lock "$dest/"
    cp .cargo/config.toml "$dest/.cargo/"

    # The workspace's `members` globs, spelled out. `tools/*` and friends match README.md
    # files too, hence the manifest test.
    shopt -s nullglob
    members=(crates/* services/* tools/* apps/*/core apps/*/client apps/*/shells/*)
    copied=0
    for m in "${members[@]}"; do
        [ -f "$m/Cargo.toml" ] || continue
        mkdir -p "$dest/$m"
        cp "$m/Cargo.toml" "$dest/$m/"

        # Markers for the targets `mkDummySrc` discovers by reading the directory.
        for f in src/lib.rs src/main.rs; do
            if [ -f "$m/$f" ]; then
                mkdir -p "$dest/$m/src"
                : >"$dest/$m/$f"
            fi
        done
        for b in "$m"/src/bin/*; do
            rel="${b#"$m"/}"
            if [ -d "$b" ]; then
                [ -f "$b/main.rs" ] || continue
                mkdir -p "$dest/$m/$rel"
                : >"$dest/$m/$rel/main.rs"
            else
                mkdir -p "$(dirname "$dest/$m/$rel")"
                : >"$dest/$m/$rel"
            fi
        done
        copied=$((copied + 1))
    done

    echo "wrote $dest for $copied workspace members"
    echo "review the diff and commit it: the bottom dependency layer rebuilds when this tree changes"

# How far the delta layer has drifted from the snapshot: the set difference of (name, version)
# pairs whose checksum differs between the live Cargo.lock and nix/deps-base/Cargo.lock, plus
# any git sources that moved or appeared. It is a lower bound, not an estimate — one changed
# package recompiles its dependents too — so the authoritative signal is the
# `layered deps: L1 compiled N registry crates` line L1 prints in CI. The parser is a separate
# file because just would try to interpolate the braces in an inline python heredoc.
#
# Advisory, never a gate: drift is expected after every lock bump until someone runs
# `deps::rebase-deps`. The threshold and recipe for rebasing are [ADR 0025](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0025-dependency-artifacts-are-a-frozen-layer-and-a-delta-layer.md).
[script('bash')]
drift:
    set -euo pipefail
    cd '{{ root }}'

    if [ ! -f nix/deps-base/Cargo.lock ]; then
        echo "nix/deps-base/Cargo.lock is missing — run just deps::rebase-deps" >&2
        exit 1
    fi

    python3 '{{ root }}/just/deps-drift.py'
