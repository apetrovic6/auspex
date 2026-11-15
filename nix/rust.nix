# The Rust toolchain itself, as a single shared derivation.
#
# The toolchain is computed once in nix/toolchain.nix and used verbatim by the dev shell
# and by CI, so local clippy and CI clippy see the same compiler (amendment A). Cross
# targets are derived from the tree by nix/pipeline/discovery.nix — a web shell implies
# wasm32, an Android project the four Android triples — so no module contributes a target
# list by hand and deleting a platform is deleting that platform's files, not auditing
# `languages.rust.targets` in several modules.
{
  pkgs,
  lib,
  inputs,
  ...
}: {
  # https://devenv.sh/languages/rust/
  languages.rust = {
    enable = true;
    # The aggregated toolchain (channel + version + targets), the same derivation the CI
    # pipeline passes to crane. Set explicitly rather than via `channel`/`version`, so the
    # two consumers cannot drift.
    toolchainPackage = import ./toolchain.nix {
      inherit pkgs lib;
      rust-overlay = inputs.rust-overlay;
    };
  };
}
