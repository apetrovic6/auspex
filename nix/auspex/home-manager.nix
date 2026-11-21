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
  # Except inside `rolling`: a tag with no keys is the rule "these machines update one at a time",
  # with the defaults, and pruning it away (as an empty section is) silently made its machines
  # free, updated `update.parallel` at a time. It is written as `[rolling.<tag>]`.
  pruned = prune cfg.settings
    // lib.optionalAttrs (cfg.settings.rolling != { }) {
      rolling = lib.mapAttrs (_: prune) cfg.settings.rolling;
    };
  # A path value is written as its string, never copied into the store: `toString`, not
  # interpolation of the path. `~/` is the home directory, as for `theme.file`.
  flakePath = toString pruned.flake;
  # The kubeconfig is a path on this machine, written as its string and expanded exactly as `flake`
  # is (`~/` is the home directory), for the same reason: a Nix path value would be copied into
  # the store, and what `kubectl` would then read is a frozen copy of a credentials file.
  hasKubeconfig = (pruned.kubernetes or { }) ? kubeconfig;
  kubeconfigPath = if hasKubeconfig then toString pruned.kubernetes.kubeconfig else "";
  # The forge token is a path on this machine too, and worse to copy than a kubeconfig: a Nix path
  # value would put the token in the world-readable store. Written as its string and expanded as
  # `flake` is; one that is already a store path is refused below.
  hasTokenFile = (pruned.forge or { }) ? token_file;
  tokenFilePath = if hasTokenFile then toString pruned.forge.token_file else "";
  expandHome = path: if lib.hasPrefix "~/" path
    then "${config.home.homeDirectory}/${lib.removePrefix "~/" path}"
    else path;
  settings = pruned
    // lib.optionalAttrs (pruned ? flake) {
      flake = expandHome flakePath;
    }
    // lib.optionalAttrs hasKubeconfig {
      kubernetes = pruned.kubernetes // { kubeconfig = expandHome kubeconfigPath; };
    }
    // lib.optionalAttrs hasTokenFile {
      forge = pruned.forge // { token_file = expandHome tokenFilePath; };
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
            public = optional types.bool "Whether the machine faces the internet (default false). Updating a public machine asks for its name to be typed instead of y.";
            frozen = optional types.bool "Freeze the machine (default false): `u` refuses it and `U` skips it. Declarative: `f` in the TUI cannot unfreeze it, so remove it here (and switch) to let the machine be updated again. Freezes made with `f` are not kept in this file but in auspex's state file.";
          });
        };
        rolling = mkOption {
          default = { };
          description = "Rolling tags for `U` (update every machine with an update pending), keyed by tag name. Machines that share any of these tags never update at the same time: one after another, in name order, however the tags overlap.";
          type = types.attrsOf (table {
            kubernetes = optional types.bool "The tag's machines are nodes of a Kubernetes cluster (default false): before the roll starts, refuse it if any node is NotReady or cordoned, and between machines wait until the updated machine's node is Ready. Needs a kubeconfig (kubernetes.kubeconfig).";
            drain = optional types.bool "Cordon and drain the node before its machine is updated and uncordon it after (default false: a NixOS switch does not reboot the node, and RKE2's containers survive a restart of the service). A failed or timed-out drain stops the roll and leaves the node uncordoned.";
            drain_timeout = timeout "kubectl drain's --timeout, in seconds (default 600).";
            etcd = optional types.bool "The tag's machines are the control plane of an RKE2 cluster (default false; implies kubernetes): before the roll and before every machine, refuse unless every etcd member (a node labelled node-role.kubernetes.io/etcd) has a Ready etcd pod in kube-system, and wait for the updated machine's etcd pod to be Ready again. Anything that cannot be read counts as unhealthy, and an unhealthy etcd stops the tag's roll. Leave it off for a cluster with no etcd pods (k3s with sqlite, managed, external etcd). Verify RKE2's etcd readinessProbe on your cluster before relying on it (see the auspex README).";
          });
        };
        update = section {
          parallel = optional (types.ints.between 1 16) "How many machines with no rolling tag `U` updates at once (default 2): clan builds locally and copies each closure.";
          ready_timeout = timeout "How long `U` waits for an updated machine to be healthy (and its node Ready) before it gives the machine up, in seconds (default 600). `update.poll_interval` must not be more than it.";
          poll_interval = optional (types.ints.between 1 60) "How often `U` looks again while it waits for an updated machine (and its node) to be healthy, in seconds (default 5). Must not be more than ready_timeout (checked here when both are set); left unset it is 5, or ready_timeout when that is shorter.";
        };
        kubernetes = section {
          kubeconfig = optional (types.either types.str types.path) "The kubeconfig `U` reads node state and drains with (kubectl --kubeconfig); a leading ~/ is your home directory. Default: \$KUBECONFIG, then ~/.kube/config. A path value is written as its string, not copied into the store; a path inside the Nix store is refused as a stale copy of a credentials file.";
        };
        forge = section {
          url = optional (types.strMatching "https://[^@/?#]+([/?#].*)?") "The forge whose combined commit status for the fleet's current commit is shown as a CI badge (status bar and `fleet_ci` in --json); https only, without credentials. Omit it to switch the badge off: nothing is then read or asked. Forgejo's API: GET /api/v1/repos/{owner}/{repo}/commits/{sha}/status.";
          repo = optional (types.strMatching "[A-Za-z0-9._-]+/[A-Za-z0-9._-]+") "The repository on the forge, owner/name. Default: read from the fleet checkout's `origin` remote, which must then be on the same host as forge.url (otherwise the badge says `unknown: set forge.repo`).";
          token_file = optional (types.either types.str types.path) "A file holding a read-only API token for the forge, sent as `Authorization: token …`; a leading ~/ is your home directory. Never the token itself: it would be written to config.toml. auspex refuses a file that group or others can access (use mode 0600) or that you do not own, and strips one trailing newline. Without it the forge is asked anonymously (public repositories). A path value is written as its string, not copied into the store; a path inside the Nix store is refused, since the store is world-readable.";
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
      }
      {
        assertion = (settings.update.poll_interval or 0) <= (settings.update.ready_timeout or 600);
        message = "programs.auspex.settings.update.poll_interval must not be more than update.ready_timeout (default 600): auspex refuses the file.";
      }
      {
        assertion = !hasKubeconfig || !lib.hasPrefix "${builtins.storeDir}/" kubeconfigPath;
        message = "programs.auspex.settings.kubernetes.kubeconfig is a ${builtins.storeDir} path, a frozen copy of a credentials file (a relative path literal in a flake is evaluated from the flake's store copy). Use an absolute path or a string such as \"~/.kube/config\".";
      }
      {
        assertion = !hasTokenFile || !lib.hasPrefix "${builtins.storeDir}/" tokenFilePath;
        message = "programs.auspex.settings.forge.token_file is a ${builtins.storeDir} path, which is world-readable (a relative path literal in a flake is evaluated from the flake's store copy, and so is a token file added to the flake). Use a path outside the store that only you can read, such as \"~/.config/auspex/forge-token\" or a secrets manager's runtime path.";
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
