# The development environment, as a set of modules.
#
# Each file under nix/ owns one concern: its packages, its environment variables, its shell
# hook and, where it has one, its process. devenv merges them -- `packages` is a list and
# `enterShell` is lines -- so a module contributes without anything here having to know what
# it contributes. Adding a concern is adding a file and a line; dropping one (a product with
# no Android app, no desktop shell) is deleting the same two.
#
# nix/versions.nix is deliberately absent from this list: it is a parsed versions.toml
# rather than a module, and the modules that need a pin import it directly.
{pkgs, ...}: {
  imports = [
    ./nix/tooling.nix
    ./nix/proto.nix
    ./nix/boltffi.nix
    ./nix/icu4x-datagen.nix
    ./nix/sea-orm-cli.nix
    ./nix/rust.nix
    ./nix/android.nix
    ./nix/web.nix
    ./nix/desktop.nix
    ./nix/services.nix
    ./nix/observability.nix
    ./nix/ci.nix
  ];

  # https://devenv.sh/basics/
  env.GREET = "devenv";

  # https://devenv.sh/scripts/
  scripts.hello.exec = ''
    echo hello from $GREET
  '';

  enterShell = ''
    hello         # Run scripts directly
    git --version # Use packages
  '';

  # https://devenv.sh/tests/
  enterTest = ''
    echo "Running tests"
    git --version | grep --color=auto "${pkgs.git.version}"
  '';

  # See full reference at https://devenv.sh/reference/options/
}
