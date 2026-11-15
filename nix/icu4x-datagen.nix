# The ICU4X data generator, as dev-shell content: what `just i18n::generate-data` and
# `just i18n::check-data` invoke by name.
#
# The derivation lives in nix/icu4x-datagen-tool.nix (a values file, imported rather than a
# module) because nix/ci.nix runs the same tool inside a sandboxed derivation and needs it as
# a build input.
{pkgs, ...}: {
  packages = [(import ./icu4x-datagen-tool.nix {inherit pkgs;}).datagen];
}
