# The template as a flake: the consumption path for products.
#
# Products take this repository as a devenv input with `flake = true` and get both the
# pipeline library (`lib.mkPipeline`, `lib.mkToolchain`, `lib.discover`) and — from PR 2 —
# the CLI package. Every entry in `lib` is a function the product calls with its OWN pkgs/
# rust-overlay/craneLib, so a product's dev shell and CI always use the product's pins; the
# library never reads a lock of its own.
#
# The `packages` output here is a PROOF — it builds a pipeline stage from a plain flake with
# no devenv involved, using this flake's own inputs (flake.lock) as "whatever pkgs it is
# given". The template's real CI does not go through this flake: the forge workflows run
# `devenv build`, and devenv shell + devenv build resolve to the same toolchain store path
# via nix/toolchain.nix.
{
  description = "rust-app-template";

  inputs = {
    # The template this product was instantiated from, at the revision it was
    # created at. `nix flake lock --update-input template` writes the node, and
    # `nix/template-pin.nix` reads that node's revision back out for `just add`.
    template.url = "git+https://forge.ugalabugala.org/manjo/rust-app-template?rev=9a40690b725310f872572c0b9450c35da9615e1f";
    # The template's own inputs are this product's, so pinning the origin adds one
    # node rather than a second nixpkgs, crane, rust-overlay and connectrpc.
    template.inputs.nixpkgs.follows = "nixpkgs";
    template.inputs.crane.follows = "crane";
    template.inputs.rust-overlay.follows = "rust-overlay";
    template.inputs.connectrpc.follows = "connectrpc";
    nixpkgs.url = "github:cachix/devenv-nixpkgs/rolling";
    crane.url = "github:ipetkov/crane";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    connectrpc.url = "github:apetrovic6/connectrpc-rust-flake";
    connectrpc.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    { self, nixpkgs, crane, rust-overlay, connectrpc, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);

      # The library, system-independent (a function the product calls with its own pkgs).
      mkPipeline = import ./nix/pipeline/pipeline.nix { lib = nixpkgs.lib; };
      mkToolchain = import ./nix/pipeline/toolchain.nix nixpkgs.lib;
      discover = import ./nix/pipeline/discovery.nix { lib = nixpkgs.lib; };

      # One evaluation per system, shared by `packages`, `checks` and `nixosModules`: the same
      # pipeline work, and evaluating it twice would give two derivations with one name.
      results = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          versions = builtins.fromTOML (builtins.readFile ./versions.toml);
          discovery = discover ./.;
          toolchain = mkToolchain {
            inherit pkgs rust-overlay;
            channel = versions.rust.channel;
            version = "latest";
            targets = discovery.rustTargets;
          };
          craneLib = (crane.mkLib pkgs).overrideToolchain (_: toolchain);
          result = mkPipeline ({
            src = ./.;
            inherit pkgs craneLib;
            inputs = { inherit connectrpc; };
            depsBase = ./nix/deps-base;
            # Match devenv's layer names instead of the changing flake source store basename.
            pname = "rust-app-template";
            extraStages = import ./nix/auspex/stages.nix;
          } // import ./nix/auspex/platform-args.nix { inherit pkgs system; lib = nixpkgs.lib; });
          # The CLI (products run it from the pinned template input; see `just add`).
          cli = craneLib.buildPackage {
            pname = "cli";
            version = "0.1.0";
            src = ./. ;
            cargoExtraArgs = "--locked -p cli";
            doCheck = false;
          };
        in
        result // { inherit cli; });

      # What a deployment does, done the way a deployment does it: a plain `nixosSystem` that
      # imports `self.nixosModules.<svc>` from the flake OUTPUT. The VM tests import the module
      # straight from the pipeline and never touch this output, which is how a wrapper whose
      # `imports` read `pkgs` (an infinite recursion in every real system) shipped unnoticed.
      # A second product's module rides along, so "two products on one host" is proved through
      # the same consumer path. Evaluating the system is the test; nothing is built.
      consumerCheck = system: name: svc:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          lib = nixpkgs.lib;
          product = svc.product;
          other = "second-product";
          discovered = lib.findFirst (s: s.name == name) null (discover ./.).services;
          os = pkgs.nixos [
            self.nixosModules.${name}
            (results.${system}.serviceModule other discovered svc.package)
            {
              system.stateVersion = lib.trivial.release;
              services.${product}.${name} = { enable = true; dynamicUser = false; };
              services.${other}.${name} = { enable = true; dynamicUser = false; };
            }
          ];
          # A deployment that overrides `package`: the binary AND the web assets must follow it.
          # The module is the first system's, so anything it closed over instead of reading from
          # `cfg.package` would keep pointing at the template's build.
          swapped = pkgs.runCommand "swapped-${name}" { } ''
            mkdir -p $out/bin $out/share/${name}/web
            touch $out/bin/${discovered.pname} $out/share/${name}/web/index.html
          '';
          osSwapped = pkgs.nixos [
            self.nixosModules.${name}
            {
              system.stateVersion = lib.trivial.release;
              services.${product}.${name} = { enable = true; dynamicUser = false; package = swapped; };
            }
          ];
          swappedUnit = osSwapped.config.systemd.services."${product}-${name}";
          hasWeb = discovered.spa != null;
          unit = os.config.systemd.services."${product}-${name}";
          otherUnit = os.config.systemd.services."${other}-${name}";
          expect = ok: message: if ok then "" else "fail ${lib.escapeShellArg message}";
        in
        pkgs.runCommand "${name}-nixos-module" { } ''
          fail() { echo "nixosModules.${name} consumed by a plain nixosSystem: $*" >&2; exit 1; }
          ${expect (lib.hasPrefix "${svc.package}/bin/" unit.serviceConfig.ExecStart)
            "ExecStart is ${unit.serviceConfig.ExecStart}, not the template's package ${svc.package}"}
          ${expect (unit.serviceConfig.User == "${product}-${name}")
            "User is ${toString unit.serviceConfig.User}, not ${product}-${name}"}
          ${expect (otherUnit.serviceConfig.User == "${other}-${name}")
            "the second product's User is ${toString otherUnit.serviceConfig.User}"}
          ${expect (lib.hasPrefix "${swapped}/bin/" swappedUnit.serviceConfig.ExecStart)
            "with package overridden, ExecStart is ${swappedUnit.serviceConfig.ExecStart}, not under ${swapped}"}
          ${lib.optionalString hasWeb (expect
            (unit.environment.APP__WEB__DIR == "${svc.package}/share/${name}/web")
            "the web dir is ${unit.environment.APP__WEB__DIR}, not the package's ${svc.package}/share/${name}/web")}
          ${lib.optionalString hasWeb (expect
            (swappedUnit.environment.APP__WEB__DIR == "${swapped}/share/${name}/web")
            "with package overridden, the web dir is ${swappedUnit.environment.APP__WEB__DIR} — it did not follow the package (${swapped})")}
          echo "ok" > $out
        '';

    in
    {
      lib = {
        inherit mkPipeline mkToolchain discover;
        inherit (import ./nix/auspex/revision.nix) mkConfigurationRevisionModule;
      };

      packages = forAllSystems (system:
        let result = results.${system};
        in result.packages // { inherit (result) cli; default = result.packages.auspex; });

      # The service checks: `<svc>-module-vm` and `<svc>-image-vm`, one pair per discovered
      # service, derived from the service's package and image. They are in `packages` too, so
      # ci-pure builds them; `checks` is what a person or `nix flake check` asks for.
      checks = forAllSystems (system:
        nixpkgs.lib.concatMapAttrs
          (name: svc:
            nixpkgs.lib.mapAttrs'
              (kind: drv: nixpkgs.lib.nameValuePair "${name}-${kind}-vm" drv)
              (svc.vmTests or { }))
          results.${system}.services
      // nixpkgs.lib.optionalAttrs (nixpkgs.lib.hasSuffix "-linux" system)
        (nixpkgs.lib.concatMapAttrs
          (name: svc: {
            "${name}-nixos-module" = consumerCheck system name svc;
          })
          results.${system}.services)
      // import ./nix/auspex/checks.nix {
        pkgs = nixpkgs.legacyPackages.${system};
        package = self.packages.${system}.auspex;
      });

      # The NixOS modules, one per discovered service. A module is system-independent, but the
      # package it defaults to is not, so each wrapper imports the variant built for the
      # consumer's system — which is why a product's `services.<product>.<svc>` options resolve to
      # that product's own pins rather than the template's. The attribute name stays the service's
      # (`nixosModules.api`), because it is already scoped by being an input of THIS flake; what
      # the module creates on the host is scoped by the product (see nix/pipeline/module.nix).
      nixosModules = nixpkgs.lib.mapAttrs
        (name: _:
          { lib, pkgs, ... }:
          let
            # The module's text does not depend on the system, so what it IMPORTS must not
            # either: `imports` is resolved before `pkgs` exists, and an import that reads
            # `pkgs.stdenv.hostPlatform.system` is an infinite recursion in any real
            # nixosSystem ("while evaluating the module argument `pkgs'"). The first system's
            # module therefore stands for all of them, and only the package it defaults to —
            # read lazily, from `config`, where `pkgs` is available — follows the consumer.
            generic = results.${builtins.head systems}.services.${name};
            ours = results.${pkgs.stdenv.hostPlatform.system}.services.${name};
          in
          {
            imports = [ generic.module ];
            config.services.${generic.product}.${name}.package = lib.mkDefault ours.package;
          })
        (nixpkgs.lib.listToAttrs
          (map (service: nixpkgs.lib.nameValuePair service.name service) (discover ./.).services))
        // { configurationRevision = (import ./nix/auspex/revision.nix).module; };

      darwinModules.configurationRevision = (import ./nix/auspex/revision.nix).module;
      homeManagerModules.auspex = import ./nix/auspex/home-manager.nix { inherit (self) packages; };
      overlays.default = final: prev: {
        auspex = self.packages.${final.stdenv.hostPlatform.system}.auspex;
      };

      # `nix run <repo>#new -- <name> [flags]`, and the `add` half for `just add`.
      apps = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          cli = self.packages.${system}.cli;
          # The same toolchain the pipeline uses, so the CLI's rustfmt matches the
          # product's `cargo fmt` (nightly) — a stable rustfmt would format imports
          # differently and leave a product failing its own fmt gate.
          versions = builtins.fromTOML (builtins.readFile ./versions.toml);
          discovery = discover ./. ;
          toolchain = mkToolchain {
            inherit pkgs rust-overlay;
            channel = versions.rust.channel;
            version = "latest";
            targets = discovery.rustTargets;
          };
          mkApp = cmd: runtimeInputs: prelude: {
            type = "app";
            program = pkgs.lib.getExe (pkgs.writeShellApplication {
              name = "template-cli-${cmd}";
              inherit runtimeInputs;
              # Every app copies/reads the flake source (the archetype skeletons), so the
              # source path is always exported; `new` also pins the flake's rev when nix
              # gives one.
              text = prelude + ''export TEMPLATE_SOURCE="${self.outPath}"
exec cli ${cmd} "$@"'';
            });
          };
        in
        {
          # `new` shells out to `nix flake lock --update-input template` to pin the origin (and
          # previously to `devenv` to rebase the dependency snapshot), so nix, devenv, git and
          # just are on PATH; the toolchain supplies rustfmt for the rename's import re-sort.
          new = mkApp "new" [ cli pkgs.nix pkgs.devenv pkgs.git pkgs.just toolchain ]
            (pkgs.lib.optionalString (self ? rev && self.rev != null) ''export TEMPLATE_REV="${self.rev}"
'');
          # `add` formats the files it writes, so the toolchain (rustfmt) is on PATH.
          add = mkApp "add" [ cli toolchain ] "";
          add-client = mkApp "add-client" [ cli toolchain ] "";
          add-shell = mkApp "add-shell" [ cli toolchain ] "";
          cli = { type = "app"; program = pkgs.lib.getExe cli; };
        });
    };
}
