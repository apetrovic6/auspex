# The wire contract: linting, code generation and compatibility.
# Import with: mod proto '<relative-path-to-repo-root>/just/proto.just'
#
# Generated output is committed (see proto/buf.gen.yaml for why), so `check` is what keeps
# the committed copy honest: it regenerates and fails if anything differs.

repo := source_directory() / ".."

default: lint

# lint the .proto files
[no-cd]
lint:
    @echo '{{ style("command") }}proto lint:{{ NORMAL }}'
    buf lint '{{ repo }}/proto'

# format the .proto files in place
[no-cd]
fix:
    @echo '{{ style("command") }}proto fix:{{ NORMAL }}'
    buf format -w '{{ repo }}/proto'

# regenerate Rust messages and Connect stubs from the .proto files
[no-cd]
[script('bash')]
generate:
    set -euo pipefail
    echo '{{ style("command") }}proto generate:{{ NORMAL }}'
    cd '{{ repo }}'
    buf generate proto --template proto/buf.gen.yaml

# fail if the committed generated code has drifted from the .proto files
#
# Compares a snapshot of the current output against a fresh generation, rather than asking
# git. `git diff` reports nothing for untracked files, so a git-based check silently
# passes before the generated tree is first committed; it would also conflate "not yet
# committed" with "out of date", which are different problems.
[no-cd]
[script('bash')]
check:
    set -euo pipefail
    echo '{{ style("command") }}proto check:{{ NORMAL }}'
    cd '{{ repo }}'
    buf lint proto
    buf format --diff --exit-code proto
    snapshot=$(mktemp -d)
    trap 'rm -rf "$snapshot"' EXIT
    # Every generated tree, not just the Rust one. The shell languages are the ones most
    # likely to rot: nothing in `cargo build` or `cargo test` touches them, so a .proto
    # change that nobody regenerates for Kotlin or Swift would otherwise go unnoticed until
    # an Android or Apple build.
    cp -r crates/proto/src/gen "$snapshot/rust"
    cp -r proto/gen "$snapshot/shells"
    buf generate proto --template proto/buf.gen.yaml
    if ! diff -r "$snapshot/rust" crates/proto/src/gen >/dev/null 2>&1 \
        || ! diff -r "$snapshot/shells" proto/gen >/dev/null 2>&1; then
        echo '{{ style("error") }}✗ generated code does not match the .proto files{{ NORMAL }}' >&2
        diff -rq "$snapshot/rust" crates/proto/src/gen >&2 || true
        diff -rq "$snapshot/shells" proto/gen >&2 || true
        echo '  run `just proto::generate` and commit the result' >&2
        exit 1
    fi
    echo '  ✓ generated code matches the .proto files'

# check for wire-incompatible changes against a base ref (PR-time check)
[no-cd]
[script('bash')]
breaking base="master":
    set -euo pipefail
    echo '{{ style("command") }}proto breaking (vs {{ base }}):{{ NORMAL }}'
    cd '{{ repo }}'
    if ! git rev-parse --verify --quiet '{{ base }}' >/dev/null; then
        # Not a failure: a fresh clone, a repository with no remote, or a shallow checkout
        # legitimately has no base to compare against. It IS a gap, though — this recipe is
        # the only thing standing between a careless edit and a broken wire contract — so it
        # says so rather than printing something that reads like a pass.
        echo "  {{ style("warning") }}! wire compatibility NOT checked{{ NORMAL }}: no ref '{{ base }}' in this clone" >&2
        echo "    CI needs the base branch fetched (checkout with fetch-depth: 0), or pass one:" >&2
        echo "    just proto::breaking origin/master" >&2
        exit 0
    fi
    # The ref can exist and still have no contract — a base older than `proto/` itself,
    # which is every base until the first release of it lands. buf reports that as
    # "had no .proto files" and exits non-zero, which reads as a compatibility failure
    # rather than as an absent baseline.
    if ! git cat-file -e '{{ base }}:proto' 2>/dev/null; then
        echo "  {{ style("warning") }}! wire compatibility NOT checked{{ NORMAL }}: '{{ base }}' has no proto/ yet" >&2
        echo "    nothing to compare against until a commit with the contract is on that ref" >&2
        exit 0
    fi
    buf breaking proto --against ".git#ref={{ base }},subdir=proto"
