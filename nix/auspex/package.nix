{ craneLib, hostArgs, depsL1, ... }:
craneLib.buildPackage (hostArgs // {
  pname = "auspex";
  version = "0.1.0";
  cargoArtifacts = depsL1;
  cargoExtraArgs = "--locked -p auspex-tui";
  doCheck = false;
  postInstall = ''
    mv "$out/bin/auspex-tui" "$out/bin/auspex"
  '';
  meta.mainProgram = "auspex";
})
