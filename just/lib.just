# Toolchain checks shared by every module.
# Import with: mod lib '<relative-path-to-repo-root>/just/lib.just'

# Versions come from versions.toml, the single source of truth. source_directory() is the
# directory of THIS file even when imported from a deeply nested Justfile — unlike
# justfile_directory(), which resolves to the importing file's directory and would give a
# different answer for every caller.
versions_file := source_directory() / ".." / "versions.toml"

# (internal) print one dotted key from versions.toml, e.g. `tools.boltffi`.
# Looked up at recipe runtime rather than with just's `shell()` at parse time, so that
# `just --list` and every unrelated recipe stay instant.
[no-cd]
[script('bash')]
_version key:
    set -euo pipefail
    python3 -c 'import sys, tomllib, functools; print(functools.reduce(lambda node, part: node[part], sys.argv[2].split("."), tomllib.load(open(sys.argv[1], "rb"))))' '{{ versions_file }}' '{{ key }}'

# Check if a tool is installed, report status (does not fail build)
[no-cd]
[script('bash')]
check-tool tool install_cmd:
    if command -v {{ tool }} >/dev/null 2>&1; then
        echo '  ✓ {{ tool }}'
    else
        echo '  {{ style("error") }}✗ {{ tool }}{{ NORMAL }} — install with: {{ install_cmd }}'
    fi

# Check if a tool is installed, report status, and fail if missing
[no-cd]
[script('bash')]
require-tool tool install_cmd:
    if command -v {{ tool }} >/dev/null 2>&1; then
        echo '  ✓ {{ tool }}'
    else
        echo '  {{ style("error") }}✗ {{ tool }}{{ NORMAL }} — install with: {{ install_cmd }}'
        exit 1
    fi

# Check that the installed BoltFFI CLI matches the version used by the apps
[no-cd]
[script('bash')]
check-boltffi:
    set -euo pipefail
    want=$(just --justfile '{{ source_file() }}' _version tools.boltffi)
    install_cmd="cargo install boltffi_cli --version '=$want' --locked"
    if ! command -v boltffi >/dev/null 2>&1; then
        echo "  {{ style("error") }}✗ boltffi{{ NORMAL }} — install with: $install_cmd"
        exit 0
    fi
    actual=$(boltffi --version 2>/dev/null | head -n 1)
    expected="boltffi $want"
    if [ "$actual" = "$expected" ]; then
        echo "  ✓ $actual"
    else
        if [ -z "$actual" ]; then actual='boltffi (unknown version)'; fi
        echo "  {{ style("error") }}✗ $actual{{ NORMAL }} — expected $expected; install with: $install_cmd"
    fi

# Check if a file exists, report status (does not fail build)
[no-cd]
[script('bash')]
check-file file setup_cmd:
    if [ -f {{ file }} ]; then
        echo '  ✓ {{ file }}'
    else
        echo '  {{ style("error") }}✗ {{ file }}{{ NORMAL }} — create with: {{ setup_cmd }}'
    fi

# Check if a Rust target is installed for the toolchain actually on PATH.
# Do not ask rustup: under Nix/devenv the toolchain comes from the nix store
# (languages.rust.targets) and rustup has no toolchains at all, so
# `rustup target list --installed` fails and every target looks missing.
# The sysroot is authoritative for both nix-provided and rustup-provided
# toolchains: installed components land in <sysroot>/lib/rustlib/<target>.
[no-cd]
[script('bash')]
check-rust-target target:
    target='{{ target }}'
    sysroot=$(rustc --print sysroot 2>/dev/null || true)
    if [ -n "$sysroot" ] && [ -d "$sysroot/lib/rustlib/$target" ]; then
        echo "  ✓ $target"
        exit 0
    fi
    if [ -n "${IN_NIX_SHELL:-}" ] || [ -n "${DEVENV_ROOT:-}" ]; then
        hint="add \"$target\" to languages.rust.targets in nix/rust.nix, then re-enter the shell"
    else
        hint="rustup target add $target"
    fi
    echo "  {{ style("error") }}✗ $target{{ NORMAL }} — install with: $hint"

# Report which Rust toolchain provider is active, since that decides how a
# missing target is installed. Nix/devenv builds the toolchain in the store with
# languages.rust.targets baked in and there is no rustup toolchain to add to
# (a stray rustup binary on PATH has no toolchains: `rustup show active-toolchain`
# fails with "no active toolchain"), so only report rustup when it is really in use.
[no-cd]
[script('bash')]
check-rust-toolchain:
    if [ -n "${IN_NIX_SHELL:-}" ] || [ -n "${DEVENV_ROOT:-}" ]; then
        echo "  ✓ rust toolchain from Nix/devenv ($(rustc --version 2>/dev/null || echo 'rustc not on PATH'))"
        echo '    targets come from languages.rust.targets in nix/rust.nix, not rustup'
    elif command -v rustup >/dev/null 2>&1; then
        echo "  ✓ $(rustup --version 2>/dev/null | head -1)"
    else
        echo '  {{ style("error") }}✗ rustup{{ NORMAL }} — install with: https://rustup.rs (or use devenv: languages.rust.targets)'
    fi

# Ensure the given Rust targets exist for the toolchain actually on PATH.
# Under Nix/devenv the targets are built into the store sysroot, so this only
# verifies them and points at languages.rust.targets; rustup is only used when it
# genuinely owns the toolchain. Keeps `just <app>/build` working in both setups.
[no-cd]
[script('bash')]
ensure-rust-targets *targets:
    set -u
    sysroot=$(rustc --print sysroot 2>/dev/null || true)
    if [ -z "$sysroot" ] || [ ! -d "$sysroot" ]; then
        echo '  {{ style("error") }}✗ no rustc sysroot on PATH{{ NORMAL }} — install Rust: https://rustup.rs, or enter the devenv shell'
        exit 1
    fi
    set -- {{ targets }}
    if [ $# -eq 0 ]; then
        echo '  {{ style("error") }}✗ ensure-rust-targets called without targets{{ NORMAL }}'
        exit 1
    fi
    missing=''
    for target in "$@"; do
        if [ -d "$sysroot/lib/rustlib/$target" ]; then
            echo "  ✓ $target"
        else
            missing="$missing $target"
        fi
    done
    if [ -z "$missing" ]; then
        exit 0
    fi
    echo "  {{ style("error") }}✗ missing target(s):$missing{{ NORMAL }}"
    if [ -n "${IN_NIX_SHELL:-}" ] || [ -n "${DEVENV_ROOT:-}" ]; then
        echo '    Nix/devenv owns this toolchain: add those targets to languages.rust.targets in nix/rust.nix, then re-enter the shell (`exit` and `devenv shell`); rustup cannot add targets to a store sysroot.'
        exit 1
    fi
    if ! command -v rustup >/dev/null 2>&1; then
        echo '    no rustup on PATH to install them — install Rust: https://rustup.rs, or use devenv: languages.rust.targets'
        exit 1
    fi
    toolchain="${RUSTUP_TOOLCHAIN:-$(rustup show active-toolchain 2>/dev/null | awk '{print $1}')}"
    if [ -z "$toolchain" ]; then
        echo '    rustup has no active toolchain — install one: rustup default stable'
        exit 1
    fi
    echo "    installing with rustup (toolchain: $toolchain)"
    rustup target add --toolchain "$toolchain" $missing

# (internal) check java version is between 17 and 24
[no-cd]
[script('bash')]
_check-java-version:
    if ! command -v java >/dev/null 2>&1; then exit 0; fi
    ver=$(java -version 2>&1 | head -1 | sed -E 's/.*"([0-9]+).*/\1/')
    if [ "$ver" -ge 17 ] && [ "$ver" -le 24 ] 2>/dev/null; then
        echo "  ✓ java version $ver (17–24)"
    else
        echo "  {{ style("error") }}✗ java version $ver — need 17–24 (Gradle does not support 25+){{ NORMAL }}"
    fi

# (internal) check Binaryen's wasm-opt is new enough
[no-cd]
[script('bash')]
_check-wasm-opt-version:
    set -euo pipefail
    if ! command -v wasm-opt >/dev/null 2>&1; then exit 0; fi
    want=$(just --justfile '{{ source_file() }}' _version tools.binaryen)
    ver=$(wasm-opt --version 2>/dev/null | head -n 1 | sed -E 's/.*version ([0-9]+).*/\1/')
    if [ "$ver" -ge "$want" ] 2>/dev/null; then
        echo "  ✓ wasm-opt version $ver (>= $want)"
    else
        echo "  {{ style("error") }}✗ wasm-opt version $ver — need $want+ (--enable-bulk-memory-opt was added in Binaryen $want){{ NORMAL }}"
    fi

# Check tools for a Rust crate (library, service, CLI, Rust shell)
[no-cd]
check-rust:
    @just lib::check-tool cargo "https://rustup.rs"
    @just lib::check-tool cargo-nextest "cargo install cargo-nextest"

# Check tools for a Crux core (adds the FFI toolchain)
[no-cd]
check-crux-core:
    @just lib::check-rust
    @just lib::check-boltffi

# Check tools for Apple platforms
[no-cd]
check-apple:
    @just lib::check-tool cargo "https://rustup.rs"
    @just lib::check-boltffi
    @just lib::check-tool xcodegen "brew install xcodegen"
    @just lib::check-tool xcodebuild "install Xcode from the App Store"
    @just lib::check-tool xcrun "xcode-select --install"
    @just lib::check-tool swiftlint "brew install swiftlint"
    @just lib::check-rust-target aarch64-apple-ios
    @just lib::check-rust-target aarch64-apple-ios-sim
    @just lib::check-rust-target x86_64-apple-ios

# Check tools for Android platforms
[no-cd]
check-android:
    @just lib::check-tool cargo "https://rustup.rs"
    @just lib::check-boltffi
    @just lib::check-rust-toolchain
    @just lib::check-tool java "install JDK 17+"
    @just lib::_check-java-version
    @just lib::check-tool python3 "install Python 3"
    @just lib::check-tool sdkmanager "install Android SDK command-line tools"
    @just lib::check-rust-target aarch64-linux-android
    @just lib::check-rust-target x86_64-linux-android
    @just lib::check-rust-target armv7-linux-androideabi
    @just lib::check-rust-target i686-linux-android

# Check tools for web shells using trunk (leptos, yew)
[no-cd]
check-web-trunk:
    @just lib::check-tool cargo "https://rustup.rs"
    @just lib::check-tool trunk "cargo install trunk"
    @just lib::_check-wasm-opt-version
    @just lib::check-rust-target wasm32-unknown-unknown

# Open the current directory in Android Studio
[no-cd]
[unix]
open-android-studio:
    echo '{{ style("command") }}open:{{ NORMAL }}'
    open -b com.google.android.studio .

[no-cd]
[windows]
open-android-studio:
    @echo '{{ style("command") }}open:{{ NORMAL }}'
    studio64 .
