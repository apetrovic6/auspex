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
  ];
}
