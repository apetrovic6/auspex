{ exports }:
{
  auspex = context: import ./package.nix context;
  auspex-checks = context:
    let
      checks = import ./checks.nix {
        inherit (context) pkgs;
        inherit exports;
        package = import ./package.nix context;
      };
    in context.pkgs.linkFarm "auspex-checks"
      (context.pkgs.lib.mapAttrsToList (name: path: { inherit name path; }) checks);
}
