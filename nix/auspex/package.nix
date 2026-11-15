{ craneLib, hostArgs, src, discovery, ... }:
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
  installPhaseCommand = ''
    install -Dm755 target/release/auspex-tui "$out/bin/auspex"
  '';
  meta.mainProgram = "auspex";
})
