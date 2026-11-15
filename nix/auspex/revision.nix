let
  mkConfigurationRevisionModule = self: { lib, ... }: {
    _file = toString ./revision.nix;
    config.system.configurationRevision = lib.mkDefault (self.rev or self.dirtyRev or null);
  };
in {
  inherit mkConfigurationRevisionModule;
  # Avoid a formal `self` argument: the module loader's missing-argument error hides
  # our diagnostic. Support both specialArgs and the usual _module.args route.
  module = args@{ config, ... }:
    mkConfigurationRevisionModule (args.self or (config._module.args.self or
      (throw "auspex configurationRevision requires the consumer's self module argument; pass specialArgs.self or import auspex.lib.mkConfigurationRevisionModule self instead."))) args;
}
