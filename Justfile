mod lib 'just/lib.just'
mod deps 'just/deps.just'
mod lock 'just/lock.just'
mod proto 'just/proto.just'
mod i18n 'just/i18n.just'
mod rust 'just/rust.just'
mod publish 'just/publish.just'
mod cross 'just/cross.just'

default: dev

# Modules are DISCOVERED, never listed: any apps/*/Justfile or services/*/Justfile is a
# module. Adding an app or a service therefore requires no edit to this file — the same
# property the Cargo globs give the workspace.
#
# crates/* and tools/* deliberately have no Justfile: they are plain library crates fully
# covered by the workspace-wide cargo recipes below.

# list the modules that the aggregate recipes will visit
[script('bash')]
modules:
    shopt -s nullglob
    for f in apps/*/Justfile services/*/Justfile; do dirname "$f"; done

# (internal) run RECIPE in every module, failing fast
[script('bash')]
_each recipe:
    set -euo pipefail
    for m in $(just modules); do
        echo '{{ style("command") }}'"$m/{{ recipe }}:"'{{ NORMAL }}'
        just "$m/{{ recipe }}"
        echo ''
    done

# --------------------------------------------------------------------------
# Whole-repo workflows (what CI runs)
# --------------------------------------------------------------------------

# check required tools across all modules
doctor: (_each "doctor")

# auto-fix formatting everywhere
fix: (_each "fix")

# validate formatting and lint everywhere
#
# Two passes on purpose. The workspace pass covers crates/, services/ and tools/, which have
# no Justfile of their own; the module pass runs each module's own `check`, which is where
# wasm32 clippy joins in. Without the first, library crates would never be linted or tested
# by CI at all.
#
# What the old comment here claimed — that this recipe reaches Gradle lint and swiftlint — it
# does not. The module pass runs `just <module>/check`, and a module's `check` walks
# `rust_modules`, which lists crates and not platform projects. Getting that wrong cost three
# lint gates their only runner, so the answer is now written down twice: here, and in
# `gates.toml`, where a recipe claiming to run somewhere the graph cannot reach fails
# `cargo xtask check`.
#
# Neither pass reaches a platform project, so this is not where Gradle lint or swiftlint run.
check: rust-check (_each "check")

# build everything
build: (_each "build")

# test everything (workspace crates, then each module's own tests)
test: rust-test (_each "test")

# local development workflow
dev: fmt check build test

# CI workflow — Rust side only; native platform builds get their own jobs
#
# `typos` is here and `deny` is not, and the asymmetry is the point. Both run in the
# supply-chain job, and for a while neither ran here — so a green `just ci` did not predict a
# green pipeline, and a misspelling in a doc comment reached CI twice before anyone wired this
# up. `typos` is fast, offline, and answers a question about the commit, so it belongs in the
# gate people run. `deny` answers a question about today's advisory database: it needs the
# network, and it can start failing without anything here changing, which is a reason to
# report it on a schedule rather than to block the change under review (see the supply-chain
# job's own comment). `gates.toml` records that split.
ci: check-repo check build test typos cross-check

# --------------------------------------------------------------------------
# Repository health
# --------------------------------------------------------------------------

# scaffold a new crate, e.g. `just add lib-crate billing` or `just add cli-app mytool`
# `*flags` passes through to the CLI, e.g. `just add platform-android myapp --from-template`.
add archetype name *flags:
    nix run "git+https://forge.ugalabugala.org/manjo/rust-app-template?rev=$(nix eval --raw --impure -f nix/template-pin.nix)#add" -- {{ archetype }} {{ name }} {{ flags }}

# grow a Crux app — the client crate its shells share, or one more shell
add-client app:
    nix run "git+https://forge.ugalabugala.org/manjo/rust-app-template?rev=$(nix eval --raw --impure -f nix/template-pin.nix)#add-client" -- {{ app }}

add-shell app shell:
    nix run "git+https://forge.ugalabugala.org/manjo/rust-app-template?rev=$(nix eval --raw --impure -f nix/template-pin.nix)#add-shell" -- {{ app }} {{ shell }}


# run the whole pipeline locally, exactly as a push does — `just pipeline`
# One definition rather than a second spelling of the stages: this runs the same devenv task
# the workflows run, so a green `just pipeline` and a green push mean the same thing. It builds
# every derivation stage in one `devenv build` and then runs the host steps — the ones that need
# `.git` history, today's advisory database, or Gradle — in workflow order.
#
# `--show-output` is not cosmetic. `devenv tasks run` swallows a PASSING task's output, so
# without it this recipe prints the derivation builds and nothing else: no stage markers, no
# Gradle summary, no advisory verdict. A run you cannot read is a run you cannot trust — and
# one that fails halfway is worse, because the swallowed half is the half that says why.
#
# A single stage is `devenv build outputs.ci-<stage>`; `just --list` does not show those, so
# `nix/ci.nix` is the list. Takes no job argument, because the stages are not jobs: they are
# derivations and steps, and the split that used to make `pipeline rust` meaningful (which
# container a recipe ran in) is gone.
pipeline:
    devenv tasks run --show-output ci:run

# verify layout invariants, version single-sourcing, the template's own checks, the
# generated wire contract and the generated CLDR slice
check-repo:
    cargo xtask check
    @just proto::check
    @just i18n::check-data

# print one value from versions.toml, e.g. `just version tools.boltffi`
version key:
    @cargo xtask version {{ key }}

# supply-chain policy: advisories, licences, banned and unexpected sources
#
# `cargo xtask deny` rather than `cargo deny check` on its own, because the failure does not say
# which of its two answers it wants — a `deny.toml` `ignore` entry or a `cargo update` — and on
# failure this adds the triage that does: findings counted by kind, and for each whether a fix is
# named. cargo-deny's own output and exit status pass through unchanged, the triage is printed
# after them, and a green run prints nothing it did not print before. CI reaches the same line
# through this recipe — `ci-impure` runs `just deny` on every push — so nothing carries a second
# copy of the triage.
#
# `*args` because the fix is this gate with one more flag, and the nightly reaches *that*
# through this recipe too (`just deny --fix`, in `advisories.yaml`). The passthrough is what keeps the claim
# above true a second time: `just deny --fix` has to arrive as `cargo xtask deny --fix` rather
# than as a flag just eats, and a separate recipe written to carry `--fix` would be a second
# command line for one gate. Measured against the just in this shell: `just deny --fix` passes
# `--fix` through, `just deny` passes nothing, and `gates.rs` still reads both as the `deny`
# recipe because `invocations` takes the first word after `just` and ignores the rest.
deny *args:
    cargo xtask deny {{ args }}

# spell-check the tree
typos:
    typos

# --------------------------------------------------------------------------
# Fast workspace-wide paths (for the inner development loop)
# --------------------------------------------------------------------------
# These use cargo's own workspace handling instead of walking modules, so they run once
# over the whole dependency graph rather than once per crate. They cover `default-members`
# — the heavy shells (gpui, wasm) are excluded; reach those with `just apps/<app>/check`.

# format the whole workspace
fmt:
    @just rust::fmt

# clippy the whole workspace in one pass
rust-check:
    @just rust::fmt-check
    cargo clippy --all-targets -- --no-deps -D warnings

# test the whole workspace in one pass
rust-test:
    cargo nextest run

# --------------------------------------------------------------------------

# clean build artefacts everywhere
clean: (_each "clean")
    cargo clean
    rm -rf target

# type-check the shipped code for the flake's other systems (aarch64-linux, both darwins)
#
# A recipe of its own rather than a dependency written `cross::check` in `ci`, because the gate
# graph (`cargo xtask check`) follows a module's recipe only through an invocation in a recipe
# body, as `check-repo` does for `proto::check`. What it covers and does not is in just/cross.just.
cross-check:
    @just cross::check
