# The entity generator, which writes `services/<name>/src/entities/*.rs`.
#
# It has to be the release that matches the `sea-orm` crate those files compile against:
# the entity is a `DeriveEntityModel` invocation using the runtime's derive macros, so a CLI
# from a different major writes a file nothing in the workspace can compile. `cargo xtask
# check-versions` refuses a skew of major.minor; see `versions.toml` for why the pin is 1.1.20
# rather than the 2.x line.
#
# It is built from the crates.io release rather than taken from nixpkgs-unstable, because
# nixpkgs-unstable is unpinned by design — `devenv update` moves it, and nothing connects that
# move to entities. That is exactly how the CLI reached 2.0.3 against a 1.1.20 runtime
# (RT-30). Nixpkgs packages no version we could select either, so `fetchCrate` and
# `buildRustPackage` are the same shape `nix/icu4x-datagen.nix` uses.
{pkgs, ...}: let
  versions = import ./versions.nix;

  sea-orm-cli = pkgs.rustPlatform.buildRustPackage rec {
    pname = "sea-orm-cli";
    # NOTE: bumping this in versions.toml also requires refreshing the two hashes below;
    # Nix will tell you the expected values on the first failed build. Bumping it alone is
    # wrong anyway — `cargo xtask check-versions` refuses a CLI whose major.minor disagrees
    # with the `sea-orm` crates that read what it writes, and the bump's instruction is to
    # re-run `entities` against a real schema and diff the fixture.
    version = versions.tools.sea-orm-cli;

    src = pkgs.fetchCrate {
      crateName = "sea-orm-cli";
      inherit version;
      hash = "sha256-n7QkCnMF15UMLPPEF093ylzzDESGKKG/q4Y2jvdTcUo=";
    };

    cargoHash = "sha256-itn1i2klZeZQIQLF/lqaqTly1QqbtUgZhqHmAzrKn38=";

    # sea-orm-cli's dependency tree reaches openssl-sys through sqlx's TLS stack, and
    # openssl-sys looks for the library with pkg-config at build time.
    nativeBuildInputs = [pkgs.pkg-config];
    buildInputs = [pkgs.openssl];

    # The crate's tests need a database, and the repository's interest is in the binary it
    # runs against a schema rather than in sea-orm-cli's own correctness.
    doCheck = false;

    postInstall = ''
      test -x $out/bin/sea-orm-cli
    '';

    meta = with pkgs.lib; {
      description = "CLI for generating SeaORM entities from a live database schema";
      homepage = "https://github.com/SeaQL/sea-orm";
      license = with licenses; [licenses.asl20 licenses.mit];
      mainProgram = "sea-orm-cli";
    };
  };
in {
  packages = [sea-orm-cli];
}