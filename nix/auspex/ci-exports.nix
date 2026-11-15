inputs:
let
  # Evaluate the public flake outputs without an impure getFlake call.
  exports = (import ../../flake.nix).outputs (inputs // { self = exports; });
in exports
