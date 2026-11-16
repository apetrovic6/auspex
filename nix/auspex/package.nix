{ craneLib, hostArgs, src, discovery, pkgs, ... }:
craneLib.buildPackage (hostArgs // {
  pname = "auspex";
  version =
    let declared = (builtins.fromTOML (builtins.readFile (src + "/${discovery.byName.auspex-tui.dir}/Cargo.toml"))).package.version;
    in if builtins.isAttrs declared && (declared.workspace or false)
      then (builtins.fromTOML (builtins.readFile (src + "/Cargo.toml"))).workspace.package.version
      else declared;
  # Reuse the toolchain, source/vendor setup and native inputs, not workspace dev artifacts.
  CARGO_PROFILE = "release";
  cargoExtraArgs = "--locked -p auspex-tui --bin auspex-tui";
  doCheck = false;
  # auspex-effects runs git through util-linux `prlimit`, which sets git's memory limit before
  # exec. The absolute path is compiled in (`option_env!`), so the installed program needs no
  # `prlimit` on PATH, and no wrapper changes the PATH of anything it starts.
  AUSPEX_PRLIMIT = "${pkgs.util-linux}/bin/prlimit";
  # The same route for nix-eval-jobs, which a typical system does not have. The shell uses the
  # one on PATH when there is one (the system's own wins, matching its nix), and this store
  # path otherwise. Not a wrapper: a wrapper's PATH prefix would put a whole package ahead of
  # the operator's PATH for ssh, nix and git too, and the package must stay exactly
  # `bin/auspex` (the `auspex-package` check).
  AUSPEX_NIX_EVAL_JOBS = "${pkgs.nix-eval-jobs}/bin/nix-eval-jobs";
  installPhaseCommand = ''
    install -Dm755 target/release/auspex-tui "$out/bin/auspex"
  '';
  meta.mainProgram = "auspex";
})
