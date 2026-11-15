# The wire contract's toolchain, as dev-shell content: buf, protoc, and the codegen
# plugins `buf generate` runs, all on PATH inside `devenv shell`.
#
# The derivations live in nix/proto-tools.nix (a values file, imported rather than a
# module) because nix/ci.nix runs the same codegen inside a sandboxed derivation and
# needs the same list as build inputs.
{
  pkgs,
  inputs,
  ...
}: {
  packages = (import ./proto-tools.nix {inherit pkgs inputs;}).packages;
}
