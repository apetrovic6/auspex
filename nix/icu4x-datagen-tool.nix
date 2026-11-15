# The ICU4X data generator, as a derivation rather than as dev-shell content.
#
# Deliberately *not* a devenv module — a values file, like nix/versions.nix and
# nix/proto-tools.nix — because two consumers need the same tool: the dev shell
# (nix/icu4x-datagen.nix puts it on PATH for `just i18n::generate-data`) and the CI
# pipeline (nix/ci.nix runs it inside a sandbox to prove the committed payload is
# current). Import with `import ./icu4x-datagen-tool.nix {inherit pkgs;}`.
{pkgs}: let
  versions = import ./versions.nix;

  # icu4x-datagen (https://github.com/unicode-org/icu4x) is not packaged in nixpkgs at the
  # pinned revision (checked: no pkgs/by-name/ic/icu4x-datagen), so build it locally from
  # the crates.io release, the way nix/boltffi.nix builds BoltFFI.
  icu4x-datagen = pkgs.rustPlatform.buildRustPackage rec {
    pname = "icu4x-datagen";
    # NOTE: bumping this in versions.toml also requires refreshing the two hashes below;
    # Nix will tell you the expected values on the first failed build. Bumping it alone is
    # wrong anyway — `cargo xtask check-versions` refuses a datagen whose major.minor
    # disagrees with the `icu_*` crates that read the payload it writes.
    version = versions.tools.icu4x-datagen;

    src = pkgs.fetchCrate {
      crateName = "icu4x-datagen";
      inherit version;
      hash = "sha256-Kr7fQ1pWyXlanzC3LOWzt58YPDGwSOjIuKQQPy5mhrQ=";
    };

    cargoHash = "sha256-j7EMJAvU0pGoSryVhRk2lpBeZ9m5JCKyK+4BksnRSmA=";

    # The crate's own tests download CLDR and compare generated output against fixtures,
    # which needs both network and a cache Nix does not give a build. The repository's
    # interest is not in datagen's correctness but in the payload it already wrote, and
    # `just i18n::check-data` is the check for that.
    doCheck = false;

    postInstall = ''
      test -x $out/bin/icu4x-datagen
    '';

    meta = with pkgs.lib; {
      description = "Generate ICU4X data libraries for use as data providers";
      homepage = "https://github.com/unicode-org/icu4x";
      license = with licenses; [licenses.asl20];
      mainProgram = "icu4x-datagen";
    };
  };
in {
  datagen = icu4x-datagen;
}
