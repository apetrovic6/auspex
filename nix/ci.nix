# The CI pipeline, consumed from the pipeline library.
#
# This module is now a thin wrapper: `mkPipeline` (nix/pipeline/pipeline.nix) owns the
# crane layering and the stage definitions, and this file supplies the template's own
# source, toolchain and the one template-only stage (`ci-cli-check`, which is an extraStage
# here, not a built-in library stage, per amendment B).
#
# The library returns plain derivations; devenv is only one consumer (`outputs` below).
{
  pkgs,
  lib,
  config,
  inputs,
  ...
}: let
  # The single shared toolchain — the same derivation the dev shell uses (nix/rust.nix).
  toolchain = import ./toolchain.nix {
    inherit pkgs lib;
    rust-overlay = inputs.rust-overlay;
  };

  craneLib = (inputs.crane.mkLib pkgs).overrideToolchain (_: toolchain);

  mkPipeline = import ./pipeline/pipeline.nix { inherit lib; };

  result = mkPipeline ({
    src = ../.;
    inherit pkgs craneLib;
    inherit inputs;
    depsBase = ../nix/deps-base;
    # Cross targets depend on the product's shells; L0 builds only for the host.
    depsToolchain = import ./toolchain.nix {
      inherit pkgs lib;
      rust-overlay = inputs.rust-overlay;
      hostOnly = true;
    };
    pname = "rust-app-template";
    extraStages = import ./auspex/stages.nix { exports = import ./auspex/ci-exports.nix inputs; };
  } // import ./auspex/platform-args.nix { inherit pkgs lib; system = pkgs.stdenv.hostPlatform.system; });

  outputs = result.packages;
in {
  inherit outputs;

  tasks."ci:run" = {
    description = "Run the full CI pipeline locally";
    exec = ''
      set -euo pipefail
      cd "''${DEVENV_ROOT:-.}"
      echo '== derivation stages'
      devenv build outputs.ci-pure outputs.ci-android-generated
      echo '== proto breaking'
      just proto::breaking
      echo '== deny'
      just deny
      echo '== android generated tree'
      gen=$(devenv build outputs.ci-android-generated \
        | ${lib.getExe pkgs.jq} -r '."outputs.ci-android-generated"')
      for project in apps/*/platforms/android; do
        [ -d "$project" ] || continue
        app="''${project#apps/}"; app="''${app%%/*}"
        if [ -d "$gen/$project/generated" ]; then
          mkdir -p "$project/generated"
          cp -r --no-preserve=mode,ownership "$gen/$project/generated/." "$project/generated/"
        else
          just "apps/$app/generate-android"
        fi
      done
      echo '== android lint'
      for project in apps/*/platforms/android; do
        [ -d "$project" ] || continue
        app="''${project#apps/}"; app="''${app%%/*}"
        just "apps/$app/platforms/android/check"
      done
      echo '== android build'
      for project in apps/*/platforms/android; do
        [ -d "$project" ] || continue
        app="''${project#apps/}"; app="''${app%%/*}"
        just "apps/$app/platforms/android/build"
      done
      echo '== android test'
      for project in apps/*/platforms/android; do
        [ -d "$project" ] || continue
        app="''${project#apps/}"; app="''${app%%/*}"
        just "apps/$app/platforms/android/test"
      done
    '';
  };
}
