{
  module = { lib, self, ... }: {
    system.configurationRevision = lib.mkDefault (self.rev or self.dirtyRev or null);
  };
  mkConfigurationRevisionModule = self: { lib, ... }: {
    system.configurationRevision = lib.mkDefault (self.rev or self.dirtyRev or null);
  };
}
