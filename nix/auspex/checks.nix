{ pkgs, package }:
let
  inherit (pkgs) lib;
  revisions = import ./revision.nix;
  packages.${pkgs.stdenv.hostPlatform.system}.auspex = package;
  stub = { ... }: {
    options = {
      home.packages = lib.mkOption { type = lib.types.listOf lib.types.package; default = [ ]; };
      xdg.configHome = lib.mkOption { type = lib.types.str; default = "/home/test/.config"; };
      xdg.configFile = lib.mkOption { type = lib.types.attrsOf (lib.types.submodule {
        options.source = lib.mkOption { type = lib.types.path; };
      }); default = { }; };
      assertions = lib.mkOption { type = lib.types.listOf lib.types.unspecified; default = [ ]; };
      lib = lib.mkOption { type = lib.types.attrs; default = { }; };
    };
  };
  evaluate = extra: (lib.evalModules {
    specialArgs = { inherit pkgs; };
    modules = [ stub (import ./home-manager.nix { inherit packages; })
      { programs.auspex.enable = true; } extra ];
  }).config;
  checked = cfg: if builtins.all (a: a.assertion) cfg.assertions then cfg
    else throw (lib.concatStringsSep "\n" (map (a: a.message) (builtins.filter (a: !a.assertion) cfg.assertions)));
  check = name: condition: pkgs.runCommand "auspex-${name}" { } ''
    ${if condition then "" else "echo 'auspex-${name} failed' >&2; exit 1"}
    touch $out
  '';
  fullSettings = {
    flake = "/home/me/clan/fleet";
    timeouts = { ssh_connect = 5; probe = 10; eval = 300; settle = 120; };
    eval = { workers = 4; max_memory_mb = 4096; };
    prometheus = { url = "http://prometheus.example:9090"; stale_after = 60; };
    cache.url = "https://ncps.example";
    machines.terra = { target = "root@192.168.1.10"; prometheus_instance = "terra:9100"; };
    keymap = { quit = [ "q" "esc" ]; down = "j"; };
    theme.file = "~/.config/auspex/theme.toml";
  };
  full = evaluate { programs.auspex.settings = fullSettings; };
  unknown = evaluate { programs.auspex.settings.future.answer = 42; };
  wrong = builtins.tryEval (builtins.deepSeq
    (evaluate { programs.auspex.settings.timeouts.probe = "ten"; }).programs.auspex.settings true);
  slots = map (s: "base${s}") [ "00" "01" "02" "03" "04" "05" "06" "07" "08" "09" "0A" "0B" "0C" "0D" "0E" "0F" ];
  stylix = checked (evaluate {
    lib.stylix.colors = lib.genAttrs slots (_: "123abc");
    programs.auspex.stylix.enable = true;
  });
  absent = evaluate { programs.auspex.stylix.enable = true; };
  revision = module: self: override: (lib.evalModules {
    specialArgs = { inherit self; };
    modules = [ {
      options.system.configurationRevision = lib.mkOption { type = lib.types.nullOr lib.types.str; };
    } module override ];
  }).config.system.configurationRevision;
  revisionOK = module: builtins.all (x: x) [
    (revision module { rev = "abc"; dirtyRev = "dirty"; } { } == "abc")
    (revision module { dirtyRev = "dirty"; } { } == "dirty")
    (revision module { } { } == null)
    (revision module { rev = "abc"; } { system.configurationRevision = "consumer"; } == "consumer")
  ];
  compareToml = name: source: expected: pkgs.runCommand "auspex-${name}" {
    nativeBuildInputs = [ pkgs.python3 ];
    inherit source;
    expectedJSON = builtins.toJSON expected;
    passAsFile = [ "expectedJSON" ];
  } ''
    python - <<'PY'
    import json, os, tomllib
    with open(os.environ['source'], 'rb') as f:
        actual = tomllib.load(f)
    with open(os.environ['expectedJSONPath']) as f:
        expected = json.load(f)
    assert actual == expected, (actual, expected)
    PY
    touch $out
  '';
in {
  auspex-empty = check "empty" ((evaluate { }).xdg.configFile == { });
  auspex-full = compareToml "full" full.xdg.configFile."auspex/config.toml".source
    (builtins.fromTOML (builtins.readFile ./expected.toml));
  auspex-freeform = compareToml "freeform" unknown.xdg.configFile."auspex/config.toml".source { future.answer = 42; };
  auspex-types = check "types" (!wrong.success);
  auspex-stylix = compareToml "stylix" stylix.xdg.configFile."auspex/theme.toml".source
    (lib.genAttrs slots (_: "#123abc"));
  auspex-theme-path = compareToml "theme-path" stylix.xdg.configFile."auspex/config.toml".source
    { theme.file = "/home/test/.config/auspex/theme.toml"; };
  auspex-no-stylix = check "no-stylix" (
    !(builtins.tryEval (builtins.deepSeq (checked absent).assertions true)).success
    && builtins.any (a: !a.assertion && lib.hasInfix "requires Stylix" a.message) absent.assertions
    && absent.xdg.configFile == { });
  auspex-revisions = check "revisions" (revisionOK revisions.module
    && builtins.all (self: revision (revisions.mkConfigurationRevisionModule self) { } { }
      == (self.rev or self.dirtyRev or null)) [ { rev = "abc"; } { dirtyRev = "dirty"; } { } ]
    && revision (revisions.mkConfigurationRevisionModule { rev = "abc"; }) { }
      { system.configurationRevision = "consumer"; } == "consumer");
  auspex-package = pkgs.runCommand "auspex-package" { nativeBuildInputs = [ package ]; } ''
    test '${package.meta.mainProgram}' = auspex
    test "$(command -v auspex)" = '${package}/bin/auspex'
    test -x '${package}/bin/auspex'
    touch $out
  '';
}
