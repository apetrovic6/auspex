{ pkgs, package, exports }:
let
  inherit (pkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;
  stub = { ... }: {
    options = {
      home.packages = lib.mkOption { type = lib.types.listOf lib.types.package; default = [ ]; };
      home.homeDirectory = lib.mkOption { type = lib.types.str; default = "/home/test"; };
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
      { programs.auspex.enable = lib.mkDefault true; } extra ];
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
  unsignedPaths = map (name: [ "timeouts" name ]) [ "ssh_connect" "probe" "eval" "settle" ]
    ++ [ [ "prometheus" "stale_after" ] ];
  positivePaths = [ [ "eval" "workers" ] [ "eval" "max_memory_mb" ] ];
  numberOK = positive: path:
    let
      type = lib.foldl' (type: name: (type.getSubOptions [ ]).${name}.type) settingsType path;
      probe = value: lib.getAttrFromPath path (evaluate {
        programs.auspex.settings = lib.setAttrByPath path value;
      }).config.programs.auspex.settings;
      rejected = value: !(builtins.tryEval (builtins.deepSeq (probe value) true)).success;
    in probe 1 == 1 && !type.check "ten" && rejected "ten"
      && !type.check (-1) && rejected (-1)
      && (if positive then !type.check 0 && rejected 0 else probe 0 == 0);
  slots = map (s: "base${s}") [ "00" "01" "02" "03" "04" "05" "06" "07" "08" "09" "0A" "0B" "0C" "0D" "0E" "0F" ];
  palette = builtins.listToAttrs (lib.imap0 (index: slot: {
    name = slot;
    value = lib.fixedWidthString 6 "0" (toString index);
  }) slots);
  styled = colors: extra: checked (evaluate ({
    lib.stylix.colors = colors;
    programs.auspex.stylix.enable = true;
  } // extra)).config;
  stylix = styled palette { };
  prefixed = styled (lib.mapAttrs (_: color: "#${color}") palette) { };
  withTheme = file: styled palette {
    programs.auspex = { stylix.enable = true; settings.theme.file = file; };
  };
  customTheme = withTheme "/mine.toml";
  tildeTheme = withTheme "~/.config/auspex/theme.toml";
  absoluteTheme = withTheme "/home/test/.config/auspex/theme.toml";
  absent = (evaluate { programs.auspex.stylix.enable = true; }).config;
  partialPalettes = map (slot: (evaluate {
    programs.auspex.stylix.enable = true;
    lib.stylix.colors = builtins.removeAttrs palette [ slot ];
  }).config) slots;
  paletteRejected = cfg:
    !(builtins.tryEval (builtins.deepSeq (checked cfg).assertions true)).success
    && builtins.any (a: !a.assertion && lib.hasInfix "requires Stylix" a.message) cfg.assertions
    && cfg.xdg.configFile == { };
  revisionWithArgs = module: specialArgs: override: (lib.evalModules {
    inherit specialArgs;
    modules = [ {
      options.system.configurationRevision = lib.mkOption { type = lib.types.nullOr lib.types.str; };
    } module override ];
  }).config.system.configurationRevision;
  revision = module: self: override: revisionWithArgs module { inherit self; } override;
  revisionOK = module: builtins.all (x: x) [
    (revision module { rev = "abc"; dirtyRev = "dirty"; } { } == "abc")
    (revision module { dirtyRev = "dirty"; } { } == "dirty")
    (revision module { } { } == null)
    (revision module { rev = "abc"; } { system.configurationRevision = "consumer"; } == "consumer")
    (revisionWithArgs module { } { _module.args.self.rev = "module-argument"; } == "module-argument")
    (!(builtins.tryEval (builtins.deepSeq (revisionWithArgs module { } { }) true)).success)
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
  exportedPackage = exports.packages.${system}.auspex;
  disabled = (evaluate {
    programs.auspex = { enable = false; stylix.enable = true; settings.flake = "/ignored"; };
  }).config;
  installedPackages = map (p: p.drvPath) full.config.home.packages;
  defaultPackage = exports.packages.${system}.default;
  # Another system's package set can force source-patching IFD on that architecture.
  # Keep CI checks native to the package set supplied by the caller.
  overlayPackage = (exports.overlays.default pkgs pkgs).auspex;
in {
  auspex-empty = check "empty" ((evaluate { }).config.xdg.configFile == { });
  auspex-full = compareToml "full" fullSource expected;
  auspex-freeform = compareToml "freeform" unknown.xdg.configFile."auspex/config.toml".source { future.answer = 42; };
  # Positive evaluation and the declared type guard prevent an unrelated throw from passing.
  auspex-types = check "types" (builtins.all (numberOK false) unsignedPaths
    && builtins.all (numberOK true) positivePaths);
  auspex-stylix = compareToml "stylix" [
    stylix.xdg.configFile."auspex/theme.toml".source
    prefixed.xdg.configFile."auspex/theme.toml".source
  ] (lib.mapAttrs (_: color: "#${color}") palette);
  auspex-theme-path = pkgs.linkFarm "auspex-theme-path" [
    {
      name = "default";
      path = compareToml "theme-default" stylix.xdg.configFile."auspex/config.toml".source
        { theme.file = "/home/test/.config/auspex/theme.toml"; };
    }
    {
      name = "tilde";
      path = compareToml "theme-tilde" tildeTheme.xdg.configFile."auspex/config.toml".source
        { theme.file = "~/.config/auspex/theme.toml"; };
    }
    {
      name = "selected-palettes";
      path = compareToml "theme-selected" [
        tildeTheme.xdg.configFile."auspex/theme.toml".source
        absoluteTheme.xdg.configFile."auspex/theme.toml".source
      ] (lib.mapAttrs (_: color: "#${color}") palette);
    }
    {
      name = "override";
      path = compareToml "theme-override"
        (assert !builtins.hasAttr "auspex/theme.toml" customTheme.xdg.configFile;
          customTheme.xdg.configFile."auspex/config.toml".source) { theme.file = "/mine.toml"; };
    }
  ];
  auspex-no-stylix = check "no-stylix" (paletteRejected absent && builtins.all paletteRejected partialPalettes);
  auspex-revisions = check "revisions" (revisionOK exports.nixosModules.configurationRevision
    && revisionOK exports.darwinModules.configurationRevision
    && builtins.all (self: revision (exports.lib.mkConfigurationRevisionModule self) { } { }
      == (self.rev or self.dirtyRev or null)) [ { rev = "abc"; dirtyRev = "dirty"; } { dirtyRev = "dirty"; } { } ]
    && revision (exports.lib.mkConfigurationRevisionModule { rev = "abc"; }) { }
      { system.configurationRevision = "consumer"; } == "consumer");
  auspex-package = assert lib.assertMsg (defaultPackage.drvPath == exportedPackage.drvPath
    && overlayPackage.drvPath == exportedPackage.drvPath)
    "auspex-package: default and overlay must export their system's auspex";
    assert lib.assertMsg (installedPackages == [ exportedPackage.drvPath ] && disabled.home.packages == [ ])
      "auspex-package: home-manager must install exactly auspex only when enabled";
    pkgs.runCommand "auspex-package" { nativeBuildInputs = [ package ]; } ''
      test '${package.meta.mainProgram}' = auspex
      test '${package.CARGO_PROFILE}' = release
      test "$(command -v auspex)" = '${package}/bin/auspex'
      test -x '${package}/bin/auspex'
      test "$(find '${package}' -mindepth 1 -printf '%P\n' | sort)" = "$(printf 'bin\nbin/auspex')"
      touch $out
    '';
}
