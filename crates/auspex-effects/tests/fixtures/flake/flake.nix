# A fake Clan fleet for tests/nix_fixture.rs: no inputs, so evaluating it needs no network
# and no nixpkgs. Each "toplevel" is a bare derivation that is instantiated (its .drv
# written to the store) but never built: its builder does not exist.
#
# The shapes are the ones auspex reads:
# - clanInternals.inventoryClass.inventory.machines, as Clan lays it out;
# - nixosConfigurations.<name>.config.system.build.toplevel;
# - darwinConfigurations.<name>.system (nix-darwin's alias for config.system.build.toplevel).
#
# `broken` fails to evaluate (a throw), and must fail alone.
{
  outputs = _: let
    toplevel = name: system:
      derivation {
        inherit name system;
        builder = "/auspex-fixture-never-built";
      };
  in {
    clanInternals.inventoryClass.inventory.machines = {
      terra = {
        deploy.targetHost = "root@192.0.2.10";
        tags = ["server" "k8s"];
      };
      luna.deploy = {
        targetHost = "root@192.0.2.11:2222";
        buildHost = "localhost";
      };
      genetor = {
        machineClass = "darwin";
        deploy.targetHost = "admin@192.0.2.20";
      };
      broken = {};
    };

    nixosConfigurations = {
      terra.config.system.build.toplevel = toplevel "nixos-system-terra" "x86_64-linux";
      luna.config.system.build.toplevel = toplevel "nixos-system-luna" "x86_64-linux";
      broken.config.system.build.toplevel = throw "broken fails to evaluate, on purpose";
    };

    darwinConfigurations.genetor.system = toplevel "darwin-system-genetor" "aarch64-darwin";
  };
}
