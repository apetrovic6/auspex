# The single Rust toolchain, computed once and shared by the dev shell and CI.
#
# Both sides import this file (a values file, like nix/versions.nix) so they get the SAME
# derivation by construction — the dev shell via `languages.rust.toolchainPackage`, the CI
# pipeline via its crane override. Local clippy and CI clippy therefore cannot diverge.
#
# The toolchain is built from versions.toml (`[rust].channel`) and the target list the
# shared discovery derives from the tree — a web shell implies wasm32, an Android project
# the four Android triples, an Apple project the three Apple triples. No module contributes
# targets by hand any more.
#
# `hostOnly` drops the cross targets: the toolchain the bottom dependency layer is built with
# (nix/ci.nix, `depsToolchain`). That layer compiles for the host and nothing else, and the
# target list is the one input that differs between a template with a web shell and Android
# and a product without them — so building the layer with the full list made it a different
# derivation in every product whose app set differed, over the same snapshot (RT-76).
{ pkgs, lib, rust-overlay, hostOnly ? false }:
let
  versions = import ./versions.nix;
  discovery = import ./pipeline/discovery.nix { inherit lib; } ../.;
  mkToolchain = import ./pipeline/toolchain.nix lib;
in
mkToolchain {
  inherit pkgs rust-overlay;
  channel = versions.rust.channel;
  version = "latest";
  targets = if hostOnly then [ ] else discovery.rustTargets;
}
