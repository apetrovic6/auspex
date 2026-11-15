# The versions that must agree across Cargo, Nix, Gradle and just, parsed once.
#
# Deliberately *not* a devenv module, so it is imported with `import ./versions.nix` by the
# modules that need a pin rather than listed in devenv.nix's `imports`. Cargo and Gradle
# cannot read this file, so `cargo xtask check-versions` asserts their literals match it --
# see versions.toml itself for why each pin is load-bearing.
builtins.fromTOML (builtins.readFile ../versions.toml)
