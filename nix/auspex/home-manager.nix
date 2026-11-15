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
  settings = prune cfg.settings;
  themePath = "${config.xdg.configHome}/auspex/theme.toml";
in {
  _file = toString ./home-manager.nix;
  options.programs.auspex = {
    enable = lib.mkEnableOption "auspex fleet terminal UI";
    package = lib.mkPackageOption packages.${pkgs.stdenv.hostPlatform.system} "auspex" { };
    settings = mkOption {
      description = "auspex config.toml settings; unknown keys pass through for newer versions. Unset optional values and empty tables are omitted recursively, including explicitly supplied empty tables.";
      default = { };
      type = table {
        flake = optional types.str "Fleet flake path; --flake and CLAN_DIR take precedence.";
        timeouts = section {
          ssh_connect = seconds "SSH connection timeout in seconds.";
          probe = seconds "Whole-machine probe timeout in seconds.";
          eval = seconds "Per-machine evaluation timeout in seconds.";
          settle = seconds "Headless fleet settling timeout in seconds.";
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
      }];
      xdg.configFile = lib.mkIf (settings != { }) {
        "auspex/config.toml".source = toml.generate "auspex-config.toml" settings;
      };
    }
    (lib.mkIf (cfg.stylix.enable && hasColors) {
      xdg.configFile."auspex/theme.toml" = lib.mkIf (cfg.settings.theme.file == themePath) {
        source = theme;
      };
      programs.auspex.settings.theme.file = lib.mkDefault themePath;
    })
  ]);
}
