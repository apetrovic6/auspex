# The single Rust toolchain: built from versions.toml + the discovered target list.
#
# Both the dev shell and the CI pipeline pass through here, so local clippy and CI clippy
# cannot diverge (amendment A). The dev shell sets
# `languages.rust.toolchainPackage = mkToolchain { ... }`, and `mkPipeline` builds the same
# derivation for its crane toolchain — one store path for both.
#
# This replicates devenv's own nightly-toolchain composition (its `channel != "nixpkgs"`
# branch): it resolves the individual components out of rust-overlay and joins them with
# rust-overlay's private `mk-aggregated` helper, which is the same function devenv imports.
# Using the same helper is what keeps the toolchain a byte-for-byte match for the one the
# repository used before the split.
#
# `lib` is a positional (curried) argument so the file can be imported once, system-
# independently, and still have `pkgs`/`rust-overlay` supplied later by the caller:
# `mkToolchain = import ./toolchain.nix nixpkgs.lib;`. `lib` is pure helper functions;
# `pkgs` and `rust-overlay` are the caller's own pins.
lib:
{ pkgs, rust-overlay, channel, version, targets }:
let
  rustBin = rust-overlay.lib.mkRustBin { } pkgs.buildPackages;
  toolchain = (rustBin.${channel} or (throw "invalid rust channel '${channel}'")).${version}
    or (throw "invalid rust version '${version}' for channel '${channel}'");

  # The individual component derivations, minus the `rust` aggregate profile.
  toolchainComponents = builtins.removeAttrs toolchain [ "rust" ];
  availableComponents = toolchain._manifest.profiles.complete or [ ];

  # rust-overlay's private aggregation helper — the same one devenv imports.
  mkAggregatedFn = import (rust-overlay + "/lib/mk-aggregated.nix");
  mkAggregatedArgs = builtins.functionArgs mkAggregatedFn;
  mkAggregated = mkAggregatedFn ({
    inherit (pkgs) lib stdenv symlinkJoin bash curl;
    inherit (pkgs.buildPackages) rustc;
    pkgsTargetTarget = pkgs.targetPackages;
  } // lib.optionalAttrs (mkAggregatedArgs ? makeWrapper) {
    inherit (pkgs) makeWrapper;
  } // lib.optionalAttrs (mkAggregatedArgs ? pkgsHostHost) {
    inherit (pkgs) pkgsHostHost;
  });

  nativeTarget = pkgs.stdenv.hostPlatform.rust.rustcTargetSpec;
  allTargets = lib.unique ([ nativeTarget ] ++ targets);

  allComponents = toolchain._components or { };
  availableTargets = toolchain._manifest.pkg.rust-std.target or { };

  # rust-std for the native target plus every discovered cross target.
  targetComponents = map (target:
    let
      set = allComponents.${target} or { };
      rustStd = set.rust-std or null;
    in
    if !(availableTargets ? ${target}) then
      throw "rust target '${target}' is not available in the ${channel} manifest (available: ${lib.concatStringsSep ", " (builtins.attrNames availableTargets)})"
    else if rustStd == null then
      throw "rust target '${target}' has no rust-std component in the ${channel} manifest"
    else
      rustStd
  ) allTargets;

  # The components the dev shell has always carried: the compiler, the build tool, and the
  # lints/formatter/lsp. rust-src is deliberately absent — RUST_SRC_PATH falls back to
  # nixpkgs' rustPlatform.rustLibSrc, as before.
  components = [ "rustc" "cargo" "clippy" "rustfmt" "rust-analyzer" ];
  resolvedComponents = map (c:
    let
      resolvedName =
        if builtins.elem c availableComponents then c
        else if builtins.elem "${c}-preview" availableComponents then "${c}-preview"
        else throw "rust component '${c}' not found in the ${channel} manifest";
    in
    toolchainComponents.${resolvedName}
  ) components;

  selectedComponents = resolvedComponents ++ targetComponents;

  profile = mkAggregated {
    pname = "rust-${channel}-${toolchain._manifest.version}";
    inherit (toolchain._manifest) version date;
    inherit selectedComponents;
  };
in
profile
