{ pkgs, lib, system }:
# The template defaults to a Linux GPUI closure even without a desktop shell.
# Keep Linux's existing layers unchanged; Darwin's TUI needs no Wayland/X11 inputs.
# Decide the argument shape without forcing pkgs: Intel Darwin output names must stay lazy.
lib.optionalAttrs (lib.hasSuffix "-darwin" system) {
  nativeBuildInputs = with pkgs; [ pkg-config openssl zstd ];
}
