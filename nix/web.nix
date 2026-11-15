# The web shell(s): a Leptos app compiled to WebAssembly and served by Trunk.
#
# The trunk processes are DISCOVERED, not listed — the same discovery the pipeline uses
# (amendment F). A crate carrying `[package.metadata.app] trunk = true` is a trunk web
# shell, and this module runs one `devenv up` process per such crate.
{
  pkgs,
  lib,
  ...
}: let
  discovery = import ./pipeline/discovery.nix { inherit lib; } ../.;
  trunkProcess = crate: {
    exec = ''
      cd "$DEVENV_ROOT/${crate.dir}" && trunk serve
    '';
  };
  # `web` when there is one web shell (the common case, and what the docs say to run);
  # `web-<app>` per app when there are several.
  webProcesses =
    if lib.length discovery.trunkCrates == 1 then
      { web = trunkProcess (lib.head discovery.trunkCrates); }
    else
      lib.listToAttrs (map (crate: {
        name = "web-" + (discovery.appOf crate);
        value = trunkProcess crate;
      }) discovery.trunkCrates);
in {
  packages = with pkgs; [
    cargo-leptos
    trunk
    tailwindcss_4
    # Trunk downloads wasm-bindgen when a binary of the right version is not on PATH —
    # the version it wants is the one Cargo.lock pins for the `wasm-bindgen` library
    # (0.2.126 today; nixpkgs' default is newer). Pinning it here means a local
    # `trunk build` uses this binary instead of fetching an unpinned one from GitHub.
    # The versioned alias must move with Cargo.lock: `cargo update -p wasm-bindgen`
    # requires changing it here (and in nix/pipeline/pipeline.nix) or trunk starts
    # downloading again.
    wasm-bindgen-cli_0_2_126
  ];

  processes = webProcesses;
}
