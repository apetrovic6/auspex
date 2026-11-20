# Does the shipped code compile for the systems the flake declares, not only the one CI builds?
# Import with: mod cross 'just/cross.just'
#
# The flake builds `packages.auspex` for x86_64-linux, aarch64-linux, x86_64-darwin and
# aarch64-darwin; CI's derivations build x86_64-linux alone. The other three are one `cargo
# check --target` away, and that is what this does. It exists because PR #29's first fix round
# passed every gate and would not have compiled anywhere else: rustix's `Stat` field widths
# differ (`st_mode` is a u16 on darwin, `st_nlink` a u32 on aarch64 Linux), and the first look
# at aarch64-apple-darwin then found two more breaks on master (`O_PATH` and `mknodat` do not
# exist there).
#
# WHAT IS COVERED
#   - the library and binary targets of auspex-domain, -core, -effects and -tui: the code the
#     package is built from (`cargo build -p auspex-tui --bin auspex-tui`), type-checked for
#     aarch64-unknown-linux-gnu, aarch64-apple-darwin and x86_64-apple-darwin.
#   - the build scripts of their dependencies, run for real. ring and SQLite (rusqlite's bundled
#     C) compile C, so each needs a C compiler that targets the foreign system: see below.
# WHAT IS NOT
#   - test targets. The flake builds with `doCheck = false`, so no system but the host compiles
#     them, and some use Linux-only calls (`rustix::fs::mknodat`, which darwin lacks, in four places). Compiling them
#     for darwin would mean rewriting tests nobody ships, to catch breaks nobody can have.
#   - linking, and so anything the linker finds (a missing system library, a symbol): `cargo
#     check` never links. Nor does it run anything, so "compiles for aarch64" is not "works".
#   - x86_64-linux itself (the derivations cover it), and Windows (not a declared system).
#   - the Nix side of those systems: their derivations (`nativeBuildInputs`, the Apple SDK, the
#     darwin overrides in nix/auspex/platform-args.nix) are not evaluated or built. By design: a
#     check that evaluates another system's derivations needs a builder for it.
#
# THE C COMPILER. The host gcc/clang cannot compile sqlite3.c for aarch64 (its glibc headers are
# x86's: `gnu/stubs-32.h not found`), and nixpkgs has no Apple SDK for a Linux host. `zig cc`
# bundles the libc headers of every target, so `just/cross/zig-cc` and `zig-ar` hand cc-rs a
# compiler that works for all three; zig is in the dev shell (nix/tooling.nix). A stub compiler
# that does nothing also passes (nothing is linked), and was rejected: it would make the check
# stop reading the C, and cc-rs's compiler probing treats an unknown compiler as MSVC.
#
# WHERE IT RUNS. `just ci` (locally) and the `ci-impure` job, as a host step rather than a
# derivation, because it fetches the crates of the foreign targets (darwin-only dependencies are
# not in the host's closure) and a derivation would have to vendor them and build a cross C
# toolchain for each in the sandbox. The cost is that nothing is cached: about 25 s per target
# cold on 32 cores. The dependency layer `nix/deps-base` is not involved: it is the host-only
# layer of the derivations and nothing here reads it.

root := source_directory() / ".."

# rust triple, then zig's name for the same target
targets := "aarch64-unknown-linux-gnu:aarch64-linux-gnu aarch64-apple-darwin:aarch64-macos x86_64-apple-darwin:x86_64-macos"

# cargo check the shipped code for every declared system that is not the host
[no-cd]
[script('bash')]
check:
    set -euo pipefail
    cd '{{ root }}'
    cc='{{ root }}/just/cross/zig-cc'
    ar='{{ root }}/just/cross/zig-ar'
    # zig keeps a cache; the default is under $HOME, which a runner may not have writable.
    export ZIG_GLOBAL_CACHE_DIR="${ZIG_GLOBAL_CACHE_DIR:-$PWD/target/zig-cache}"
    # Every target is tried before the recipe fails, because a break is usually on more than one
    # (a darwin `u16` and an aarch64 `u32` are different errors in the same line).
    failed=()
    for pair in {{ targets }}; do
        rust="${pair%%:*}"
        zig="${pair##*:}"
        var="${rust//-/_}"
        echo '{{ style("command") }}cross check: '"$rust"'{{ NORMAL }}'
        env ZIG_TARGET="$zig" "CC_$var=$cc" "AR_$var=$ar" \
            cargo check --locked --target "$rust" --lib --bins \
            -p auspex-domain -p auspex-core -p auspex-effects -p auspex-tui \
            || failed+=("$rust")
    done
    if [ "${#failed[@]}" -ne 0 ]; then
        echo "cross check failed for: ${failed[*]}" >&2
        exit 1
    fi
