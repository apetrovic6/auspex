# Keeping the two lock files in agreement. `flake.lock` is canonical; `devenv.lock` follows.
#
# This is the one command in the pair. A template and a product both carry two locks — the
# flake's (`flake.lock`, what `nix build`, `nix run` and a product's flake outputs read) and
# devenv's (`devenv.lock`, what the dev shell and CI build with) — and they resolve
# independently. The inputs both of them consume (nixpkgs, crane, rust-overlay, connectrpc)
# therefore drift silently, and the drift has teeth: rust-overlay at two revisions is two
# toolchains, so the dev shell's clippy and the flake's crane build use different compilers
# while both are called "the pinned toolchain"; connectrpc at two revisions is two sets of buf
# plugins, so the committed generated code gets checked against one and regenerated with the
# other.
#
# `flake.lock` is the canonical one (decision 1): a product carries the flake, `cli new` pins
# the template input there, and `nix/template-pin.nix` reads the origin from it. So the sync is
# one direction — write `flake.lock`'s revisions into `devenv.yaml`'s URLs and let devenv
# re-resolve its own lock.
#
# `cargo xtask check` asserts the result (`tools/xtask/src/locks.rs`), naming both revisions
# when they disagree. Nothing here is a gate: like `just deps::rebase-deps`, running it
# rewrites a committed input and is followed by a diff and a commit.
#
# Import with: mod lock '<relative-path-to-repo-root>/just/lock.just'

# The repository root. `source_directory()` is this file's directory even when the module is
# imported from the root Justfile, which is what makes it the root from here.
root := source_directory() / ".."

# The inputs both locks carry. Spelled here rather than only in xtask so a reader of this file
# sees the list; `tools/xtask/src/locks.rs` is the authority, and its check fails if the two
# ever disagree about which inputs matter.
shared := "nixpkgs crane rust-overlay connectrpc"

# Point devenv.yaml's URLs at the revisions flake.lock holds, then re-lock just those inputs.
#
# Pinning the URL — rather than only updating the lock — is what makes the agreement hold the
# next time somebody runs `devenv update`: an unpinned input would resolve to whatever the
# branch says today, which is exactly how the two locks drifted apart in the first place.
#
# `devenv update <name>` one input at a time, never bare `devenv update`: devenv's own inputs
# (devenv, git-hooks, android-nixpkgs, rustfs) are not part of this agreement, and moving them
# here would ride an unrelated upgrade along with a lock fix.
[script('bash')]
sync:
    set -euo pipefail
    cd '{{ root }}'

    python3 - <<'PY'
    import json, re, sys

    lock = json.load(open("flake.lock"))
    nodes = lock["nodes"]
    root = nodes[lock["root"]]

    shared = "{{ shared }}".split()
    text = open("devenv.yaml").read()
    for name in shared:
        node = nodes[root["inputs"][name]]["locked"]
        # devenv spells a pinned github input `github:owner/repo/rev` (the rustfs input already
        # does), which is the same reference Nix records in a lock.
        url = f"github:{node['owner']}/{node['repo']}/{node['rev']}"
        # Two-space input, four-space `url:`. Literal spaces rather than a counted regex: just
        # interpolates anything inside double braces, so a `{2}` quantifier would arrive here
        # as a `2` and silently rewrite nothing.
        pattern = re.compile(rf"^(  {re.escape(name)}:\n    url: ).*$", re.M)
        text, replaced = pattern.subn(rf"\g<1>{url}", text)
        if replaced != 1:
            sys.exit(
                f"devenv.yaml: expected one `{name}:` input with a `url:`, found {replaced}"
            )
        print(f"  {name} -> {node['rev'][:12]}")
    open("devenv.yaml", "w").write(text)
    PY

    for name in {{ shared }}; do
        devenv update "$name"
    done

    echo "devenv.yaml follows flake.lock. Review the diff to devenv.yaml and devenv.lock,"
    echo "then run 'cargo xtask check' and commit both."
