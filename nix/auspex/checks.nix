{ pkgs, package, exports }:
let
  inherit (pkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;
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
  evaluate = extra: lib.evalModules {
    specialArgs = { inherit pkgs; };
    modules = [ stub exports.homeManagerModules.auspex
      { programs.auspex.enable = true; } extra ];
  };
  checked = cfg: if builtins.all (a: a.assertion) cfg.assertions then cfg
    else throw (lib.concatStringsSep "\n" (map (a: a.message) (builtins.filter (a: !a.assertion) cfg.assertions)));
  check = name: condition: pkgs.runCommand "auspex-${name}" { } ''
    ${if condition then "" else "echo 'auspex-${name} failed' >&2; exit 1"}
    touch $out
  '';
  expected = builtins.fromTOML (builtins.readFile ./expected.toml);
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
  settingsType = full.options.programs.auspex.settings.type;
  # attrsOf declares dynamic map entries (machine names and action names), not fixed keys.
  # Submodule leaves must be actual options, never the TOML freeform fallback.
  declares = type: value:
    if !builtins.isAttrs value then true
    else if type.name == "attrsOf" then
      builtins.all (declares type.nestedTypes.elemType) (builtins.attrValues value)
    else let options = type.getSubOptions [ ]; in
      builtins.all (name: builtins.hasAttr name options
        && declares options.${name}.type value.${name}) (builtins.attrNames value);
  fullSource = assert lib.assertMsg (declares settingsType expected)
    "auspex-full: every fixture leaf must be covered by a declared settings option";
    full.config.xdg.configFile."auspex/config.toml".source;
  unknown = (evaluate { programs.auspex.settings.future.answer = 42; }).config;
  probe = value: (evaluate { programs.auspex.settings.timeouts.probe = value; }).config.programs.auspex.settings.timeouts.probe;
  rejected = value: !(builtins.tryEval (builtins.deepSeq (probe value) true)).success;
  probeType = ((settingsType.getSubOptions [ ]).timeouts.type.getSubOptions [ ]).probe.type;
  slots = map (s: "base${s}") [ "00" "01" "02" "03" "04" "05" "06" "07" "08" "09" "0A" "0B" "0C" "0D" "0E" "0F" ];
  styled = color: extra: checked (evaluate ({
    lib.stylix.colors = lib.genAttrs slots (_: color);
    programs.auspex.stylix.enable = true;
  } // extra)).config;
  stylix = styled "123abc" { };
  prefixed = styled "#123abc" { };
  customTheme = styled "123abc" {
    programs.auspex = { stylix.enable = true; settings.theme.file = "/mine.toml"; };
  };
  absent = (evaluate { programs.auspex.stylix.enable = true; }).config;
  partial = (evaluate {
    programs.auspex.stylix.enable = true;
    lib.stylix.colors.base00 = "123abc";
  }).config;
  paletteRejected = cfg:
    !(builtins.tryEval (builtins.deepSeq (checked cfg).assertions true)).success
    && builtins.any (a: !a.assertion && lib.hasInfix "requires Stylix" a.message) cfg.assertions
    && cfg.xdg.configFile == { };
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
  compareToml = name: sources: expectedValue: pkgs.runCommand "auspex-${name}" {
    nativeBuildInputs = [ pkgs.python3 ];
    sourceFiles = lib.concatMapStringsSep " " toString (lib.toList sources);
    expectedJSON = builtins.toJSON expectedValue;
    passAsFile = [ "expectedJSON" ];
  } ''
    python - <<'PY'
    import json, os, tomllib
    with open(os.environ['expectedJSONPath']) as f:
        expected = json.load(f)
    for path in os.environ['sourceFiles'].split():
        with open(path, 'rb') as f:
            actual = tomllib.load(f)
        assert actual == expected, (path, actual, expected)
    PY
    touch $out
  '';
  defaultPackage = exports.packages.${system}.default;
  overlayPackage = (exports.overlays.default pkgs pkgs).auspex;
in {
  auspex-empty = check "empty" ((evaluate { }).config.xdg.configFile == { });
  auspex-full = compareToml "full" fullSource expected;
  auspex-freeform = compareToml "freeform" unknown.xdg.configFile."auspex/config.toml".source { future.answer = 42; };
  # Positive evaluation and the declared type guard prevent an unrelated throw from passing.
  auspex-types = check "types" (probe 10 == 10 && probe 0 == 0
    && !probeType.check "ten" && !probeType.check (-1)
    && rejected "ten" && rejected (-1));
  auspex-stylix = compareToml "stylix" [
    stylix.xdg.configFile."auspex/theme.toml".source
    prefixed.xdg.configFile."auspex/theme.toml".source
  ] (lib.genAttrs slots (_: "#123abc"));
  auspex-theme-path = pkgs.linkFarm "auspex-theme-path" [
    {
      name = "default";
      path = compareToml "theme-default" stylix.xdg.configFile."auspex/config.toml".source
        { theme.file = "/home/test/.config/auspex/theme.toml"; };
    }
    {
      name = "override";
      path = compareToml "theme-override"
        (assert !builtins.hasAttr "auspex/theme.toml" customTheme.xdg.configFile;
          customTheme.xdg.configFile."auspex/config.toml".source) { theme.file = "/mine.toml"; };
    }
  ];
  auspex-no-stylix = check "no-stylix" (paletteRejected absent && paletteRejected partial);
  auspex-revisions = check "revisions" (revisionOK exports.nixosModules.configurationRevision
    && revisionOK exports.darwinModules.configurationRevision
    && builtins.all (self: revision (exports.lib.mkConfigurationRevisionModule self) { } { }
      == (self.rev or self.dirtyRev or null)) [ { rev = "abc"; dirtyRev = "dirty"; } { dirtyRev = "dirty"; } { } ]
    && revision (exports.lib.mkConfigurationRevisionModule { rev = "abc"; }) { }
      { system.configurationRevision = "consumer"; } == "consumer");
  auspex-package = assert lib.assertMsg (defaultPackage.drvPath == package.drvPath
    && overlayPackage.drvPath == package.drvPath) "auspex-package: default and overlay must export auspex";
    pkgs.runCommand "auspex-package" { nativeBuildInputs = [ defaultPackage overlayPackage ]; } ''
      test '${package.meta.mainProgram}' = auspex
      test '${package.CARGO_PROFILE}' = release
      test "$(command -v auspex)" = '${package}/bin/auspex'
      test -x '${package}/bin/auspex'
      test "$(find '${package}' -mindepth 1 -printf '%P\n' | sort)" = "$(printf 'bin\nbin/auspex')"
      touch $out
    '';
}
