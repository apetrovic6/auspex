# auspex

A Crux app: one core holding the logic, several shells drawing it.

```
core/           the portable application logic — no I/O, no framework
shells/tui      terminal (ratatui)
```

The core never performs an effect; it asks for one, and a shell fulfils it. That is what
lets every shell share one set of decisions without any of them disagreeing.

```sh
just apps/auspex/dev              # format, lint, build and test every crate here
just apps/auspex/shells/tui/run
```

Only the shells listed above were created. `just add-shell auspex <tui|web|desktop>` adds another; nothing else needs editing.

## Installing

Run directly with `nix run git+https://forge.noosphere.uk/manjo/auspex`, or add
auspex to your fleet flake:

```nix
inputs.auspex.url = "git+https://forge.noosphere.uk/manjo/auspex";
```

Without `inputs.auspex.inputs.nixpkgs.follows = "nixpkgs"`, auspex uses its pinned
devenv-nixpkgs (import-from-derivation evaluation and potentially a second glibc).
Following your nixpkgs avoids that pin's evaluation cost but changes the derivation
and can require a full rebuild instead of a cache hit.
A `path:` reference to a Git checkout does not work with the template's source
fileset; use `git+file:` or a plain path instead.

Import its Home Manager module (no Home Manager input is required by auspex):

```nix
{
  imports = [ inputs.auspex.homeManagerModules.auspex ];
  programs.auspex = {
    enable = true;
    settings.flake = "/home/me/clan/fleet";
    settings.timeouts.probe = 10;
    # stylix.enable = true; # optional, requires Stylix colours
  };
}
```

The module installs `auspex` and writes only configured settings to
`~/.config/auspex/config.toml`. With no settings, it leaves that file unmanaged.
`stylix.enable` selects a generated Base16 `theme.toml`, unless you set another
`settings.theme.file` (then no unused theme is written); without a theme, auspex uses
the terminal's ANSI colours. A hand-written config works without Home Manager.
A leading `~/` in the selected theme path uses Home Manager's home directory.
The package is also available as `packages.<system>.auspex` (and `default`);
`overlays.default` adds `pkgs.auspex`.

Stamp deployed machines with the **fleet's** revision, not auspex's:

```nix
# In a NixOS module:
imports = [ inputs.auspex.nixosModules.configurationRevision ];
# In a nix-darwin module:
imports = [ inputs.auspex.darwinModules.configurationRevision ];
```

Both imports expect the fleet's `self` in module arguments (Clan provides it;
otherwise set `specialArgs = { inherit self; };`). Alternatively import
`inputs.auspex.lib.mkConfigurationRevisionModule self` to capture it explicitly.
An explicit `system.configurationRevision` overrides the module's default.

## Two things this archetype did not create

**Native platform projects.** A Gradle or Xcode project is not a Cargo crate, and is only
wanted sometimes, so it is a separate command:

```sh
just add platform-android auspex    # or apple
```

It derives from an app that already has that platform, renaming every identifier as it
copies, and falls back to the skeleton in `templates/platform-<platform>` when no app does.
The Justfile here discovers `platforms/*` at runtime, so nothing else needs editing.

**A way to reach a server.** The core's only effect is `Render`, so this app's state is
entirely local. When it needs the outside world, add an `Effect` variant whose operation is
in this app's own vocabulary — `Increment { by }`, not `POST /v1/increment` — and let each
shell fulfil it with a client generated from the contract. `apps/counter` is the worked
example: the core stays domain-level, `apps/counter/client` maps those operations onto
Connect calls for the Rust shells, and the Kotlin and Swift shells do the same with their
own generated clients.

Both are additive. Neither changes anything already here.
