{ packages }:
{ config, lib, pkgs, ... }:
let
  inherit (lib) mkOption types;
  cfg = config.programs.auspex;
  optional = type: description: mkOption {
    type = types.nullOr type;
    default = null;
    inherit description;
  };
  toml = pkgs.formats.toml { };
  table = options: types.submodule {
    freeformType = toml.type;
    inherit options;
  };
  section = options: mkOption { type = table options; default = { }; };
  seconds = description: optional types.ints.unsigned description;
  # auspex refuses a timeout of 0 or over a day (the shell's config loader, `MAX_TIMEOUT`); the
  # module refuses the same, so a home-manager switch fails rather than auspex.
  timeout = description: optional (types.ints.between 1 86400) description;
  colors = config.lib.stylix.colors or null;
  slots = map (n: "base${n}") [
    "00" "01" "02" "03" "04" "05" "06" "07"
    "08" "09" "0A" "0B" "0C" "0D" "0E" "0F"
  ];
  hasColors = colors != null && builtins.all (slot: builtins.hasAttr slot colors) slots;
  theme = toml.generate "auspex-theme.toml"
    (lib.genAttrs slots (slot: "#${lib.removePrefix "#" colors.${slot}}"));
  # Null is an option-level sentinel only: omit it and empty sections from TOML.
  prune = value: if builtins.isAttrs value then
    lib.filterAttrs (_: v: v != null && v != { }) (lib.mapAttrs (_: prune) value)
    else value;
  pruned = prune cfg.settings;
  # A path value is written as its string, never copied into the store: `toString`, not
  # interpolation of the path. `~/` is the home directory, as for `theme.file`.
  flakePath = toString pruned.flake;
  settings = pruned // lib.optionalAttrs (pruned ? flake) {
    flake = if lib.hasPrefix "~/" flakePath
      then "${config.home.homeDirectory}/${lib.removePrefix "~/" flakePath}"
      else flakePath;
  };
  themePath = "${config.xdg.configHome}/auspex/theme.toml";
  selectedThemePath =
    if cfg.settings.theme.file != null && lib.hasPrefix "~/" cfg.settings.theme.file
    then "${config.home.homeDirectory}/${lib.removePrefix "~/" cfg.settings.theme.file}"
    else cfg.settings.theme.file;
in {
  _file = toString ./home-manager.nix;
  options.programs.auspex = {
    enable = lib.mkEnableOption "auspex fleet terminal UI";
    package = lib.mkPackageOption packages.${pkgs.stdenv.hostPlatform.system} "auspex" { };
    settings = mkOption {
      description = "auspex config.toml settings; unknown keys pass through for newer versions. Unset optional values and empty tables are omitted recursively, including explicitly supplied empty tables.";
      default = { };
      type = table {
        flake = optional (types.either types.str types.path) "Fleet flake path; a leading ~/ is your home directory. A path value is written as its string, not copied into the store; a path inside the Nix store (such as a relative ./path in a flake) is refused as a stale copy. --flake and CLAN_DIR take precedence.";
        ssh = section {
          control_persist = optional (types.ints.between 60 3600) "Idle authenticated master lifetime in seconds (default 600); reusable by processes running as the operator.";
        };
        timeouts = section {
          ssh_connect = timeout "SSH connection timeout in seconds.";
          ssh_approval = optional (types.ints.between 5 600) "SSH authentication/agent approval allowance in seconds (default 60), separate from the probe.";
          probe = timeout "Probe script timeout in seconds, after authentication.";
          eval = timeout "Per-machine evaluation timeout in seconds.";
          settle = timeout "Headless fleet settling timeout in seconds.";
          diff = timeout "Package preview timeout in seconds (default 120): how long one machine's two closure queries may take. Not cut short by the settle timeout; the preview is failed when it passes, and the scan is not.";
        };
        diff = section {
          enabled = optional types.bool "Whether a machine with an update pending gets a package preview (default true). When false no closure is queried and package_diff stays null.";
        };
        eval = section {
          workers = optional types.ints.positive "Number of nix-eval-jobs workers.";
          max_memory_mb = optional types.ints.positive "Memory limit per evaluation worker in MiB.";
        };
        prometheus = section {
          url = optional types.str "Prometheus URL; omit to use SSH only.";
          stale_after = seconds "Sample age in seconds before falling back to SSH.";
        };
        cache = section {
          url = optional types.str "Binary cache URL for the cached badge.";
        };
        machines = mkOption {
          default = { };
          description = "Per-machine overrides keyed by inventory machine name.";
          type = types.attrsOf (table {
            target = optional types.str "SSH target overriding inventory deploy.targetHost.";
            prometheus_instance = optional types.str "Prometheus instance label; defaults to <name>:9100.";
          });
        };
        keymap = mkOption {
          default = { };
          type = types.attrsOf (types.either types.str (types.listOf types.str));
          description = "Action to key or list of keys; unlisted actions retain their defaults.";
        };
        theme = section {
          file = optional types.str "Base16 theme TOML path; omit to use terminal ANSI colours.";
        };
      };
    };
    stylix.enable = lib.mkEnableOption "a Base16 theme from Stylix colours";
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      home.packages = [ cfg.package ];
      assertions = [{
        assertion = !cfg.stylix.enable || hasColors;
        message = "programs.auspex.stylix.enable requires Stylix with config.lib.stylix.colors (base00 through base0F).";
      }
      {
        assertion = !(settings ? flake) || !lib.hasPrefix "${builtins.storeDir}/" settings.flake;
        message = "programs.auspex.settings.flake is a ${builtins.storeDir} path, a frozen copy of the fleet (a relative path literal in a flake is evaluated from the flake's store copy). Use an absolute path or a string such as \"~/clan/fleet\".";
      }];
      xdg.configFile = lib.mkIf (settings != { }) {
        "auspex/config.toml".source = toml.generate "auspex-config.toml" settings;
      };
    }
    (lib.mkIf (cfg.stylix.enable && hasColors) {
      xdg.configFile."auspex/theme.toml" = lib.mkIf (selectedThemePath == themePath) {
        source = theme;
      };
      programs.auspex.settings.theme.file = lib.mkDefault themePath;
    })
  ]);
}
