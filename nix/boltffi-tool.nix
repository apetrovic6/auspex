# The BoltFFI CLI, as a derivation rather than as dev-shell content.
#
# A values file, like nix/proto-tools.nix, nix/cldr.nix and nix/android-ndk.nix, because two
# consumers need the same tool: the dev shell (nix/boltffi.nix puts it on PATH for
# `just <app>/generate-android`) and the CI pipeline (nix/ci.nix runs it inside a sandbox to
# cross-compile the app's native libraries). Import with
# `import ./boltffi-tool.nix {inherit pkgs;}`.
{pkgs}: let
  versions = import ./versions.nix;

  # BoltFFI (https://github.com/boltffi/boltffi) is not packaged in nixpkgs, so build it
  # locally from the crates.io release (the GitHub repo ships no Cargo.lock, the published
  # .crate tarball does).
  boltffi-cli = pkgs.rustPlatform.buildRustPackage rec {
    pname = "boltffi-cli";
    # NOTE: bumping this in versions.toml also requires refreshing the two hashes below;
    # Nix will tell you the expected values on the first failed build.
    version = versions.tools.boltffi;

    src = pkgs.fetchCrate {
      crateName = "boltffi_cli";
      inherit version;
      hash = "sha256-FtD5ZPq0tY+c30VE8H0Qx+VpB/RbdfVUPBUWwTHfgIQ=";
    };

    cargoHash = "sha256-DdfG8Z1joV1ftyIw9UIF0GVYalxMC1gYZtxIWeF8O/E=";

    # upstream tests rely on fixtures/templates not all shipped in the crate tarball
    doCheck = false;

    # the binary is named `boltffi` (package name is boltffi_cli)
    postInstall = ''
      test -x $out/bin/boltffi
    '';

    meta = with pkgs.lib; {
      description = "CLI for BoltFFI - generate Swift, Kotlin, Java, C#, WASM/TS, and Python bindings from Rust";
      homepage = "https://github.com/boltffi/boltffi";
      license = licenses.mit;
      mainProgram = "boltffi";
    };
  };
in {
  cli = boltffi-cli;
}
