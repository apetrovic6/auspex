# The pinned template revision — the origin record a product carries.
#
# The record is the product's `template` input in `flake.lock` (decision 1: the flake is
# canonical, and devenv follows it — see `just lock::sync`). The input's URL carries the
# origin revision and Nix writes the node with the right narHash/type, so there is no second
# origin file and nothing to keep in step by hand.
#
# This is a product's file. The template repository itself has no `template` input — it is
# not instantiated from anything — so evaluating this in the template throws, which is what
# `throw` here is for: a product that lost the node is told so, rather than resolving to a
# revision named by some unrelated input.
let
  lock = builtins.fromJSON (builtins.readFile ../flake.lock);
  root = lock.nodes.${lock.root} or { };
  inputs = root.inputs or { };
  node = lock.nodes.${inputs.template or "template"} or { };
in
node.locked.rev
or (throw "flake.lock has no `template` input — this tree is not a product")
