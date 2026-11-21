{ pkgs, lib, system }:
# The template defaults to a Linux GPUI closure even without a desktop shell.
# Keep Linux's existing layers unchanged; Darwin's TUI needs no Wayland/X11 inputs.
# Decide the argument shape without forcing pkgs: Intel Darwin output names must stay lazy.
let darwin = lib.hasSuffix "-darwin" system; in
lib.optionalAttrs darwin {
  nativeBuildInputs = with pkgs; [ pkg-config openssl zstd ];
}
# auspex-effects' tests run git through util-linux `prlimit` (Linux only: the launcher is
# `cfg(target_os = "linux")`), so the test stage needs it. A product argument, not an edit to
# the template's stage definition (`testInputs` in nix/pipeline/pipeline.nix).
// lib.optionalAttrs (!darwin) {
  testInputs = [ pkgs.util-linux ];
}
