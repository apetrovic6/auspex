# The BoltFFI CLI, which turns the Crux core's exported types into Kotlin and Swift
# bindings (`just <app>/typegen`).
{pkgs, ...}: {
  packages = [(import ./boltffi-tool.nix {inherit pkgs;}).cli];
}
