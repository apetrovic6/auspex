# Repository-wide command line tools: what `just check`, the scaffolder and the release
# recipes invoke. Anything a single concern needs belongs in that concern's module instead.
{pkgs, ...}: {
  packages = with pkgs; [
    git
    just
    python3

    # `cargo xtask advisory-pr` shells out to curl for its two forge calls. Pinned here rather
    # than assumed of the host: a runner's PATH carries nix, devenv, git, bash, coreutils,
    # cargo and cacert, and nothing else — so a command that reaches for curl without this line
    # works on a laptop and fails on the runner. See the module documentation in
    # tools/xtask/src/advisory_pr.rs for why curl and not an HTTP client.
    curl

    cargo-nextest
    cargo-seek
    cargo-deny
    typos
    taplo

    # `just publish::push` copies a built archive to a registry, and `publish::validate` runs
    # the same inspection the CI derivation does. On the host it has to come from the shell:
    # the runner's PATH carries nix, devenv, git, bash and coreutils, and nothing else.
    skopeo
    jq

    # auspex evaluates each machine's toplevel with nix-eval-jobs (one evaluator per machine,
    # so one failure cannot abort the rest), and `crates/auspex-effects/tests/nix_fixture.rs`
    # runs it against a fixture flake. Pinned here so that test runs in the dev shell rather
    # than skipping for want of the binary. Not in the `ci-pure` sandbox: there is no Nix
    # daemon to evaluate against there, so the test skips itself.
    nix-eval-jobs
  ];
}
