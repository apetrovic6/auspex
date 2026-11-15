# The desktop shell's system libraries.
#
# GPUI links these at build time and dlopens several of them at run time, and NixOS has no
# global library search path, so the same list has to be both on `packages` and on
# LD_LIBRARY_PATH. It mirrors upstream's script/install-linux.sh from
# github.com/longbridge/gpui-kit.
{
  pkgs,
  lib,
  ...
}: let
  libraries = with pkgs; [
    openssl
    zstd
    fontconfig
    freetype
    libxkbcommon
    libglvnd
    vulkan-loader
    wayland
    libX11
    libXcursor
    libXi
    libxcb
    libxkbfile
  ];
in {
  packages = libraries ++ [pkgs.pkg-config];

  env.LD_LIBRARY_PATH = lib.makeLibraryPath libraries;
}
