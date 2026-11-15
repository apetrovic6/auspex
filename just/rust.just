# Shared Rust recipes, so that every crate's Justfile is a thin, four-line delegation
# instead of a copy of the same cargo invocations.
# Import with: mod rust '<relative-path-to-repo-root>/just/rust.just'
#
# Every recipe selects its crate with `-p`, so none of them care about the working
# directory — only about being somewhere inside the workspace.
#
# Lint flags deliberately do NOT appear here: pedantic/nursery live in
# [workspace.lints.clippy] and each crate opts in with `[lints] workspace = true`, so
# cargo, rust-analyzer and CI all see the same lint set. `-D warnings` just promotes them.

[no-cd]
fmt:
    cargo fmt --all

[no-cd]
fmt-check:
    cargo fmt --all -- --check

[no-cd]
clippy pkg *args:
    cargo clippy -p {{ pkg }} {{ args }} -- --no-deps -D warnings

[no-cd]
build pkg *args:
    cargo build -p {{ pkg }} {{ args }}

# --no-tests=warn: nextest exits 4 ("no tests to run") by default, which would fail CI for
# any crate that legitimately has no tests yet — a UI shell, a freshly scaffolded crate.
# `warn` keeps that visible without turning it into a build break.
[no-cd]
test pkg *args:
    cargo nextest run --no-tests=warn -p {{ pkg }} {{ args }}

[no-cd]
run pkg *args:
    cargo run -p {{ pkg }} {{ args }}
